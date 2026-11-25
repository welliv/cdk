//! Policy-layer NWC end-to-end harness.
//!
//! Starts an in-process Nostr relay, a pure in-memory Cashu mint and a real
//! `WalletNwcHandler`, then drives the stack with a standard NWC client.

#![allow(missing_docs)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::Router;
use cdk::cdk_database::{self, WalletDatabase};
use cdk::nuts::CurrencyUnit;
use cdk::wallet::{WalletBuilder, WalletNwcHandler};
use cdk_fake_wallet::create_fake_invoice;
use cdk_integration_tests::init_pure_tests::{
    create_and_start_in_memory_test_mint, create_and_start_in_memory_test_mint_with_fee,
    fund_wallet, setup_tracing, DirectMintConnection,
};
use cdk_nostr::nwc::{NwcService, NwcServiceConfig};
use cdk_nwc_forward::ForwardingNwcHandler;
use cdk_nwc_policy::manager::ConnectionManager;
use cdk_nwc_policy::{BudgetPeriod, ConnectionPolicy, PolicyNwcHandler};
use futures::{SinkExt, StreamExt};
use nostr::prelude::{Event, Filter, Keys, Kind, PublicKey, RelayUrl, SecretKey, Timestamp};
use nostr_sdk::prelude::Client as NostrClient;
use nwc::prelude::{
    MakeInvoiceRequest, NostrWalletConnect, NostrWalletConnectUri, PayInvoiceRequest,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct RelayState {
    events: Arc<Mutex<Vec<Event>>>,
    tx: broadcast::Sender<Event>,
}

impl RelayState {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            tx,
        }
    }
}

struct TestRelay {
    url: RelayUrl,
    state: RelayState,
    task: JoinHandle<()>,
}

impl TestRelay {
    async fn start() -> Self {
        let state = RelayState::new();
        let app = Router::new()
            .route("/", axum::routing::get(ws_handler))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("relay listener");
        let port = listener.local_addr().expect("relay addr").port();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            url: RelayUrl::parse(&format!("ws://127.0.0.1:{port}")).expect("relay url"),
            state,
            task,
        }
    }

    async fn events(&self) -> Vec<Event> {
        self.state.events.lock().await.clone()
    }
}

impl Drop for TestRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<RelayState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: RelayState) {
    let (mut sink, mut stream) = socket.split();
    let mut incoming_events = state.tx.subscribe();
    let mut subscriptions: HashMap<String, Vec<Value>> = HashMap::new();

