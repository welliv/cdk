//! Nostr Wallet Connect (NIP-47) wallet **service**.
//!
//! This is the side that holds the funds and answers commands. It owns the
//! Nostr relay connection, publishes the kind `13194` info event advertising
//! capabilities, listens for kind `23194` requests from the authorized client,
//! decrypts and validates them, dispatches to a [`NwcRequestHandler`], and
//! publishes the encrypted kind `23195` response.
//!
//! Security properties enforced here (monetary software — defense in depth):
//! - **Authorization**: only events authored by the single client public key
//!   issued in the connection URI are processed (enforced both by the relay
//!   subscription filter and an explicit author check).
//! - **Replay / idempotency**: each request event id is processed at most once,
//!   so relay re-delivery cannot trigger a duplicate `pay_invoice`.
//! - **Freshness**: requests carrying an expired NIP-40 `expiration` tag are
//!   dropped, and only events created after the service started are considered.
//! - **No information leak / no panics**: every handler error is mapped to a
//!   NIP-47 error response; the relay loop never aborts on a single bad request.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use nostr::prelude::nip47::{
    ErrorCode, NIP47Error, NostrWalletConnectUri, Request, RequestParams, Response, ResponseResult,
};
use nostr::prelude::{
    nip04, nip44, Event, EventBuilder, EventId, Filter, FinalizeEvent, Keys, Kind, PublicKey,
    RelayUrl, SecretKey, Tag, Timestamp,
};
use nostr_sdk::prelude::{
    Client as NostrClient, ClientNotification, SignerAuthenticator, StreamExt,
};
use tokio_util::sync::CancellationToken;

use crate::nwc::error::{Error, Result};
use crate::nwc::handler::{NwcRequestHandler, RequestContext};

/// Commands advertised in the info event and reported by `get_info`.
///
/// Order is the canonical advertisement order; it is also the set of commands
/// this service will actually dispatch — anything else returns
/// [`ErrorCode::NotImplemented`].
pub const SUPPORTED_METHODS: [&str; 6] = [
    "pay_invoice",
    "make_invoice",
    "lookup_invoice",
    "list_transactions",
    "get_balance",
    "get_info",
];

const SUPPORTED_ENCRYPTION_SCHEMES: &str = "nip44_v2 nip04";

/// Maximum number of recently-seen request event ids kept in memory for
/// fast relay-level deduplication.
const DEDUP_CAPACITY: usize = 10_000;

/// Maximum age (in seconds) of a request's `created_at` relative to service
/// receive time. Requests older than this are dropped so persistent replay
/// ledgers can safely prune entries past the window without admitting replays.
pub const MAX_REQUEST_AGE_SECS: u64 = 3_600;

/// Maximum clock skew (in seconds) allowed ahead of the service clock.
pub const MAX_FUTURE_SKEW_SECS: u64 = 300;

/// Encryption scheme negotiated for a single request/response exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encryption {
    /// NIP-44 v2 (preferred).
    Nip44,
    /// NIP-04 (legacy, still widely used by NWC clients).
    Nip04,
}

/// Configuration for an [`NwcService`].
#[derive(Debug, Clone)]
pub struct NwcServiceConfig {
    /// Keys of the wallet service (the "signer"). Its public key is the one
    /// advertised in the connection URI.
    pub service_keys: Keys,
    /// The client secret embedded in the connection URI. The public key derived
    /// from it is the only key authorized to send requests.
    pub client_secret: SecretKey,
    /// Relays the service connects to and listens on. Must be non-empty.
    pub relays: Vec<RelayUrl>,
    /// Optional lightning address advertised in the connection URI (`lud16`).
    pub lud16: Option<String>,
}

/// A NIP-47 wallet service bound to a single client connection.
#[derive(Debug, Clone)]
pub struct NwcService {
    service_keys: Keys,
    client_secret: SecretKey,
    client_pubkey: PublicKey,
    relays: Vec<RelayUrl>,
    lud16: Option<String>,
    allowed_methods: Option<Vec<String>>,
}

impl NwcService {
    /// Create a new service from configuration.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoRelays`] if no relays are configured.
    pub fn new(config: NwcServiceConfig) -> Result<Self> {
        if config.relays.is_empty() {
            return Err(Error::NoRelays);
        }

        let client_pubkey = Keys::new(config.client_secret.clone()).public_key();

        Ok(Self {
            service_keys: config.service_keys,
            client_secret: config.client_secret,
            client_pubkey,
            relays: config.relays,
            lud16: config.lud16,
            allowed_methods: None,
        })
    }

