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
//! - **Replay / idempotency**: each request event id is dispatched at most
//!   once, so relay re-delivery cannot trigger a duplicate `pay_invoice`. The
//!   outcome is cached and republished on a later delivery of the same event,
//!   so a client that missed the first response still sees the original
//!   result.
//! - **Publish retry**: after a handler returns (including a successful
//!   `pay_invoice`), the kind `23195` response is published with backoff and
//!   a relay reconnect, so a dropped send is not reported as a failed
//!   payment.
//! - **Freshness**: requests carrying an expired NIP-40 `expiration` tag are
//!   dropped, and only events created after the service started are considered.
//! - **No information leak / no panics**: every handler error is mapped to a
//!   NIP-47 error response; the relay loop never aborts on a single bad request.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

        // Outcomes are shared with in-flight request tasks so a later delivery
        // of the same event can republish the cached result without paying
        // again. `observe` is called in this loop, before spawn, so concurrent
        // duplicates cannot race into a second dispatch.
        let outcomes = Arc::new(Mutex::new(OutcomeCache::new(DEDUP_CAPACITY)));

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

            let action = lock_outcomes(&outcomes).observe(event.id);
            let handler = handler.clone();
            let service_keys = self.service_keys.clone();
            let client = client.clone();
            let outcomes = Arc::clone(&outcomes);
            tokio::spawn(async move {
                let publisher = NostrPublisher { client: &client };
                fulfill_request(
                    &service_keys,
                    &publisher,
                    handler.as_ref(),
                    &event,
                    &outcomes,
                    action,
                    &PUBLISH_RETRY_DELAYS,
                )
                .await;
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

/// Backoff between attempts to publish a NIP-47 response after the first try.
const PUBLISH_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(200),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// Decrypt, dispatch if needed, and publish the response for one request.
///
/// A cached outcome is republished without calling the handler again. Any
/// failure is logged; this function never panics and never returns an error
/// to the caller — keeping the relay loop alive is part of the security
/// contract.
async fn fulfill_request<H, P>(
    service_keys: &Keys,
    publisher: &P,
    handler: &H,
    event: &Event,
    outcomes: &Mutex<OutcomeCache>,
    action: Observe,
    retry_delays: &[Duration],
) where
    H: NwcRequestHandler + ?Sized,
    P: ResponseSink,
{
    let (response, encryption, client_pubkey) = match action {
        Observe::InFlight => return,
        Observe::Done(cached) => (cached.response, cached.encryption, cached.client_pubkey),
        Observe::Fresh => match process_request(service_keys, handler, event).await {
            Some((response, encryption)) => {
                lock_outcomes(outcomes).complete(
                    event.id,
                    Box::new(CachedOutcome {
                        response: response.clone(),
                        encryption,
                        client_pubkey: event.pubkey,
                    }),
                );
                (response, encryption, event.pubkey)
            }
            None => {
                lock_outcomes(outcomes).abandon(event.id);
                return;
            }
        },
    };

    if let Err(e) = send_response(
        service_keys,
        publisher,
        &client_pubkey,
        event.id,
        &response,
        encryption,
        retry_delays,
    )
    .await
    {
        tracing::warn!("Failed to send NWC response for {}: {e}", event.id);
    }
}

/// Decrypt a request and dispatch it to the handler.
///
/// Returns `None` when the event cannot be decoded (nothing to cache or
/// answer).
async fn process_request<H>(
    service_keys: &Keys,
    handler: &H,
    event: &Event,
) -> Option<(Response, Encryption)>
where
    H: NwcRequestHandler + ?Sized,
{
    let secret = service_keys.secret_key();
    match decrypt_request(secret, &event.pubkey, &event.content) {
        Ok((request, encryption)) => {
            let context = request_context(event);
            Some((dispatch(handler, request, &context).await, encryption))
        }
        Err(e) => {
            tracing::warn!("Failed to decode NWC request {}: {e}", event.id);
            None
        }
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
///
/// The signed event is reused across retries. Each failed send is followed
/// by a reconnect and the next delay in `retry_delays`.
async fn send_response<P: ResponseSink>(
    service_keys: &Keys,
    publisher: &P,
    client_pubkey: &PublicKey,
    request_id: EventId,
    response: &Response,
    encryption: Encryption,
    retry_delays: &[Duration],
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

    publish_with_retry(publisher, &event, retry_delays).await
}

/// Publish `event`, reconnecting and backing off after each failure.
async fn publish_with_retry<P: ResponseSink>(
    publisher: &P,
    event: &Event,
    retry_delays: &[Duration],
) -> Result<()> {
    let mut last_error = match publisher.publish(event).await {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    for delay in retry_delays {
        tracing::debug!(
            "Retrying NWC response publish for {} after {delay:?}: {last_error}",
            event.id
        );
        publisher.reconnect().await;
        if !delay.is_zero() {
            tokio::time::sleep(*delay).await;
        }
        match publisher.publish(event).await {
            Ok(()) => return Ok(()),
            Err(error) => last_error = error,
        }
    }

    Err(last_error)
}

/// Publishes a kind `23195` response and can reconnect the relay client.
trait ResponseSink: Send + Sync {
    fn publish(&self, event: &Event) -> impl std::future::Future<Output = Result<()>> + Send;
    fn reconnect(&self) -> impl std::future::Future<Output = ()> + Send;
}

/// [`ResponseSink`] backed by the service's [`NostrClient`].
struct NostrPublisher<'a> {
    client: &'a NostrClient,
}

impl ResponseSink for NostrPublisher<'_> {
    async fn publish(&self, event: &Event) -> Result<()> {
        self.client
            .send_event(event)
            .await
            .map_err(|e| Error::Event(format!("publish response: {e}")))?;
        Ok(())
    }

    async fn reconnect(&self) {
        self.client.connect().await;
    }
}

/// Cached handler result for one request event id.
#[derive(Debug, Clone)]
struct CachedOutcome {
    response: Response,
    encryption: Encryption,
    client_pubkey: PublicKey,
}

/// What to do with a request event the service has just observed.
#[derive(Debug, Clone)]
enum Observe {
    /// First time this event id is seen; dispatch the handler.
    Fresh,
    /// A task is already handling this event; do nothing.
    InFlight,
    /// The handler already finished; republish this cached outcome.
    Done(Box<CachedOutcome>),
}

enum CacheSlot {
    InFlight,
    Done(Box<CachedOutcome>),
}

/// Bounded map of request event ids to their in-flight or finished outcome.
struct OutcomeCache {
    entries: HashMap<EventId, CacheSlot>,
    order: VecDeque<EventId>,
    capacity: usize,
}

impl OutcomeCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    /// Record a first sighting, or return the state of a previously seen id.
    fn observe(&mut self, id: EventId) -> Observe {
        match self.entries.get(&id) {
            Some(CacheSlot::InFlight) => Observe::InFlight,
            Some(CacheSlot::Done(outcome)) => Observe::Done(outcome.clone()),
            None => {
                self.evict_if_full();
                self.entries.insert(id, CacheSlot::InFlight);
                self.order.push_back(id);
                Observe::Fresh
            }
        }
    }

    fn complete(&mut self, id: EventId, outcome: Box<CachedOutcome>) {
        self.entries.insert(id, CacheSlot::Done(outcome));
        if !self.order.contains(&id) {
            self.order.push_back(id);
        }
    }

    /// Drop an in-flight entry whose request could not be decoded, so a
    /// later well-formed delivery can be tried.
    fn abandon(&mut self, id: EventId) {
        if matches!(self.entries.get(&id), Some(CacheSlot::InFlight)) {
            self.entries.remove(&id);
            self.order.retain(|existing| *existing != id);
        }
    }

    fn evict_if_full(&mut self) {
        while self.entries.len() >= self.capacity {
            let evict = self
                .order
                .iter()
                .find(|id| matches!(self.entries.get(id), Some(CacheSlot::Done(_))))
                .copied();
            let Some(oldest) = evict else {
                break;
            };
            self.order.retain(|existing| *existing != oldest);
            self.entries.remove(&oldest);
        }
    }
}

fn lock_outcomes(cache: &Mutex<OutcomeCache>) -> std::sync::MutexGuard<'_, OutcomeCache> {
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_outcome() -> CachedOutcome {
        CachedOutcome {
            response: Response {
                result_type: nostr::prelude::nip47::Method::GetBalance,
                error: None,
                result: Some(ResponseResult::GetBalance(
                    nostr::prelude::nip47::GetBalanceResponse { balance: 1 },
                )),
            },
            encryption: Encryption::Nip44,
            client_pubkey: Keys::generate().public_key(),
        }
    }

    #[test]
    fn outcome_cache_replays_done_results_and_evicts_oldest_completed() {
        let mut cache = OutcomeCache::new(2);
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

        assert!(matches!(cache.observe(a), Observe::Fresh));
        assert!(matches!(cache.observe(a), Observe::InFlight));
        cache.complete(a, Box::new(dummy_outcome()));
        assert!(matches!(cache.observe(a), Observe::Done(_)));

        assert!(matches!(cache.observe(b), Observe::Fresh));
        cache.complete(b, Box::new(dummy_outcome()));
        // Capacity 2 is full of completed entries; `c` evicts `a`.
        assert!(matches!(cache.observe(c), Observe::Fresh));
        assert!(matches!(cache.observe(a), Observe::Fresh));
    }

    #[test]
    fn outcome_cache_does_not_evict_in_flight_entries() {
        let mut cache = OutcomeCache::new(1);
        let keys = Keys::generate();
        let mk = |content: &str| {
            EventBuilder::new(Kind::TextNote, content)
                .finalize(&keys)
                .expect("sign dummy event")
                .id
        };
        let a = mk("a");
        let b = mk("b");

        assert!(matches!(cache.observe(a), Observe::Fresh));
        // `a` is still in flight, so it must not be evicted when `b` arrives.
        assert!(matches!(cache.observe(b), Observe::Fresh));
        assert!(matches!(cache.observe(a), Observe::InFlight));
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

    struct RecordingPublisher {
        /// Remaining forced failures before a publish succeeds.
        fail_remaining: std::sync::atomic::AtomicU32,
        publishes: std::sync::atomic::AtomicU32,
        reconnects: std::sync::atomic::AtomicU32,
        /// When true, every publish fails (used to leave a cached outcome unpublished).
        always_fail: bool,
    }

    impl RecordingPublisher {
        fn fail_then_succeed(failures: u32) -> Self {
            Self {
                fail_remaining: std::sync::atomic::AtomicU32::new(failures),
                publishes: std::sync::atomic::AtomicU32::new(0),
                reconnects: std::sync::atomic::AtomicU32::new(0),
                always_fail: false,
            }
        }

        fn always_fail() -> Self {
            Self {
                fail_remaining: std::sync::atomic::AtomicU32::new(0),
                publishes: std::sync::atomic::AtomicU32::new(0),
                reconnects: std::sync::atomic::AtomicU32::new(0),
                always_fail: true,
            }
        }
    }

    impl ResponseSink for RecordingPublisher {
        async fn publish(&self, _event: &Event) -> Result<()> {
            self.publishes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.always_fail {
                return Err(Error::Event("relay not connected".to_owned()));
            }
            let left = self
                .fail_remaining
                .load(std::sync::atomic::Ordering::SeqCst);
            if left > 0 {
                self.fail_remaining
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                return Err(Error::Event("relay not connected".to_owned()));
            }
            Ok(())
        }

        async fn reconnect(&self) {
            self.reconnects
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Counts `pay_invoice` calls and always succeeds.
    struct CountingPayHandler {
        pays: std::sync::atomic::AtomicU32,
    }

    #[async_trait]
    impl crate::nwc::handler::NwcRequestHandler for CountingPayHandler {
        async fn get_info(&self) -> std::result::Result<GetInfoResponse, NIP47Error> {
            Err(NIP47Error {
                code: ErrorCode::Internal,
                message: "unused".to_owned(),
            })
        }
        async fn get_balance(&self) -> std::result::Result<GetBalanceResponse, NIP47Error> {
            Err(NIP47Error {
                code: ErrorCode::Internal,
                message: "unused".to_owned(),
            })
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
            self.pays.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(PayInvoiceResponse {
                preimage: "ab".repeat(16),
                fees_paid: Some(0),
            })
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

    fn signed_pay_request(service: &Keys, client: &Keys) -> Event {
        let request = Request::pay_invoice(PayInvoiceRequest {
            id: None,
            invoice: "lnbc1test".to_owned(),
            amount: None,
        });
        let content = encrypt_request(
            client.secret_key(),
            &service.public_key(),
            &request,
            Encryption::Nip44,
        );
        EventBuilder::new(Kind::WalletConnectRequest, content)
            .finalize(client)
            .expect("sign request")
    }

    fn observe(cache: &Mutex<OutcomeCache>, id: EventId) -> Observe {
        lock_outcomes(cache).observe(id)
    }

    /// A successful melt whose first response publish fails must still be
    /// published after a reconnect, and must not be paid a second time.
    #[tokio::test]
    async fn publish_failure_after_successful_pay_is_retried_without_paying_twice() {
        let service = Keys::generate();
        let client = Keys::generate();
        let event = signed_pay_request(&service, &client);
        let cache = Mutex::new(OutcomeCache::new(DEDUP_CAPACITY));
        let handler = CountingPayHandler {
            pays: std::sync::atomic::AtomicU32::new(0),
        };
        let publisher = RecordingPublisher::fail_then_succeed(1);

        let action = observe(&cache, event.id);
        fulfill_request(
            &service,
            &publisher,
            &handler,
            &event,
            &cache,
            action,
            &[Duration::ZERO],
        )
        .await;

        assert_eq!(handler.pays.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            publisher
                .publishes
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(
            publisher
                .reconnects
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(matches!(observe(&cache, event.id), Observe::Done(_)));

        // A retried or re-delivered request is answered from the cache.
        let action = observe(&cache, event.id);
        fulfill_request(&service, &publisher, &handler, &event, &cache, action, &[]).await;
        assert_eq!(handler.pays.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            publisher
                .publishes
                .load(std::sync::atomic::Ordering::SeqCst),
            3
        );
    }

    /// If every publish attempt fails after a successful pay, a later replay
    /// of the same request still returns that success and does not pay again.
    #[tokio::test]
    async fn replay_after_failed_publish_returns_cached_success() {
        let service = Keys::generate();
        let client = Keys::generate();
        let event = signed_pay_request(&service, &client);
        let cache = Mutex::new(OutcomeCache::new(DEDUP_CAPACITY));
        let handler = CountingPayHandler {
            pays: std::sync::atomic::AtomicU32::new(0),
        };
        let failing = RecordingPublisher::always_fail();

        let action = observe(&cache, event.id);
        fulfill_request(&service, &failing, &handler, &event, &cache, action, &[]).await;
        assert_eq!(handler.pays.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(observe(&cache, event.id), Observe::Done(_)));

        let recovering = RecordingPublisher::fail_then_succeed(0);
        let action = observe(&cache, event.id);
        fulfill_request(&service, &recovering, &handler, &event, &cache, action, &[]).await;
        assert_eq!(handler.pays.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            recovering
                .publishes
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
}