    loop {
        tokio::select! {
            message = stream.next() => {
                let Some(Ok(message)) = message else { break };
                let Message::Text(text) = message else { continue };
                let request: Value = match serde_json::from_str(text.as_ref()) {
                    Ok(request) => request,
                    Err(_) => continue,
                };
                let Some(kind) = request.get(0).and_then(Value::as_str) else { continue };

                match kind {
                    "REQ" => {
                        let Some(sub_id) = request.get(1).and_then(Value::as_str) else { continue };
                        let filters: Vec<Value> = request
                            .as_array()
                            .into_iter()
                            .flatten()
                            .skip(2)
                            .cloned()
                            .collect();
                        subscriptions.insert(sub_id.to_owned(), filters.clone());

                        let stored = state.events.lock().await.clone();
                        for event in stored {
                            if filters.iter().any(|filter| matches_filter(&event, filter)) {
                                let message = json!(["EVENT", sub_id, event]).to_string();
                                if sink.send(Message::Text(message.into())).await.is_err() {
                                    return;
                                }
                            }
                        }
                        let eose = json!(["EOSE", sub_id]).to_string();
                        if sink.send(Message::Text(eose.into())).await.is_err() {
                            return;
                        }
                    }
                    "EVENT" => {
                        let Some(event): Option<Event> = request
                            .get(1)
                            .and_then(|value| serde_json::from_value(value.clone()).ok())
                        else { continue };
                        let event_id = event.id.to_hex();
                        state.events.lock().await.push(event.clone());
                        let _ = state.tx.send(event);
                        let ok = json!(["OK", event_id, true, ""]).to_string();
                        if sink.send(Message::Text(ok.into())).await.is_err() {
                            return;
                        }
                    }
                    "CLOSE" => {
                        if let Some(sub_id) = request.get(1).and_then(Value::as_str) {
                            subscriptions.remove(sub_id);
                        }
                    }
                    _ => {}
                }
            }
            incoming = incoming_events.recv() => {
                let Ok(event) = incoming else { continue };
                for (sub_id, filters) in &subscriptions {
                    if filters.iter().any(|filter| matches_filter(&event, filter)) {
                        let message = json!(["EVENT", sub_id, event]).to_string();
                        if sink.send(Message::Text(message.into())).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

fn matches_filter(event: &Event, filter: &Value) -> bool {
    let value = match serde_json::to_value(event) {
        Ok(value) => value,
        Err(_) => return false,
    };

    if let Some(kinds) = filter.get("kinds").and_then(Value::as_array) {
        let event_kind = value.get("kind").and_then(Value::as_u64);
        let mut matched = false;
        for kind in kinds {
            if kind.as_u64() == event_kind {
                matched = true;
                break;
            }
        }
        if !matched {
            return false;
        }
    }
    if let Some(authors) = filter.get("authors").and_then(Value::as_array) {
        let author = value.get("pubkey").and_then(Value::as_str);
        let mut matched = false;
        for candidate in authors {
            if candidate.as_str() == author {
                matched = true;
                break;
            }
        }
        if !matched {
            return false;
        }
    }
    if let Some(ids) = filter.get("ids").and_then(Value::as_array) {
        let id = value.get("id").and_then(Value::as_str);
        let mut matched = false;
        for candidate in ids {
            if candidate.as_str() == id {
                matched = true;
                break;
            }
        }
        if !matched {
            return false;
        }
    }
    if let Some(since) = filter.get("since").and_then(Value::as_u64) {
        if value.get("created_at").and_then(Value::as_u64).unwrap_or(0) < since {
            return false;
        }
    }
    if let Some(until) = filter.get("until").and_then(Value::as_u64) {
        if value
            .get("created_at")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX)
            > until
        {
            return false;
        }
    }

    let tags = value.get("tags").and_then(Value::as_array);
    for key in ["#p", "#e", "#a"] {
        if let Some(expected) = filter.get(key).and_then(Value::as_array) {
            let mut matched = false;
            if let Some(tags) = tags {
                for tag in tags {
                    let Some(tag) = tag.as_array() else { continue };
                    if tag.first().and_then(Value::as_str) != Some(&key[1..]) {
                        continue;
                    }
                    let Some(tag_value) = tag.get(1).and_then(Value::as_str) else {
                        continue;
                    };
                    if expected
                        .iter()
                        .any(|candidate| candidate.as_str() == Some(tag_value))
                    {
                        matched = true;
                        break;
                    }
                }
            }
            if !matched {
                return false;
            }
        }
    }

    true
}

async fn wait_for_info_event(
    relay_url: &RelayUrl,
    service_pubkey: PublicKey,
    timeout: Duration,
) -> bool {
    let client = NostrClient::new();
    if client.add_relay(relay_url.clone()).await.is_err() {
        return false;
    }
    client.connect().await;
    let filter = Filter::new()
        .kind(Kind::WalletConnectInfo)
        .author(service_pubkey);
    let deadline = tokio::time::Instant::now() + timeout;
    let found = loop {
        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        match tokio::time::timeout(Duration::from_secs(2), client.fetch_events(filter.clone()))
            .await
        {
            Ok(Ok(events)) if !events.is_empty() => break true,
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };
    client.disconnect().await;
    found
}

fn policy(client_pubkey: PublicKey) -> ConnectionPolicy {
    ConnectionPolicy {
        id: "e2e-app".to_owned(),
        name: "E2E app".to_owned(),
        client_pubkey: client_pubkey.to_hex(),
        allowed_methods: vec![
            "get_info".to_owned(),
            "get_balance".to_owned(),
            "pay_invoice".to_owned(),
        ],
        budget_msat: Some(200_000),
        budget_period: BudgetPeriod::Daily,
        max_single_payment_msat: Some(200_000),
        max_fee_reserve_msat: 10_000,
        fee_overage_msat: 0,
        expires_at: None,
        revoked: false,
        used_msat: 0,
        period_start_secs: Timestamp::now().as_secs(),
    }
}

#[tokio::test]
async fn policy_nwc_e2e_with_real_wallet_and_relay() {
    setup_tracing();
    let relay = TestRelay::start().await;

    let mint = create_and_start_in_memory_test_mint()
        .await
        .expect("in-memory test mint");
    let mint_info = mint.mint_info().await.expect("mint info");
    let mint_url = mint_info
        .urls
        .as_ref()
        .and_then(|urls| urls.first())
        .expect("mint url")
        .clone();
    let connector = DirectMintConnection::new(mint.clone());
    let localstore: Arc<dyn WalletDatabase<cdk_database::Error> + Send + Sync> = Arc::new(
        cdk_sqlite::wallet::memory::empty()
            .await
            .expect("wallet database"),
    );
    let seed = bip39::Mnemonic::generate(12)
        .expect("mnemonic")
        .to_seed_normalized("");
    let wallet = WalletBuilder::new()
        .mint_url(mint_url.parse().expect("mint url"))
        .unit(CurrencyUnit::Sat)
        .localstore(localstore)
        .seed(seed)
        .client(connector)
        .use_http_subscription()
        .build()
        .expect("test wallet");
    fund_wallet(&wallet, 1_000, None)
        .await
        .expect("fund wallet");
    let wallet = Arc::new(wallet);

    let service_secret_key = wallet.derive_nwc_secret_key().expect("nwc key");
    let service_keys = Keys::parse(&service_secret_key.to_secret_hex()).expect("service keys");
    let client_secret = SecretKey::generate();
    let client_pubkey = Keys::new(client_secret.clone()).public_key();
    let ledger_path =
        std::env::temp_dir().join(format!("nwc-policy-e2e-{}.json", uuid::Uuid::new_v4()));

    let wallet_handler = Arc::new(WalletNwcHandler::new(wallet.clone(), None));
    let handler =
        Arc::new(PolicyNwcHandler::new(&ledger_path, wallet_handler).expect("policy handler"));
    handler
        .add_connection(policy(client_pubkey))
        .expect("policy");

    let service = NwcService::new(NwcServiceConfig {
        service_keys: service_keys.clone(),
        client_secret: client_secret.clone(),
        relays: vec![relay.url.clone()],
        lud16: None,
    })
    .expect("nwc service");
    let uri = service.connection_uri().to_string();
    let service_pubkey = service.service_pubkey();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task_handler = handler.clone();
    let service_task = tokio::spawn(async move {
        let _ = service.run(task_handler, task_cancel).await;
    });

    assert!(
        wait_for_info_event(&relay.url, service_pubkey, Duration::from_secs(15)).await,
        "service info event not found"
    );

    let parsed_uri: NostrWalletConnectUri = uri.parse().expect("nwc uri");
    let nwc_client = NostrWalletConnect::builder(parsed_uri)
        .timeout(Duration::from_secs(30))
        .build();

    let info = nwc_client.get_info().await.expect("get_info");
    assert_eq!(info.methods.len(), 3, "get_info must be filtered by policy");

    let balance = nwc_client.get_balance().await.expect("get_balance");
    assert_eq!(balance.balance, 1_000_000);

    let restricted = nwc_client
        .make_invoice(MakeInvoiceRequest {
            amount: 10_000,
            description: None,
            description_hash: None,
            expiry: None,
        })
        .await;
    assert!(
        restricted.is_err(),
        "make_invoice must be restricted by policy"
    );

    let fake_invoice = create_fake_invoice(100_000, "policy e2e payment".to_owned());
    let paid = nwc_client
        .pay_invoice(PayInvoiceRequest {
            id: None,
            invoice: fake_invoice.to_string(),
            amount: None,
        })
        .await
        .expect("pay_invoice through policy");
    assert!(paid.fees_paid.is_some());

    let used = handler.used_msat("e2e-app").expect("used budget");
    assert!(
        (100_000..=110_000).contains(&used),
        "budget must charge payment and fee"
    );

    let pay_events: Vec<Event> = relay
        .events()
        .await
        .into_iter()
        .filter(|event| event.kind == Kind::WalletConnectRequest && event.pubkey == client_pubkey)
        .collect();
    let pay_event = pay_events
        .last()
        .expect("captured pay_invoice request")
        .clone();

    cancel.cancel();
    let _ = service_task.await;
    drop(handler);

    let wallet_handler2 = Arc::new(WalletNwcHandler::new(wallet.clone(), None));
    let handler2 = Arc::new(
        PolicyNwcHandler::new(&ledger_path, wallet_handler2).expect("reopened policy handler"),
    );
    let service2 = NwcService::new(NwcServiceConfig {
        service_keys,
        client_secret: client_secret.clone(),
        relays: vec![relay.url.clone()],
        lud16: None,
    })
    .expect("restarted nwc service");
    let cancel2 = CancellationToken::new();
    let task_cancel2 = cancel2.clone();
    let task_handler2 = handler2.clone();
    let service_task2 = tokio::spawn(async move {
        let _ = service2.run(task_handler2, task_cancel2).await;
    });
    assert!(
        wait_for_info_event(&relay.url, service_pubkey, Duration::from_secs(15)).await,
        "restarted service info event not found"
    );

    let before_replay = nwc_client
        .get_balance()
        .await
        .expect("balance before replay");
    let publisher = NostrClient::new();
    publisher
        .add_relay(relay.url.clone())
        .await
        .expect("add relay");
    publisher.connect().await;
    let _ = publisher.send_event(&pay_event).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after_replay = nwc_client
        .get_balance()
        .await
        .expect("balance after replay");
    assert_eq!(
        before_replay.balance, after_replay.balance,
        "replayed payment must not spend again after restart"
    );

    handler2.revoke("e2e-app").expect("revoke connection");
    let revoked = nwc_client.get_balance().await;
    assert!(revoked.is_err(), "revoked connection must be unauthorized");

    cancel2.cancel();
    let _ = service_task2.await;
    publisher.disconnect().await;
    let _ = std::fs::remove_file(&ledger_path);
}

#[tokio::test]
async fn manager_e2e_create_start_and_revoke() {
    setup_tracing();
    let relay = TestRelay::start().await;

    let mint = create_and_start_in_memory_test_mint()
        .await
        .expect("in-memory test mint");
    let mint_info = mint.mint_info().await.expect("mint info");
    let mint_url = mint_info
        .urls
        .as_ref()
        .and_then(|urls| urls.first())
        .expect("mint url")
        .clone();
    let connector = DirectMintConnection::new(mint.clone());
    let localstore: Arc<dyn WalletDatabase<cdk_database::Error> + Send + Sync> = Arc::new(
        cdk_sqlite::wallet::memory::empty()
            .await
            .expect("wallet database"),
    );
    let seed = bip39::Mnemonic::generate(12)
        .expect("mnemonic")
        .to_seed_normalized("");
    let wallet = WalletBuilder::new()
        .mint_url(mint_url.parse().expect("mint url"))
        .unit(CurrencyUnit::Sat)
        .localstore(localstore)
        .seed(seed)
        .client(connector)
        .use_http_subscription()
        .build()
        .expect("test wallet");
    fund_wallet(&wallet, 1_000, None)
        .await
        .expect("fund wallet");

    let suffix = uuid::Uuid::new_v4();
    let manager_path = std::env::temp_dir().join(format!("nwc-manager-e2e-{suffix}.json"));
    let policy_path = std::env::temp_dir().join(format!("nwc-policy-e2e-{suffix}.json"));
    let handler = Arc::new(WalletNwcHandler::new(Arc::new(wallet), None));
    let mut manager =
        ConnectionManager::new(&manager_path, &policy_path, handler).expect("connection manager");

    let dummy_key = Keys::new(SecretKey::generate()).public_key();
    let uri = manager
        .create_connection(
            "Managed app".to_owned(),
            vec![relay.url.to_string()],
            policy(dummy_key),
        )
        .expect("managed connection");
    let record = manager.list_connections()[0].clone();
    let service_pubkey = Keys::parse(&record.service_secret_hex)
        .expect("service keys")
        .public_key();

    manager
        .start_all()
        .await
        .expect("start all managed services");
    assert!(
        wait_for_info_event(&relay.url, service_pubkey, Duration::from_secs(15)).await,
        "managed service info event not found"
    );
    let info_event = relay
        .events()
        .await
        .into_iter()
        .rfind(|e| e.kind == Kind::WalletConnectInfo && e.pubkey == service_pubkey)
        .expect("managed info event in relay");
    assert_eq!(
        info_event.content, "pay_invoice get_balance get_info",
        "kind 13194 info event must only advertise connection's allowed methods"
    );

    let parsed_uri: NostrWalletConnectUri = uri.parse().expect("managed nwc uri");
    let client = NostrWalletConnect::builder(parsed_uri)
        .timeout(Duration::from_secs(30))
        .build();
    let balance = client.get_balance().await.expect("managed get_balance");
    assert_eq!(balance.balance, 1_000_000);

    manager.revoke(&record.id).await.expect("manager revoke");
    let revoked = client.get_balance().await;
    assert!(revoked.is_err(), "manager revocation must take effect");

    manager.stop_all().await;
    let _ = std::fs::remove_file(&manager_path);
    let _ = std::fs::remove_file(&policy_path);
}

#[tokio::test]
async fn forwarding_policy_e2e_uses_upstream_wallet() {
    setup_tracing();
    let relay = TestRelay::start().await;

    let mint = create_and_start_in_memory_test_mint()
        .await
        .expect("in-memory test mint");
    let mint_info = mint.mint_info().await.expect("mint info");
    let mint_url = mint_info
        .urls
        .as_ref()
        .and_then(|urls| urls.first())
        .expect("mint url")
        .clone();
    let connector = DirectMintConnection::new(mint.clone());
    let localstore: Arc<dyn WalletDatabase<cdk_database::Error> + Send + Sync> = Arc::new(
        cdk_sqlite::wallet::memory::empty()
            .await
            .expect("wallet database"),
    );
    let seed = bip39::Mnemonic::generate(12)
        .expect("mnemonic")
        .to_seed_normalized("");
    let wallet = WalletBuilder::new()
        .mint_url(mint_url.parse().expect("mint url"))
        .unit(CurrencyUnit::Sat)
        .localstore(localstore)
        .seed(seed)
        .client(connector)
        .use_http_subscription()
        .build()
        .expect("test wallet");
    fund_wallet(&wallet, 1_000, None)
        .await
        .expect("fund wallet");
    let wallet = Arc::new(wallet);

    let upstream_secret = wallet.derive_nwc_secret_key().expect("upstream key");
    let upstream_keys = Keys::parse(&upstream_secret.to_secret_hex()).expect("upstream keys");
    let upstream_client_secret = SecretKey::generate();
    let upstream_service = NwcService::new(NwcServiceConfig {
        service_keys: upstream_keys,
        client_secret: upstream_client_secret,
        relays: vec![relay.url.clone()],
        lud16: None,
    })
    .expect("upstream service");
    let upstream_uri = upstream_service.connection_uri().to_string();
    let upstream_pubkey = upstream_service.service_pubkey();
    let upstream_cancel = CancellationToken::new();
    let upstream_task_cancel = upstream_cancel.clone();
    let upstream_handler = Arc::new(WalletNwcHandler::new(wallet.clone(), None));
    let upstream_task = tokio::spawn(async move {
        let _ = upstream_service
            .run(upstream_handler, upstream_task_cancel)
            .await;
    });
    assert!(
        wait_for_info_event(&relay.url, upstream_pubkey, Duration::from_secs(15)).await,
        "upstream info event not found"
    );

    let forwarding = Arc::new(
        ForwardingNwcHandler::from_uri(&upstream_uri, Duration::from_secs(30))
            .expect("forwarding handler"),
    );
    let ledger_path = std::env::temp_dir().join(format!(
        "forwarding-policy-e2e-{}.json",
        uuid::Uuid::new_v4()
    ));
    let policy_handler =
        Arc::new(PolicyNwcHandler::new(&ledger_path, forwarding).expect("policy handler"));
    let downstream_client_secret = SecretKey::generate();
    let downstream_pubkey = Keys::new(downstream_client_secret.clone()).public_key();
    policy_handler
        .add_connection(policy(downstream_pubkey))
        .expect("downstream policy");

    let downstream_service_secret = SecretKey::generate();
    let downstream_service = NwcService::new(NwcServiceConfig {
        service_keys: Keys::new(downstream_service_secret),
        client_secret: downstream_client_secret,
        relays: vec![relay.url.clone()],
        lud16: None,
    })
    .expect("downstream service");
    let downstream_uri = downstream_service.connection_uri().to_string();
    let downstream_pubkey_service = downstream_service.service_pubkey();
    let downstream_cancel = CancellationToken::new();
    let downstream_task_cancel = downstream_cancel.clone();
    let downstream_task = tokio::spawn(async move {
        let _ = downstream_service
            .run(policy_handler, downstream_task_cancel)
            .await;
    });
    assert!(
        wait_for_info_event(
            &relay.url,
            downstream_pubkey_service,
            Duration::from_secs(15)
        )
        .await,
        "downstream info event not found"
    );

    let parsed_uri: NostrWalletConnectUri = downstream_uri.parse().expect("downstream uri");
    let client = NostrWalletConnect::builder(parsed_uri)
        .timeout(Duration::from_secs(30))
        .build();
    let balance = client.get_balance().await.expect("forwarded get_balance");
    assert_eq!(balance.balance, 1_000_000);

    let fake_invoice = create_fake_invoice(100_000, "forwarded payment".to_owned());
    let paid = client
        .pay_invoice(PayInvoiceRequest {
            id: None,
            invoice: fake_invoice.to_string(),
            amount: None,
        })
        .await
        .expect("forwarded pay_invoice");
    assert!(paid.fees_paid.is_some());

    upstream_cancel.cancel();
    downstream_cancel.cancel();
    let _ = upstream_task.await;
    let _ = downstream_task.await;
    let _ = std::fs::remove_file(&ledger_path);
}

#[tokio::test]
async fn policy_nwc_e2e_tracks_input_fees_in_budget() {
    setup_tracing();
    let relay = TestRelay::start().await;

    // 1000 ppk = 1 sat input fee per proof spent in swap or melt.
    let mint = create_and_start_in_memory_test_mint_with_fee(1_000)
        .await
        .expect("in-memory test mint with fee");
    let mint_info = mint.mint_info().await.expect("mint info");
    let mint_url = mint_info
        .urls
        .as_ref()
        .and_then(|urls| urls.first())
        .expect("mint url")
        .clone();
    let connector = DirectMintConnection::new(mint.clone());
    let localstore: Arc<dyn WalletDatabase<cdk_database::Error> + Send + Sync> = Arc::new(
        cdk_sqlite::wallet::memory::empty()
            .await
            .expect("wallet database"),
    );
    let seed = bip39::Mnemonic::generate(12)
        .expect("mnemonic")
        .to_seed_normalized("");
    let wallet = WalletBuilder::new()
        .mint_url(mint_url.parse().expect("mint url"))
        .unit(CurrencyUnit::Sat)
        .localstore(localstore)
        .seed(seed)
        .client(connector)
        .use_http_subscription()
        .build()
        .expect("test wallet");
    // Fund with one 1_000-sat mint quote so a smaller melt requires a pre-melt swap
    // or multi-proof spend that incurs input fees.
    fund_wallet(&wallet, 1_000, None)
        .await
        .expect("fund wallet");
    let wallet = Arc::new(wallet);
    let balance_before_sat: u64 = wallet.total_balance().await.expect("balance before").into();

    let service_secret_key = wallet.derive_nwc_secret_key().expect("nwc key");
    let service_keys = Keys::parse(&service_secret_key.to_secret_hex()).expect("service keys");
    let client_secret = SecretKey::generate();
    let client_pubkey = Keys::new(client_secret.clone()).public_key();
    let ledger_path =
        std::env::temp_dir().join(format!("nwc-policy-fee-e2e-{}.json", uuid::Uuid::new_v4()));

    let wallet_handler = Arc::new(WalletNwcHandler::new(wallet.clone(), None));
    let handler =
        Arc::new(PolicyNwcHandler::new(&ledger_path, wallet_handler).expect("policy handler"));
    let mut fee_policy = policy(client_pubkey);
    fee_policy.id = "e2e-fee-app".to_owned();
    fee_policy.max_fee_reserve_msat = 50_000;
    handler.add_connection(fee_policy).expect("policy");

    let service = NwcService::new(NwcServiceConfig {
        service_keys,
        client_secret,
        relays: vec![relay.url.clone()],
        lud16: None,
    })
    .expect("nwc service");
    let uri = service.connection_uri().to_string();
    let service_pubkey = service.service_pubkey();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task_handler = handler.clone();
    let service_task = tokio::spawn(async move {
        let _ = service.run(task_handler, task_cancel).await;
    });

    assert!(
        wait_for_info_event(&relay.url, service_pubkey, Duration::from_secs(15)).await,
        "service info event not found"
    );

    let parsed_uri: NostrWalletConnectUri = uri.parse().expect("nwc uri");
    let nwc_client = NostrWalletConnect::builder(parsed_uri)
        .timeout(Duration::from_secs(30))
        .build();

    // Odd amount (123 sats = 123_000 msat) forces proof selection / pre-melt swap with input_fee_ppk=1000.
    let fake_invoice = create_fake_invoice(123_000, "fee e2e payment".to_owned());
    let paid = nwc_client
        .pay_invoice(PayInvoiceRequest {
            id: None,
            invoice: fake_invoice.to_string(),
            amount: None,
        })
        .await
        .expect("pay_invoice with input fees");

    let balance_after_sat: u64 = wallet.total_balance().await.expect("balance after").into();
    let true_delta_msat = (balance_before_sat - balance_after_sat) * 1_000;
    let fees_paid_msat = paid.fees_paid.expect("fees_paid reported");
    assert!(
        fees_paid_msat > 0,
        "expected non-zero fee including mint input_fee_ppk"
    );
    assert_eq!(
        123_000 + fees_paid_msat,
        true_delta_msat,
        "reported invoice + fees_paid must equal true wallet balance delta"
    );
    assert_eq!(
        handler.used_msat("e2e-fee-app").expect("used budget"),
        true_delta_msat,
        "budget used_msat must match true wallet balance delta including input_fee_ppk"
    );

    cancel.cancel();
    let _ = service_task.await;
    let _ = std::fs::remove_file(&ledger_path);
}