    /// Restrict the methods advertised in the kind `13194` info event for
    /// this connection. Unrecognized method names are ignored; canonical
    /// advertisement order is preserved.
    #[must_use]
    pub fn with_allowed_methods(mut self, methods: Vec<String>) -> Self {
        self.allowed_methods = Some(methods);
        self
    }

    /// Space-separated method list advertised in the kind `13194` info event.
    pub fn info_event_content(&self) -> String {
        match &self.allowed_methods {
            Some(allowed) => SUPPORTED_METHODS
                .iter()
                .copied()
                .filter(|method| allowed.iter().any(|candidate| candidate == method))
                .collect::<Vec<_>>()
                .join(" "),
            None => SUPPORTED_METHODS.join(" "),
        }
    }

    /// Public key of the wallet service (advertised in the connection URI).
    pub fn service_pubkey(&self) -> PublicKey {
        self.service_keys.public_key()
    }

    /// Public key of the authorized client.
    pub fn client_pubkey(&self) -> PublicKey {
        self.client_pubkey
    }

    /// Build the `nostr+walletconnect://` connection URI to hand to the client.
    pub fn connection_uri(&self) -> NostrWalletConnectUri {
        NostrWalletConnectUri::new(
            self.service_keys.public_key(),
            self.relays.clone(),
            self.client_secret.clone(),
            self.lud16.clone(),
        )
    }

    /// Connect to the relays, publish the info event, and service requests
    /// until `cancel` is triggered.
    ///
    /// The subscription is established *before* the info event is published,
    /// so a client that observes the info event is guaranteed the service is
    /// already receiving requests.
    ///
    /// Each request is dispatched on its own task: a slow `pay_invoice` never
    /// delays other requests. Cancellation is observed promptly even when no
    /// notifications arrive; in-flight request tasks are left to finish on
    /// their own.
    ///
    /// # Errors
    ///
    /// Returns an error if relays cannot be added, the info event cannot be
    /// published, the subscription cannot be created, or the notification
    /// channel closes unexpectedly. Per-request failures never bubble up
    /// here — they are answered with a NIP-47 error response. Cancellation
    /// returns `Ok(())`.
    pub async fn run<H>(&self, handler: Arc<H>, cancel: CancellationToken) -> Result<()>
    where
        H: NwcRequestHandler + 'static,
    {
        let client = NostrClient::builder()
            .authenticator(SignerAuthenticator::new(self.service_keys.clone()))
            .build();

        for relay in &self.relays {
            client
                .add_relay(relay.clone())
                .await
                .map_err(|e| Error::Relay(format!("add relay {relay}: {e}")))?;
        }

        client.connect().await;

        // Only consider requests created from now on, addressed to us, authored
        // by the authorized client.
        let started_at = Timestamp::now();
        let filter = Filter::new()
            .kind(Kind::WalletConnectRequest)
            .author(self.client_pubkey)
            .pubkey(self.service_keys.public_key())
            .since(started_at);

        // Take the notification stream before subscribing so no request can
        // slip between subscription creation and the receive loop.
        let mut notifications = client.notifications();

        client
            .subscribe(filter)
            .await
            .map_err(|e| Error::Subscription(e.to_string()))?;

        self.publish_info_event(&client).await?;

        // The loop is the only consumer, so dedup needs no locking; it must be
        // checked before spawning so concurrent duplicates cannot race.
        let mut dedup = Dedup::new(DEDUP_CAPACITY);

        let res = loop {
            let notification = tokio::select! {
                _ = cancel.cancelled() => break Ok(()),
                notification = notifications.next() => notification,
            };

            let notification = match notification {
                Some(notification) => notification,
                None => {
                    break Err(Error::Subscription(
                        "notification stream closed".to_string(),
                    ));
                }
            };

            let ClientNotification::Event { event, .. } = notification else {
                continue;
            };

            // Defense in depth: the filter already constrains the author,
            // but never trust a relay to honor it.
            if event.pubkey != self.client_pubkey || event.kind != Kind::WalletConnectRequest {
                continue;
            }

            if event.is_expired()
                || is_before_start(&event, started_at)
                || is_outside_acceptance_window(
                    &event,
                    Timestamp::now(),
                    MAX_REQUEST_AGE_SECS,
                    MAX_FUTURE_SKEW_SECS,
                )
            {
                tracing::debug!("Dropping stale or out-of-window NWC request {}", event.id);
                continue;
            }

            // Replay protection: process each request id at most once.
            if !dedup.insert(event.id) {
                tracing::debug!("Dropping duplicate NWC request {}", event.id);
                continue;
            }

            let handler = handler.clone();
            let service_keys = self.service_keys.clone();
            let client = client.clone();
            tokio::spawn(async move {
                handle_request(&service_keys, &client, handler.as_ref(), &event).await;
            });
        };

        client.disconnect().await;

        res
    }

