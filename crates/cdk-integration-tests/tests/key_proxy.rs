//! `key proxy`: every API key pays per request from its own Cashu pocket.
//!
//! An in-process mint with input fees (100 ppk, like cashu.mutinynet.com),
//! a fake Routstr provider that takes `X-Cashu`, charges 1 sat and returns
//! change in the `X-Cashu` response header, and the proxy router. Each rule
//! the proxy enforces has an assertion here, with the ecash it moved.

#![allow(missing_docs)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use cdk::amount::SplitTarget;
use cdk::cdk_database::{self, WalletDatabase};
use cdk::nuts::CurrencyUnit;
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet, WalletBuilder};
use cdk::{Amount, Mint};
use cdk_integration_tests::init_pure_tests::{
    create_and_start_in_memory_test_mint_with_fee, fund_wallet, DirectMintConnection,
};
use cdk_nwc_pocket::{
    derive_pocket_seed, transfer_to_pocket, AutoRefill, PocketConfig, PocketRegistry, RefillPolicy,
};
use cdk_nwc_policy::{BudgetPeriod, ConnectionPolicy, PolicyStore};
use cdk_nwc_policy_cli::ecash::{bundle_for, take_exact, take_offline};
use cdk_nwc_policy_cli::key::{api_key_digest, save_key, KeyMeta};
use cdk_nwc_policy_cli::proxy::{Proxy, ProxyConfig};
use serde_json::{json, Value};

const FEE_PPK: u64 = 100;
/// What the fake provider keeps per paid request.
const PROVIDER_CHARGE: u64 = 1;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random_seed() -> [u8; 64] {
    bip39::Mnemonic::generate(12)
        .expect("mnemonic")
        .to_seed_normalized("")
}

async fn mint_url(mint: &Arc<Mint>) -> String {
    mint.mint_info()
        .await
        .expect("mint info")
        .urls
        .and_then(|urls| urls.first().cloned())
        .expect("mint url")
}

async fn wallet_on(mint: &Arc<Mint>, seed: [u8; 64]) -> Arc<Wallet> {
    let localstore: Arc<dyn WalletDatabase<cdk_database::Error> + Send + Sync> = Arc::new(
        cdk_sqlite::wallet::memory::empty()
            .await
            .expect("wallet database"),
    );
    Arc::new(
        WalletBuilder::new()
            .mint_url(mint_url(mint).await.parse().expect("mint url"))
            .unit(CurrencyUnit::Sat)
            .localstore(localstore)
            .seed(seed)
            .client(DirectMintConnection::new(Arc::clone(mint)))
            .build()
            .expect("wallet"),
    )
}

async fn sats(wallet: &Wallet) -> u64 {
    u64::from(wallet.total_balance().await.expect("balance"))
}

// ---------------------------------------------------------------- provider

struct Provider {
    wallet: Arc<Wallet>,
    paid: std::sync::Mutex<u64>,
}

fn models_body() -> Value {
    json!({ "object": "list", "data": [
        { "id": "mini", "sats_pricing": { "max_cost": 44.1 } },
        { "id": "big", "sats_pricing": { "max_cost": 735.0 } },
        { "id": "ghost" },
    ]})
}

async fn provider_models() -> Json<Value> {
    Json(models_body())
}

