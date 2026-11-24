//! Ignored live harness for the Mutinynet Cashu mint and an upstream Alby Hub
//! NWC connection.

#![allow(missing_docs)]

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::Router;
use cdk_nostr::nwc::{NwcService, NwcServiceConfig};
use cdk_nwc_forward::ForwardingNwcHandler;
use cdk_nwc_policy::{BudgetPeriod, ConnectionPolicy, PolicyNwcHandler};
use futures::{SinkExt, StreamExt};
use nostr::prelude::{Event, Filter, Keys, Kind, PublicKey, RelayUrl, SecretKey};
use nostr_sdk::prelude::Client as NostrClient;
use nwc::prelude::{NostrWalletConnect, NostrWalletConnectUri, PayInvoiceRequest};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const MINT_URL: &str = "https://cashu.mutinynet.com";

#[derive(Clone)]
struct RelayState {
    events: Arc<Mutex<Vec<Event>>>,
    tx: broadcast::Sender<Event>,
}

struct TestRelay {
    url: RelayUrl,
    task: JoinHandle<()>,
}

impl TestRelay {
    async fn start() -> Self {
        let state = RelayState {
            events: Arc::new(Mutex::new(Vec::new())),
            tx: broadcast::channel(256).0,
        };
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
            task,
        }
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
    let mut incoming = state.tx.subscribe();
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
                        let filters: Vec<Value> = request.as_array().into_iter().flatten().skip(2).cloned().collect();
                        subscriptions.insert(sub_id.to_owned(), filters.clone());
                        for event in state.events.lock().await.clone() {
                            if filters.iter().any(|filter| matches_filter(&event, filter)) {
                                let message = json!(["EVENT", sub_id, event]).to_string();
                                if sink.send(Message::Text(message.into())).await.is_err() { return; }
                            }
                        }
                        let eose = json!(["EOSE", sub_id]).to_string();
                        if sink.send(Message::Text(eose.into())).await.is_err() { return; }
                    }
                    "EVENT" => {
                        let Some(event): Option<Event> = request.get(1).and_then(|value| serde_json::from_value(value.clone()).ok()) else { continue };
                        let event_id = event.id.to_hex();
                        state.events.lock().await.push(event.clone());
                        let _ = state.tx.send(event);
                        let ok = json!(["OK", event_id, true, ""]).to_string();
                        if sink.send(Message::Text(ok.into())).await.is_err() { return; }
                    }
                    "CLOSE" => {
                        if let Some(sub_id) = request.get(1).and_then(Value::as_str) {
                            subscriptions.remove(sub_id);
                        }
                    }
                    _ => {}
                }
            }
            incoming = incoming.recv() => {
                let Ok(event) = incoming else { continue };
                for (sub_id, filters) in &subscriptions {
                    if filters.iter().any(|filter| matches_filter(&event, filter)) {
                        let message = json!(["EVENT", sub_id, event]).to_string();
                        if sink.send(Message::Text(message.into())).await.is_err() { return; }
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
    if let Some(since) = filter.get("since").and_then(Value::as_u64) {
        if value.get("created_at").and_then(Value::as_u64).unwrap_or(0) < since {
            return false;
        }
    }
    for key in ["#p", "#e"] {
        if let Some(expected) = filter.get(key).and_then(Value::as_array) {
            let tags = value.get("tags").and_then(Value::as_array);
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

async fn wait_for_info(relay: &RelayUrl, pubkey: PublicKey) -> bool {
    let client = NostrClient::new();
    let _ = client.add_relay(relay.clone()).await;
    client.connect().await;
    let filter = Filter::new().kind(Kind::WalletConnectInfo).author(pubkey);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let found = loop {
        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        match tokio::time::timeout(Duration::from_secs(3), client.fetch_events(filter.clone()))
            .await
        {
            Ok(Ok(events)) if !events.is_empty() => break true,
            _ => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    };
    client.disconnect().await;
    found
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn policy(client_pubkey: PublicKey) -> ConnectionPolicy {
    ConnectionPolicy {
        id: "mutinynet-live".to_owned(),
        name: "Mutinynet live test".to_owned(),
        client_pubkey: client_pubkey.to_hex(),
        allowed_methods: vec![
            "get_info".to_owned(),
            "get_balance".to_owned(),
            "pay_invoice".to_owned(),
        ],
        budget_msat: Some(100_000),
        budget_period: BudgetPeriod::Daily,
        max_single_payment_msat: Some(100_000),
        max_fee_reserve_msat: 10_000,
        fee_overage_msat: 0,
        expires_at: None,
        revoked: false,
        used_msat: 0,
        period_start_secs: now_secs(),
    }
}

#[tokio::test]
#[ignore = "requires ALBY_NWC_URI_FILE pointing to a 0600 file with an Alby NWC URI"]
async fn mutinynet_live_policy_spend() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let uri_path = std::env::var("ALBY_NWC_URI_FILE")
        .expect("set ALBY_NWC_URI_FILE to a 0600 file containing the Alby NWC URI");
    let upstream_uri = fs::read_to_string(&uri_path)
        .expect("read upstream URI")
        .trim()
        .to_owned();

    let relay = TestRelay::start().await;
    let forwarding = Arc::new(
        ForwardingNwcHandler::from_uri(&upstream_uri, Duration::from_secs(60))
            .expect("upstream forwarding handler"),
    );
    let suffix = now_secs();
    let ledger_path = std::env::temp_dir().join(format!("mutinynet-policy-{suffix}.json"));
    let handler =
        Arc::new(PolicyNwcHandler::new(&ledger_path, forwarding).expect("policy handler"));

    let client_secret = SecretKey::generate();
    let client_pubkey = Keys::new(client_secret.clone()).public_key();
    handler
        .add_connection(policy(client_pubkey))
        .expect("policy");

    let service = NwcService::new(NwcServiceConfig {
        service_keys: Keys::new(SecretKey::generate()),
        client_secret,
        relays: vec![relay.url.clone()],
        lud16: None,
    })
    .expect("downstream service");
    let downstream_uri = service.connection_uri().to_string();
    let service_pubkey = service.service_pubkey();
    let handler_for_revoke = handler.clone();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        let _ = service.run(handler, task_cancel).await;
    });

    assert!(
        wait_for_info(&relay.url, service_pubkey).await,
        "policy service info event not found"
    );
    let uri: NostrWalletConnectUri = downstream_uri.parse().expect("downstream uri");
    let client = NostrWalletConnect::builder(uri)
        .timeout(Duration::from_secs(60))
        .build();

    let info = client.get_info().await.expect("get_info");
    assert_eq!(info.methods.len(), 3);

    let before = client.get_balance().await.expect("get_balance before");
    assert!(
        before.balance > 0,
        "Alby wallet must have a positive balance"
    );

    let http = reqwest::Client::new();
    let quote: Value = http
        .post(format!("{MINT_URL}/v1/mint/quote/bolt11"))
        .json(&json!({"unit": "sat", "amount": 1}))
        .send()
        .await
        .expect("mint quote request")
        .json()
        .await
        .expect("mint quote json");
    let invoice = quote
        .get("request")
        .and_then(Value::as_str)
        .expect("mint quote invoice")
        .to_owned();
    let quote_id = quote
        .get("quote")
        .and_then(Value::as_str)
        .expect("mint quote id")
        .to_owned();

    let pay_result = client
        .pay_invoice(PayInvoiceRequest {
            id: None,
            invoice,
            amount: None,
        })
        .await;
    println!("pay_result_ok={}", pay_result.is_ok());
    if let Err(error) = &pay_result {
        println!("pay_error={error}");
    }

    let quote_state: Value = http
        .get(format!("{MINT_URL}/v1/mint/quote/bolt11/{quote_id}"))
        .send()
        .await
        .expect("mint quote state request")
        .json()
        .await
        .expect("mint quote state json");
    let state = quote_state
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    println!("mint_quote_state={state}");
    assert_eq!(state, "paid", "Mutinynet mint quote must be paid");

    let after = client.get_balance().await.expect("get_balance after");
    assert!(after.balance < before.balance, "balance must decrease");

    handler_for_revoke
        .revoke("mutinynet-live")
        .expect("revoke live policy");
    let revoked = client.get_balance().await;
    assert!(revoked.is_err(), "revoked connection must be denied");

    cancel.cancel();
    let _ = task.await;
    let _ = fs::remove_file(&ledger_path);
}