    /// Publish the kind `13194` info event advertising supported commands and
    /// encryption schemes.
    async fn publish_info_event(&self, client: &NostrClient) -> Result<()> {
        let content = self.info_event_content();

        let event = EventBuilder::new(Kind::WalletConnectInfo, content)
            .tags([info_event_encryption_tag()])
            .finalize(&self.service_keys)
            .map_err(|e| Error::Event(e.to_string()))?;

        client
            .send_event(&event)
            .await
            .map_err(|e| Error::Event(format!("publish info event: {e}")))?;

        Ok(())
    }
}

fn info_event_encryption_tag() -> Tag {
    Tag::custom("encryption", [SUPPORTED_ENCRYPTION_SCHEMES])
}

/// Return true when a request predates this service run.
fn is_before_start(event: &Event, started_at: Timestamp) -> bool {
    event.created_at < started_at
}

/// Return true when a request's `created_at` falls outside the bounded
/// `[now - max_age_secs, now + max_future_skew_secs]` acceptance window.
fn is_outside_acceptance_window(
    event: &Event,
    now: Timestamp,
    max_age_secs: u64,
    max_future_skew_secs: u64,
) -> bool {
    let created = event.created_at.as_secs();
    let now_secs = now.as_secs();
    created < now_secs.saturating_sub(max_age_secs)
        || created > now_secs.saturating_add(max_future_skew_secs)
}

/// Build policy context with service-received time, not client-supplied time.
fn request_context(event: &Event) -> RequestContext {
    RequestContext {
        event_id: event.id.to_hex(),
        client_pubkey: event.pubkey.to_hex(),
        created_at: event.created_at.as_secs(),
        received_at: Timestamp::now().as_secs(),
    }
}

/// Decrypt, dispatch, and respond to a single request event.
///
/// Any failure is logged and (where possible) answered with a NIP-47 error
/// response. This function never panics and never returns an error to the
/// caller — keeping the relay loop alive is part of the security contract.
async fn handle_request<H>(service_keys: &Keys, client: &NostrClient, handler: &H, event: &Event)
where
    H: NwcRequestHandler + ?Sized,
{
    let secret = service_keys.secret_key();

    let (request, encryption) = match decrypt_request(secret, &event.pubkey, &event.content) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!("Failed to decode NWC request {}: {e}", event.id);
            return;
        }
    };

    let context = request_context(event);
    let response = dispatch(handler, request, &context).await;

    if let Err(e) = send_response(
        service_keys,
        client,
        &event.pubkey,
        event.id,
        &response,
        encryption,
    )
    .await
    {
        tracing::warn!("Failed to send NWC response for {}: {e}", event.id);
    }
}

/// Try NIP-44 first, then fall back to NIP-04, and parse the request JSON.
fn decrypt_request(
    secret: &SecretKey,
    author: &PublicKey,
    content: &str,
) -> Result<(Request, Encryption)> {
    let (plaintext, encryption) = match nip44::decrypt(secret, author, content) {
        Ok(plaintext) => (plaintext, Encryption::Nip44),
        Err(_) => {
            let plaintext = nip04::decrypt(secret, author, content)
                .map_err(|e| Error::Encryption(e.to_string()))?;
            (plaintext, Encryption::Nip04)
        }
    };

    let request: Request =
        serde_json::from_str(&plaintext).map_err(|e| Error::Protocol(e.to_string()))?;

    Ok((request, encryption))
}