/// Like routstr-core's X-Cashu flow: 402 (token untouched) below max cost,
/// otherwise redeem, bill, and return `face value - cost` as change.
async fn provider_chat(
    State(provider): State<Arc<Provider>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let model = body["model"].as_str().unwrap_or_default().to_owned();
    let max_cost = match model.as_str() {
        "mini" => 45,
        "big" => 735,
        _ => return (StatusCode::NOT_FOUND, "no model").into_response(),
    };
    let Some(token) = headers.get("x-cashu").and_then(|v| v.to_str().ok()) else {
        return (StatusCode::UNAUTHORIZED, "no token").into_response();
    };
    let face = cdk_nwc_policy_cli::ecash::token_value_sat(token).expect("token");
    if face < max_cost {
        return (StatusCode::PAYMENT_REQUIRED, "token below max cost").into_response();
    }
    provider
        .wallet
        .receive(token, ReceiveOptions::default())
        .await
        .expect("provider redeems");
    *provider.paid.lock().unwrap() += PROVIDER_CHARGE;
    let change = provider
        .wallet
        .prepare_send(Amount::from(face - PROVIDER_CHARGE), SendOptions::default())
        .await
        .expect("change")
        .confirm(None)
        .await
        .expect("change token")
        .to_string();
    let text = format!(
        "echo: {}",
        body["messages"][0]["content"].as_str().unwrap_or("")
    );
    let mut response = if body["stream"].as_bool() == Some(true) {
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":text}}]}),
            json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        );
        ([("content-type", "text/event-stream")], sse).into_response()
    } else {
        Json(json!({"object":"chat.completion","model":model,
            "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}]}))
        .into_response()
    };
    response
        .headers_mut()
        .insert("x-cashu", change.parse().expect("header"));
    response
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    format!("http://{addr}")
}

// ---------------------------------------------------------------- keys

struct Key {
    api_key: String,
    pocket: Arc<Wallet>,
}

struct Rules<'a> {
    id: &'a str,
    models: &'a [&'a str],
    cap: Option<u64>,
    fund: u64,
    refill_cap: u64,
    refill_target: u64,
    expires_at: Option<u64>,
}

struct Lab {
    _dir: tempfile::TempDir,
    mint: Arc<Mint>,
    main: Arc<Wallet>,
    provider: Arc<Provider>,
    proxy: Proxy,
    url: String,
    policy_path: std::path::PathBuf,
    keys: HashMap<String, Key>,
    http: reqwest::Client,
}