/// Dispatch a decoded request to the handler and build the NIP-47 response.
async fn dispatch<H>(handler: &H, request: Request, context: &RequestContext) -> Response
where
    H: NwcRequestHandler + ?Sized,
{
    let result_type = request.method;

    let result: std::result::Result<ResponseResult, NIP47Error> = match request.params {
        RequestParams::GetInfo => handler
            .get_info_with_context(context)
            .await
            .map(ResponseResult::GetInfo),
        RequestParams::GetBalance => handler
            .get_balance_with_context(context)
            .await
            .map(ResponseResult::GetBalance),
        RequestParams::MakeInvoice(params) => handler
            .make_invoice_with_context(params, context)
            .await
            .map(ResponseResult::MakeInvoice),
        RequestParams::PayInvoice(params) => handler
            .pay_invoice_with_context(params, context)
            .await
            .map(ResponseResult::PayInvoice),
        RequestParams::LookupInvoice(params) => handler
            .lookup_invoice_with_context(params, context)
            .await
            .map(ResponseResult::LookupInvoice),
        RequestParams::ListTransactions(params) => handler
            .list_transactions_with_context(params, context)
            .await
            .map(ResponseResult::ListTransactions),
        // Commands outside the supported set.
        _ => Err(NIP47Error {
            code: ErrorCode::NotImplemented,
            message: format!("method {} is not implemented", result_type.as_str()),
        }),
    };

    match result {
        Ok(result) => Response {
            result_type,
            error: None,
            result: Some(result),
        },
        Err(error) => Response {
            result_type,
            error: Some(error),
            result: None,
        },
    }
}

/// Encrypt and publish the response event (kind `23195`).
async fn send_response(
    service_keys: &Keys,
    client: &NostrClient,
    client_pubkey: &PublicKey,
    request_id: EventId,
    response: &Response,
    encryption: Encryption,
) -> Result<()> {
    let payload = serde_json::to_string(response)?;
    let secret = service_keys.secret_key();

    let content = match encryption {
        Encryption::Nip44 => nip44::encrypt(secret, client_pubkey, payload, nip44::Version::V2)
            .map_err(|e| Error::Encryption(e.to_string()))?,
        Encryption::Nip04 => nip04::encrypt(secret, client_pubkey, payload)
            .map_err(|e| Error::Encryption(e.to_string()))?,
    };

    let event = EventBuilder::new(Kind::WalletConnectResponse, content)
        .tags([Tag::public_key(*client_pubkey), Tag::event(request_id)])
        .finalize(service_keys)
        .map_err(|e| Error::Event(e.to_string()))?;

    client
        .send_event(&event)
        .await
        .map_err(|e| Error::Event(format!("publish response: {e}")))?;

    Ok(())
}

/// Bounded set of recently-seen request event ids for replay protection.
#[derive(Debug)]
struct Dedup {
    seen: HashSet<EventId>,
    order: VecDeque<EventId>,
    capacity: usize,
}

impl Dedup {
    fn new(capacity: usize) -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    /// Insert an id; returns `true` if it was newly inserted (i.e. should be
    /// processed), `false` if it was already seen.
    fn insert(&mut self, id: EventId) -> bool {
        if !self.seen.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_rejects_repeat_and_evicts_oldest() {
        let mut dedup = Dedup::new(2);
        let keys = Keys::generate();
        let mk = |content: &str| {
            EventBuilder::new(Kind::TextNote, content)
                .finalize(&keys)
                .expect("sign dummy event")
                .id
        };
        let a = mk("a");
        let b = mk("b");
        let c = mk("c");

        assert!(dedup.insert(a));
        assert!(!dedup.insert(a)); // duplicate
        assert!(dedup.insert(b));
        assert!(dedup.insert(c)); // evicts `a`
        assert!(dedup.insert(a)); // `a` evicted, treated as new again
    }

    #[test]
    fn info_event_encryption_tag_matches_nip47_shape() {
        assert_eq!(
            info_event_encryption_tag().to_vec(),
            ["encryption", "nip44_v2 nip04"]
        );
    }

    use async_trait::async_trait;
    use nostr::prelude::nip47::{
        GetBalanceResponse, GetInfoResponse, ListTransactionsRequest, LookupInvoiceRequest,
        LookupInvoiceResponse, MakeInvoiceRequest, MakeInvoiceResponse, PayInvoiceRequest,
        PayInvoiceResponse, Request,
    };

    /// Canned handler: `get_balance` returns a fixed value, everything else errors.
    struct MockHandler;

    #[async_trait]
    impl crate::nwc::handler::NwcRequestHandler for MockHandler {
        async fn get_info(&self) -> std::result::Result<GetInfoResponse, NIP47Error> {
            Err(NIP47Error {
                code: ErrorCode::Internal,
                message: "no".into(),
            })
        }
        async fn get_balance(&self) -> std::result::Result<GetBalanceResponse, NIP47Error> {
            Ok(GetBalanceResponse { balance: 5000 })
        }
        async fn make_invoice(
            &self,
            _: MakeInvoiceRequest,
        ) -> std::result::Result<MakeInvoiceResponse, NIP47Error> {
            unreachable!()
        }
        async fn pay_invoice(
            &self,
            _: PayInvoiceRequest,
        ) -> std::result::Result<PayInvoiceResponse, NIP47Error> {
            unreachable!()
        }
        async fn lookup_invoice(
            &self,
            _: LookupInvoiceRequest,
        ) -> std::result::Result<LookupInvoiceResponse, NIP47Error> {
            unreachable!()
        }
        async fn list_transactions(
            &self,
            _: ListTransactionsRequest,
        ) -> std::result::Result<Vec<LookupInvoiceResponse>, NIP47Error> {
            unreachable!()
        }
    }

    #[derive(Debug, Default)]
    struct ContextHandler {
        seen: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait]
    impl crate::nwc::handler::NwcRequestHandler for ContextHandler {
        async fn get_info(&self) -> std::result::Result<GetInfoResponse, NIP47Error> {
            Err(NIP47Error {
                code: ErrorCode::Internal,
                message: "no".into(),
            })
        }
        async fn get_balance(&self) -> std::result::Result<GetBalanceResponse, NIP47Error> {
            Ok(GetBalanceResponse { balance: 1 })
        }
        async fn get_balance_with_context(
            &self,
            ctx: &crate::nwc::handler::RequestContext,
        ) -> std::result::Result<GetBalanceResponse, NIP47Error> {
            *self.seen.lock().map_err(|_| NIP47Error {
                code: ErrorCode::Internal,
                message: "lock poisoned".into(),
            })? = Some(ctx.event_id.clone());
            self.get_balance().await
        }
        async fn make_invoice(
            &self,
            _: MakeInvoiceRequest,
        ) -> std::result::Result<MakeInvoiceResponse, NIP47Error> {
            unreachable!()
        }
        async fn pay_invoice(
            &self,
            _: PayInvoiceRequest,
        ) -> std::result::Result<PayInvoiceResponse, NIP47Error> {
            unreachable!()
        }
        async fn lookup_invoice(
            &self,
            _: LookupInvoiceRequest,
        ) -> std::result::Result<LookupInvoiceResponse, NIP47Error> {
            unreachable!()
        }
        async fn list_transactions(
            &self,
            _: ListTransactionsRequest,
        ) -> std::result::Result<Vec<LookupInvoiceResponse>, NIP47Error> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn dispatch_passes_request_context() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let handler = ContextHandler { seen: seen.clone() };
        let ctx = crate::nwc::handler::RequestContext {
            event_id: "evt-42".to_owned(),
            client_pubkey: "client".to_owned(),
            created_at: 123,
            received_at: 123,
        };

        let response = dispatch(&handler, Request::get_balance(), &ctx).await;

        assert!(response.error.is_none());
        assert_eq!(seen.lock().expect("lock").as_deref(), Some("evt-42"));
    }

    #[test]
    fn stale_request_is_rejected_without_trusting_relay_filter() {
        let keys = Keys::new(SecretKey::from_slice(&[2; 32]).expect("secret key"));
        let old = EventBuilder::new(Kind::WalletConnectRequest, "old")
            .custom_created_at(Timestamp::from_secs(1))
            .finalize(&keys)
            .expect("event");
        let fresh = EventBuilder::new(Kind::WalletConnectRequest, "fresh")
            .custom_created_at(Timestamp::now())
            .finalize(&keys)
            .expect("event");
        let started_at = Timestamp::from_secs(1_700_000_000);

        assert!(is_before_start(&old, started_at));
        assert!(!is_before_start(&fresh, started_at));
    }

    #[test]
    fn request_context_uses_received_time_not_event_time() {
        let keys = Keys::new(SecretKey::from_slice(&[1; 32]).expect("secret key"));
        let event = EventBuilder::new(Kind::WalletConnectRequest, "request")
            .custom_created_at(Timestamp::from_secs(1))
            .finalize(&keys)
            .expect("event");

        let context = request_context(&event);

        assert_eq!(context.event_id, event.id.to_hex());
        assert_eq!(context.client_pubkey, event.pubkey.to_hex());
        assert!(context.received_at >= 1_700_000_000);
        assert!(context.received_at > event.created_at.as_secs());
    }

    /// Encrypt a request the way a client would, with the chosen scheme.
    fn encrypt_request(
        client_secret: &SecretKey,
        service_pubkey: &PublicKey,
        request: &Request,
        scheme: Encryption,
    ) -> String {
        let json = serde_json::to_string(request).expect("serialize request");
        match scheme {
            Encryption::Nip44 => {
                nip44::encrypt(client_secret, service_pubkey, json, nip44::Version::V2)
                    .expect("nip44 encrypt")
            }
            Encryption::Nip04 => {
                nip04::encrypt(client_secret, service_pubkey, json).expect("nip04 encrypt")
            }
        }
    }

    #[test]
    fn decrypt_request_roundtrips_both_schemes() {
        let service = Keys::generate();
        let client = Keys::generate();
        let request = Request::get_balance();

        for scheme in [Encryption::Nip44, Encryption::Nip04] {
            let content =
                encrypt_request(client.secret_key(), &service.public_key(), &request, scheme);

            let (decoded, detected) =
                decrypt_request(service.secret_key(), &client.public_key(), &content)
                    .expect("decrypt request");

            assert_eq!(detected, scheme);
            assert_eq!(decoded.method, request.method);
        }
    }

    #[tokio::test]
    async fn dispatch_get_balance_ok() {
        let context = crate::nwc::handler::RequestContext {
            event_id: "event-1".to_owned(),
            client_pubkey: "client".to_owned(),
            created_at: 1,
            received_at: 1,
        };
        let response = dispatch(&MockHandler, Request::get_balance(), &context).await;
        assert!(response.error.is_none());
        match response.result {
            Some(ResponseResult::GetBalance(b)) => assert_eq!(b.balance, 5000),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_unsupported_method_is_not_implemented() {
        // pay_keysend is outside the supported set.
        let request = Request::pay_keysend(nostr::prelude::nip47::PayKeysendRequest {
            id: None,
            amount: 1000,
            pubkey: "00".repeat(32),
            preimage: None,
            tlv_records: Vec::new(),
        });

        let context = crate::nwc::handler::RequestContext {
            event_id: "event-2".to_owned(),
            client_pubkey: "client".to_owned(),
            created_at: 1,
            received_at: 1,
        };
        let response = dispatch(&MockHandler, request, &context).await;
        let error = response.error.expect("unsupported method should error");
        assert_eq!(error.code, ErrorCode::NotImplemented);
        assert!(response.result.is_none());
    }

    #[test]
    fn info_event_advertises_only_allowed_methods_in_canonical_order() {
        let config = NwcServiceConfig {
            service_keys: Keys::generate(),
            client_secret: SecretKey::generate(),
            relays: vec![RelayUrl::parse("wss://relay.example.com").expect("relay")],
            lud16: None,
        };
        let default_service = NwcService::new(config.clone()).expect("service");
        assert_eq!(
            default_service.info_event_content(),
            SUPPORTED_METHODS.join(" ")
        );

        let scoped = NwcService::new(config)
            .expect("service")
            .with_allowed_methods(vec![
                "get_info".to_owned(),
                "bogus_method".to_owned(),
                "get_balance".to_owned(),
            ]);
        assert_eq!(scoped.info_event_content(), "get_balance get_info");
    }

    #[test]
    fn acceptance_window_rejects_old_and_far_future_events() {
        let keys = Keys::generate();
        let mk = |ts: u64| {
            EventBuilder::new(Kind::WalletConnectRequest, "x")
                .custom_created_at(Timestamp::from_secs(ts))
                .finalize(&keys)
                .expect("event")
        };
        let now = Timestamp::from_secs(10_000);
        assert!(is_outside_acceptance_window(
            &mk(10_000 - 3_601),
            now,
            3_600,
            300
        ));
        assert!(!is_outside_acceptance_window(
            &mk(10_000 - 3_600),
            now,
            3_600,
            300
        ));
        assert!(!is_outside_acceptance_window(&mk(10_300), now, 3_600, 300));
        assert!(is_outside_acceptance_window(&mk(10_301), now, 3_600, 300));
    }
}