async fn lab(mint: Arc<Mint>, main_sats: u64, rules: &[Rules<'_>]) -> Lab {
    let mint_handle = Arc::clone(&mint);
    let dir = tempfile::tempdir().expect("tempdir");
    let main_seed = random_seed();
    let main = wallet_on(&mint, main_seed).await;
    fund_wallet(&main, main_sats, None)
        .await
        .expect("fund main");
    let provider_wallet = wallet_on(&mint, random_seed()).await;
    fund_wallet(&provider_wallet, 2_000, None)
        .await
        .expect("fund provider float");
    let provider = Arc::new(Provider {
        wallet: provider_wallet,
        paid: std::sync::Mutex::new(0),
    });
    let provider_url = serve(
        Router::new()
            .route("/v1/models", get(provider_models))
            .route("/v1/chat/completions", post(provider_chat))
            .with_state(Arc::clone(&provider)),
    )
    .await;

    let policy_path = dir.path().join("policy.json");
    let pockets_path = dir.path().join("policy.pockets.json");
    let keys_path = dir.path().join("policy.keys.json");
    let registry = PocketRegistry::new(&pockets_path).expect("registry");
    let mut store = PolicyStore::new(&policy_path).expect("policy");
    let mut keys = HashMap::new();
    let mut pockets = HashMap::new();
    for (index, rule) in rules.iter().enumerate() {
        let index = index as u32;
        let pocket = wallet_on(&mint, derive_pocket_seed(&main_seed, index)).await;
        if rule.fund > 0 {
            transfer_to_pocket(&main, &pocket, Amount::from(rule.fund), None)
                .await
                .expect("fund pocket");
        }
        store
            .add_connection(ConnectionPolicy {
                id: rule.id.into(),
                name: rule.id.into(),
                client_pubkey: format!("{index:064x}"),
                allowed_methods: vec![],
                budget_msat: None,
                budget_period: BudgetPeriod::Never,
                max_single_payment_msat: None,
                max_fee_reserve_msat: 0,
                fee_overage_msat: 0,
                expires_at: rule.expires_at,
                revoked: false,
                used_msat: 0,
                period_start_secs: 0,
            })
            .expect("policy");
        registry
            .add(PocketConfig {
                connection_id: rule.id.into(),
                index,
                ceiling_msat: 1_000_000,
                refill: RefillPolicy {
                    cap_msat: rule.refill_cap * 1_000,
                    window_secs: 86_400,
                    auto_refill: (rule.refill_cap > 0).then_some(AutoRefill {
                        threshold_msat: 0,
                        target_msat: rule.refill_target * 1_000,
                    }),
                },
                swept_at: None,
            })
            .expect("pocket");
        let api_key = format!("sk-pocket-test-{}", rule.id);
        save_key(
            &keys_path,
            KeyMeta {
                key_id: rule.id.into(),
                label: rule.id.into(),
                allowed_models: rule.models.iter().map(|m| (*m).to_owned()).collect(),
                max_per_payment_sats: rule.cap,
                created_at: now(),
                api_key_sha256: Some(api_key_digest(&api_key)),
            },
        )
        .expect("key");
        pockets.insert(index, Arc::clone(&pocket));
        keys.insert(rule.id.to_owned(), Key { api_key, pocket });
    }
    let proxy = Proxy::new(
        ProxyConfig {
            provider: provider_url,
            policy_path: policy_path.clone(),
            pockets_path,
            keys_path,
            mint_url: mint_url(&mint).await,
            wallet_db: dir.path().join("unused.sqlite"),
            seed: main_seed,
            sweep_interval: Duration::from_secs(3600),
        },
        Arc::clone(&main),
        pockets,
    )
    .expect("proxy");
    let url = serve(proxy.router()).await;
    Lab {
        _dir: dir,
        mint: mint_handle,
        main,
        provider,
        proxy,
        url,
        policy_path,
        keys,
        http: reqwest::Client::new(),
    }
}

struct Reply {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> u64 {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("header {name}"))
    }

    fn code(&self) -> String {
        serde_json::from_str::<Value>(&self.body).expect("json")["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

impl Lab {
    fn pocket(&self, key: &str) -> &Arc<Wallet> {
        &self.keys[key].pocket
    }

    async fn post(&self, key: Option<&str>, path: &str, body: Value) -> Reply {
        let mut request = self.http.post(format!("{}{path}", self.url)).json(&body);
        if let Some(key) = key {
            let api_key = self
                .keys
                .get(key)
                .map(|k| k.api_key.clone())
                .unwrap_or_else(|| key.to_owned());
            request = request.bearer_auth(api_key);
        }
        let response = request.send().await.expect("request");
        Reply {
            status: response.status().as_u16(),
            headers: response.headers().clone(),
            body: response.text().await.expect("body"),
        }
    }

    async fn chat(&self, key: &str, model: &str) -> Reply {
        self.post(
            Some(key),
            "/v1/chat/completions",
            json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }] }),
        )
        .await
    }

    async fn pocket_sats(&self) -> u64 {
        let mut total = 0;
        for key in self.keys.values() {
            total += sats(&key.pocket).await;
        }
        total
    }

    fn provider_paid(&self) -> u64 {
        *self.provider.paid.lock().unwrap()
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread")]
async fn key_proxy_enforces_every_rule_and_books_balance() {
    let mint = create_and_start_in_memory_test_mint_with_fee(FEE_PPK)
        .await
        .expect("mint");
    let lab = lab(
        mint,
        1_000,
        &[
            Rules {
                id: "a",
                models: &["mini", "big", "ghost"],
                cap: Some(60),
                fund: 100,
                refill_cap: 60,
                refill_target: 100,
                expires_at: None,
            },
            Rules {
                id: "b",
                models: &["mini"],
                cap: Some(50),
                fund: 60,
                refill_cap: 0,
                refill_target: 0,
                expires_at: None,
            },
            Rules {
                id: "c",
                models: &[],
                cap: None,
                fund: 30,
                refill_cap: 0,
                refill_target: 0,
                expires_at: Some(now().saturating_sub(1)),
            },
        ],
    )
    .await;
    let start_total =
        sats(&lab.main).await + lab.pocket_sats().await + sats(&lab.provider.wallet).await;
    let b_start = sats(lab.pocket("b")).await;

    // Auth.
    let reply = lab
        .post(None, "/v1/chat/completions", json!({"model":"mini"}))
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (401, "invalid_api_key")
    );
    let reply = lab.chat("sk-pocket-forged", "mini").await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (401, "invalid_api_key")
    );

    // /v1/models is filtered by the key's allow-list.
    let models: Value = lab
        .http
        .get(format!("{}/v1/models", lab.url))
        .bearer_auth(&lab.keys["b"].api_key)
        .send()
        .await
        .expect("models")
        .json()
        .await
        .expect("json");
    assert_eq!(models["data"].as_array().expect("data").len(), 1);

    // Pay per request: exact bundle out, change back, provider kept 1.
    let before = sats(lab.pocket("a")).await;
    let reply = lab.chat("a", "mini").await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.body.contains("echo: hi"));
    assert_eq!(reply.header("x-pocket-price-sat"), 45);
    assert!(reply.header("x-pocket-change-sat") >= 40);
    let cost = reply.header("x-pocket-cost-sat");
    assert_eq!(before - sats(lab.pocket("a")).await, cost);
    assert_eq!(lab.provider_paid(), PROVIDER_CHARGE);
    // Isolation: B did not move.
    assert_eq!(sats(lab.pocket("b")).await, b_start);

    // The re-bundle cut bundles ahead: the next request sends with no swap,
    // so it costs only the provider's 1 sat + its input fee + the change fee.
    let reply = lab.chat("a", "mini").await;
    assert_eq!(reply.status, 200);
    assert!(
        reply.header("x-pocket-cost-sat") <= PROVIDER_CHARGE + 2,
        "swap-free request cost {}",
        reply.header("x-pocket-cost-sat")
    );

    // Streaming passes through, change still lands.
    let reply = lab
        .post(
            Some("a"),
            "/v1/chat/completions",
            json!({ "model": "mini", "stream": true, "messages": [{"role":"user","content":"s"}] }),
        )
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.headers["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    assert!(reply.body.contains("echo: s") && reply.body.contains("[DONE]"));
    assert_eq!(reply.headers["x-pocket-change"], "received");

    // Refusals happen before any ecash moves.
    let a_before = sats(lab.pocket("a")).await;
    let b_before = sats(lab.pocket("b")).await;
    let reply = lab.chat("a", "big").await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (402, "over_request_cap")
    );
    let reply = lab.chat("b", "big").await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (403, "model_not_allowed")
    );
    assert!(reply.body.contains("allowed: mini"));
    let reply = lab.chat("a", "ghost").await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (404, "model_not_found")
    );
    let reply = lab.chat("c", "mini").await;
    assert_eq!((reply.status, reply.code().as_str()), (401, "key_expired"));
    assert_eq!(sats(lab.pocket("a")).await, a_before);
    assert_eq!(sats(lab.pocket("b")).await, b_before);

    // Raw tokens for a client that pays the provider itself (routstrd).
    let reply = lab
        .post(Some("a"), "/v1/pocket/send", json!({ "amount_sat": 45 }))
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (403, "model_required")
    );
    let reply = lab
        .post(
            Some("a"),
            "/v1/pocket/send",
            json!({ "amount_sat": 61, "model": "mini" }),
        )
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (402, "over_request_cap")
    );
    let reply = lab
        .post(
            Some("a"),
            "/v1/pocket/send",
            json!({ "amount_sat": 45, "model": "mini" }),
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let token = serde_json::from_str::<Value>(&reply.body).expect("json")["token"]
        .as_str()
        .expect("token")
        .to_owned();
    let reply = lab
        .post(Some("a"), "/v1/pocket/receive", json!({ "token": token }))
        .await;
    assert_eq!(reply.status, 200);
    assert!(
        serde_json::from_str::<Value>(&reply.body).expect("json")["received_sat"]
            .as_u64()
            .expect("received")
            >= 44
    );

    // A token handed out and never used: revoke must make it worthless.
    let reply = lab
        .post(
            Some("a"),
            "/v1/pocket/send",
            json!({ "amount_sat": 45, "model": "mini" }),
        )
        .await;
    assert_eq!(reply.status, 200);
    let outstanding = serde_json::from_str::<Value>(&reply.body).expect("json")["token"]
        .as_str()
        .expect("token")
        .to_owned();

    // Budget: B has no refill and runs out with a clean 402.
    let mut b_paid = 0;
    let last = loop {
        let reply = lab.chat("b", "mini").await;
        if reply.status != 200 {
            break reply;
        }
        b_paid += 1;
        assert!(b_paid < 30, "budget never ran out");
    };
    assert_eq!(
        (last.status, last.code().as_str()),
        (402, "budget_exhausted")
    );
    assert!(b_paid >= 2);
    let b_left = sats(lab.pocket("b")).await;
    // Exhausted: no set of B's proofs is worth 45 after the input fee.
    let b_proofs = lab.pocket("b").get_unspent_proofs().await.expect("proofs");
    let b_fee = (b_proofs.len() as u64 * FEE_PPK).div_ceil(1000);
    assert!(
        b_left.saturating_sub(b_fee) < 45,
        "b_left {b_left}: {}",
        last.body
    );

    // Refill cap: A refills from main, within 60 sat, then stops.
    let main_before = sats(&lab.main).await;
    let mut a_paid = 0;
    let last = loop {
        let reply = lab.chat("a", "mini").await;
        if reply.status != 200 {
            break reply;
        }
        a_paid += 1;
        assert!(a_paid < 60, "refill cap never reached");
    };
    assert_eq!(
        (last.status, last.code().as_str()),
        (402, "budget_exhausted")
    );
    let refilled_cost = main_before - sats(&lab.main).await;
    assert!(
        refilled_cost > 0 && refilled_cost <= 60,
        "refill cost {refilled_cost}"
    );

    // Revoke A and expire C: refused, then swept home by the sweeper.
    PolicyStore::new(&lab.policy_path)
        .expect("policy")
        .revoke("a")
        .expect("revoke");
    let reply = lab.chat("a", "mini").await;
    assert_eq!((reply.status, reply.code().as_str()), (401, "key_revoked"));
    let main_before = sats(&lab.main).await;
    lab.proxy.sweep_once().await.expect("sweep");
    assert_eq!(sats(lab.pocket("a")).await, 0);
    assert_eq!(sats(lab.pocket("c")).await, 0);
    assert!(sats(&lab.main).await > main_before);
    // The handed-out token was swapped home on revoke.
    let stranger = wallet_on(&lab.mint, random_seed()).await;
    assert!(matches!(
        stranger
            .receive(&outstanding, ReceiveOptions::default())
            .await,
        Err(cdk::Error::TokenAlreadySpent)
    ));
    // B (still active) keeps its remainder.
    assert_eq!(sats(lab.pocket("b")).await, b_left);

    // Books: everything that left the wallets is the provider's revenue
    // (1 sat per paid request) plus mint input fees.
    let paid_requests = 3 + b_paid + a_paid;
    assert_eq!(lab.provider_paid(), paid_requests * PROVIDER_CHARGE);
    let end_total =
        sats(&lab.main).await + lab.pocket_sats().await + sats(&lab.provider.wallet).await;
    let fees = start_total - end_total;
    // Per request: the provider's redeem fee (inside our token) + our change
    // swap + the provider's own change send; plus a few re-bundles, refills
    // and sweeps. Without bundles every send also swapped.
    assert!(
        fees <= paid_requests * 3 + 20,
        "fees {fees} for {paid_requests} requests"
    );
    // Strict: every sat the mint has outstanding is in one of the wallets.
    // Nothing parked, nothing in flight, nothing lost.
    let issued: u64 = lab
        .mint
        .total_issued()
        .await
        .expect("issued")
        .values()
        .map(|a| u64::from(*a))
        .sum();
    let redeemed: u64 = lab
        .mint
        .total_redeemed()
        .await
        .expect("redeemed")
        .values()
        .map(|a| u64::from(*a))
        .sum();
    assert_eq!(
        end_total,
        issued - redeemed,
        "wallets must hold exactly the outstanding ecash"
    );
    for key in lab.keys.values() {
        assert!(key
            .pocket
            .get_pending_spent_proofs()
            .await
            .expect("pending")
            .is_empty());
    }
    println!(
        "books: start {start_total} = end {end_total} + fees {fees}; provider revenue {} over {paid_requests} requests",
        lab.provider_paid()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bundles_send_offline_without_a_swap() {
    let mint = create_and_start_in_memory_test_mint_with_fee(FEE_PPK)
        .await
        .expect("mint");
    let wallet = wallet_on(&mint, random_seed()).await;
    fund_wallet(&wallet, 300, None).await.expect("fund");
    let bundle = bundle_for(&wallet, 45).await.expect("bundle");
    assert!(bundle.send_sat >= 45 && bundle.total_sat > bundle.send_sat);

    // Loose proofs: one re-bundle swap, cutting bundles for later requests.
    let before = sats(&wallet).await;
    let first = take_exact(&wallet, 45, 46).await.expect("take");
    let drop = before - sats(&wallet).await;
    if first.swapped {
        assert!(drop > bundle.total_sat);
    } else {
        assert_eq!(drop, bundle.total_sat);
    }

    // Then bundles go out offline: the drop is exactly one bundle.
    for _ in 0..3 {
        let before = sats(&wallet).await;
        let taken = take_offline(&wallet, 45, 46)
            .await
            .expect("offline")
            .expect("a bundle is ready");
        assert!(!taken.swapped);
        assert_eq!(taken.token_sat, bundle.total_sat);
        assert_eq!(before - sats(&wallet).await, bundle.total_sat);
    }

    // A pocket too small for a bundle is refused without a mint call.
    let small = wallet_on(&mint, random_seed()).await;
    fund_wallet(&small, 20, None).await.expect("fund");
    assert!(matches!(
        take_exact(&small, 45, 46).await,
        Err(cdk::Error::InsufficientFunds)
    ));
    assert_eq!(sats(&small).await, 20);

    // No exact bundle, but one 64 proof: sent as it is (overpaying, no swap)
    // only when its value fits the per-request cap.
    let one = wallet_on(&mint, random_seed()).await;
    fund_wallet(&one, 64, Some(SplitTarget::Values(vec![Amount::from(64)])))
        .await
        .expect("fund");
    assert!(take_offline(&one, 45, 60).await.expect("offline").is_none());
    let taken = take_offline(&one, 45, 63)
        .await
        .expect("offline")
        .expect("overpay within the cap");
    assert!(!taken.swapped);
    assert_eq!(taken.token_sat, 64);
    assert_eq!(sats(&one).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn spend_lock_is_exclusive_across_handles() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Two registries on the same file stand in for `serve` and `key proxy`.
    let serve = PocketRegistry::new(dir.path().join("p.json")).expect("registry");
    let proxy = PocketRegistry::new(dir.path().join("p.json")).expect("registry");
    let held = serve.lock_spend("k").await.expect("lock");
    let waiting = tokio::spawn(async move { proxy.lock_spend("k").await.expect("lock") });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished(), "second holder must wait");
    // Other pockets and main are independent.
    let _other = serve.lock_spend("other").await.expect("other");
    let _main = serve
        .lock_spend(cdk_nwc_pocket::MAIN_SPEND_LOCK)
        .await
        .expect("main");
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("acquired after release")
        .expect("join");
}
