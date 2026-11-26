//! `key proxy`: an OpenAI-compatible endpoint where every request is paid
//! with ecash from the calling key's own pocket.
//!
//! Per request:
//! 1. the key is found by its API key; revoked or expired keys are refused;
//! 2. the model must be on the key's allow-list;
//! 3. the price is the provider's published `sats_pricing.max_cost` for the
//!    model, and must not exceed the key's per-request cap;
//! 4. exactly that much ecash is taken from this key's pocket (refilled from
//!    main within the refill cap if needed) and sent as `X-Cashu`;
//! 5. the provider's change (`X-Cashu` response header) goes back into the
//!    same pocket.
//!
//! All checks in 1-3 happen before any ecash leaves the pocket. Keys,
//! policies and the pocket registry are re-read on every request, so new,
//! revoked and expired keys take effect without a restart.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet};
use cdk::Amount;
use cdk_nwc_pocket::{sweep_pocket, Error as PocketError, PocketRegistry, Refiller, MSAT_PER_SAT};
use cdk_nwc_policy::{ConnectionPolicy, PolicyStore};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::key::{api_key_digest, load_keys, KeyMeta};
use crate::pocket::{is_closed, open_pocket};

/// Sats kept free in the pocket for the swap fee of a send.
const SEND_FEE_MARGIN_SAT: u64 = 3;

/// Why a request was refused before any ecash left the pocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No key matches the API key.
    InvalidKey,
    /// The key was revoked.
    Revoked,
    /// The key is past its expiry.
    Expired,
    /// The model is not on the key's allow-list.
    ModelNotAllowed(String),
    /// The provider does not list the model or its price.
    UnknownModel(String),
    /// The model's max cost is above the key's per-request cap.
    OverRequestCap {
        /// Provider max cost in sats.
        price_sat: u64,
        /// The key's cap in sats.
        cap_sat: u64,
    },
    /// The pocket cannot cover the price and the refill cap is used up.
    BudgetExhausted {
        /// Pocket balance in sats.
        balance_sat: u64,
        /// Price in sats.
        price_sat: u64,
    },
}

impl Refusal {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            Self::InvalidKey => (StatusCode::UNAUTHORIZED, "invalid_api_key"),
            Self::Revoked => (StatusCode::UNAUTHORIZED, "key_revoked"),
            Self::Expired => (StatusCode::UNAUTHORIZED, "key_expired"),
            Self::ModelNotAllowed(_) => (StatusCode::FORBIDDEN, "model_not_allowed"),
            Self::UnknownModel(_) => (StatusCode::NOT_FOUND, "model_not_found"),
            Self::OverRequestCap { .. } => (StatusCode::PAYMENT_REQUIRED, "over_request_cap"),
            Self::BudgetExhausted { .. } => (StatusCode::PAYMENT_REQUIRED, "budget_exhausted"),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::InvalidKey => "invalid API key".into(),
            Self::Revoked => "key revoked".into(),
            Self::Expired => "key expired".into(),
            Self::ModelNotAllowed(model) => format!("model {model} is not allowed for this key"),
            Self::UnknownModel(model) => format!("provider has no price for model {model}"),
            Self::OverRequestCap { price_sat, cap_sat } => {
                format!("max cost {price_sat} sat is above this key's {cap_sat} sat per-request cap")
            }
            Self::BudgetExhausted {
                balance_sat,
                price_sat,
            } => format!(
                "pocket has {balance_sat} sat, request needs {price_sat} sat + fee and no refill is left"
            ),
        }
    }
}

/// Status of a key from its policy: revoked or expired keys are refused.
pub fn check_key(policy: Option<&ConnectionPolicy>, now: u64) -> Result<(), Refusal> {
    match policy {
        None => Err(Refusal::Revoked),
        Some(policy) if policy.revoked => Err(Refusal::Revoked),
        Some(policy) if is_closed(Some(policy), now) => Err(Refusal::Expired),
        Some(_) => Ok(()),
    }
}

/// The key's own request rules: allowed model and per-request cap.
pub fn check_request(meta: &KeyMeta, model: &str, price_sat: Option<u64>) -> Result<u64, Refusal> {
    if !meta.allowed_models.is_empty() && !meta.allowed_models.iter().any(|m| m == model) {
        return Err(Refusal::ModelNotAllowed(model.to_owned()));
    }
    let price_sat = price_sat.ok_or_else(|| Refusal::UnknownModel(model.to_owned()))?;
    if let Some(cap_sat) = meta.max_per_payment_sats {
        if price_sat > cap_sat {
            return Err(Refusal::OverRequestCap { price_sat, cap_sat });
        }
    }
    Ok(price_sat)
}

/// The provider's max cost for `model` in whole sats (rounded up), from an
/// OpenAI-style `/v1/models` body with Routstr `sats_pricing`.
pub fn max_cost_sat(models: &Value, model: &str) -> Option<u64> {
    let entry = models
        .get("data")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(model))?;
    let max_cost = entry.get("sats_pricing")?.get("max_cost")?.as_f64()?;
    if !max_cost.is_finite() || max_cost < 0.0 {
        return None;
    }
    Some((max_cost.ceil() as u64).max(1))
}

/// Proxy configuration.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Provider base URL, e.g. `http://127.0.0.1:8100`.
    pub provider: String,
    /// Policy ledger path.
    pub policy_path: PathBuf,
    /// Pocket registry path.
    pub pockets_path: PathBuf,
    /// Key metadata path.
    pub keys_path: PathBuf,
    /// Mint URL.
    pub mint_url: String,
    /// Main wallet database.
    pub wallet_db: PathBuf,
    /// Main wallet seed.
    pub seed: [u8; 64],
    /// Seconds between expiry/revoke sweeps.
    pub sweep_interval: Duration,
}

struct ProxyState {
    config: ProxyConfig,
    http: reqwest::Client,
    main: Arc<Wallet>,
    refiller: Refiller,
    wallets: Mutex<HashMap<u32, Arc<Wallet>>>,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Keys whose last request failed at the mint, with the time they may
    /// retry; avoids hammering a rate-limited mint with swaps.
    cooldown: Mutex<HashMap<String, u64>>,
}

/// Seconds a key waits after a mint/provider failure.
const FAILURE_COOLDOWN_SECS: u64 = 10;

impl ProxyState {
    async fn pocket(&self, index: u32) -> Result<Arc<Wallet>> {
        let mut wallets = self.wallets.lock().await;
        if let Some(wallet) = wallets.get(&index) {
            return Ok(Arc::clone(wallet));
        }
        let wallet = open_pocket(
            &self.config.mint_url,
            &self.config.wallet_db,
            &self.config.seed,
            index,
        )
        .await?;
        wallets.insert(index, Arc::clone(&wallet));
        Ok(wallet)
    }

    async fn key_lock(&self, key_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        Arc::clone(locks.entry(key_id.to_owned()).or_default())
    }

    fn registry(&self) -> Result<PocketRegistry> {
        Ok(PocketRegistry::new(&self.config.pockets_path)?)
    }

    fn policies(&self) -> Result<Vec<ConnectionPolicy>> {
        Ok(PolicyStore::new(&self.config.policy_path)?.connections()?)
    }

    async fn provider_models(&self) -> Result<Value> {
        let url = format!("{}/v1/models", self.config.provider.trim_end_matches('/'));
        Ok(self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}

/// Directory for ecash that could not be received yet (change or a reclaimed
/// token), retried on every sweep tick so a mint hiccup never loses it.
fn pending_dir(keys_path: &std::path::Path) -> PathBuf {
    let mut name = keys_path.as_os_str().to_owned();
    name.push(".pending");
    PathBuf::from(name)
}

fn park_token(keys_path: &std::path::Path, key_id: &str, token: &str) -> Result<PathBuf> {
    let dir = pending_dir(keys_path);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = dir.join(format!("{key_id}-{nanos}.token"));
    crate::key::write_private(&path, token.as_bytes())?;
    Ok(path)
}

/// Receive `token` into `pocket`; if the mint fails, park it for a retry.
async fn receive_or_park(
    state: &ProxyState,
    key_id: &str,
    pocket: &Wallet,
    token: &str,
) -> (u64, bool) {
    match pocket.receive(token, ReceiveOptions::default()).await {
        Ok(amount) => (u64::from(amount), false),
        Err(error) => {
            let parked = park_token(&state.config.keys_path, key_id, token);
            log(
                json!({ "key_id": key_id, "parked_token": parked.is_ok(), "receive_error": error.to_string() }),
            );
            (0, true)
        }
    }
}

/// Retry parked tokens for `key_id`. Returns sats received.
async fn retry_parked(state: &ProxyState, key_id: &str, pocket: &Wallet) -> u64 {
    let Ok(entries) = std::fs::read_dir(pending_dir(&state.config.keys_path)) else {
        return 0;
    };
    let mut received = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&format!("{key_id}-")) {
            continue;
        }
        let Ok(token) = std::fs::read_to_string(&path) else {
            continue;
        };
        match pocket
            .receive(token.trim(), ReceiveOptions::default())
            .await
        {
            Ok(amount) => {
                received += u64::from(amount);
                let _ = std::fs::remove_file(&path);
            }
            Err(cdk::Error::TokenAlreadySpent) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => {}
        }
    }
    received
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "error": { "message": message, "type": "pocket_error", "code": code } })),
    )
        .into_response()
}

fn refusal_response(refusal: &Refusal) -> Response {
    let (status, code) = refusal.status_and_code();
    error_response(status, code, &refusal.message())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

fn find_key(state: &ProxyState, headers: &HeaderMap) -> Result<KeyMeta, Refusal> {
    let api_key = bearer(headers).ok_or(Refusal::InvalidKey)?;
    let digest = api_key_digest(api_key);
    load_keys(&state.config.keys_path)
        .map_err(|_| Refusal::InvalidKey)?
        .into_iter()
        .find(|meta| meta.api_key_sha256.as_deref() == Some(digest.as_str()))
        .ok_or(Refusal::InvalidKey)
}

fn log(event: Value) {
    println!("{event}");
}

async fn models(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    let meta = match find_key(&state, &headers) {
        Ok(meta) => meta,
        Err(refusal) => return refusal_response(&refusal),
    };
    match state.provider_models().await {
        Ok(mut body) => {
            if let Some(data) = body.get_mut("data").and_then(Value::as_array_mut) {
                data.retain(|entry| {
                    meta.allowed_models.is_empty()
                        || entry
                            .get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| meta.allowed_models.iter().any(|m| m == id))
                });
            }
            Json(body).into_response()
        }
        Err(error) => error_response(
            StatusCode::BAD_GATEWAY,
            "provider_unreachable",
            &error.to_string(),
        ),
    }
}

async fn pocket_info(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    let meta = match find_key(&state, &headers) {
        Ok(meta) => meta,
        Err(refusal) => return refusal_response(&refusal),
    };
    let result: Result<Value> = async {
        let registry = state.registry()?;
        let config = registry.get(&meta.key_id)?;
        let pocket = state.pocket(config.index).await?;
        let now = now_secs();
        let policies = state.policies()?;
        let policy = policies.iter().find(|p| p.id == meta.key_id);
        Ok(json!({
            "key_id": meta.key_id,
            "label": meta.label,
            "status": match check_key(policy, now) {
                Ok(()) => "active",
                Err(Refusal::Expired) => "expired",
                Err(_) => "revoked",
            },
            "balance_sat": u64::from(pocket.total_balance().await?),
            "refill_remaining_sat": registry.remaining_msat(&meta.key_id, now)? / MSAT_PER_SAT,
            "max_per_request_sat": meta.max_per_payment_sats,
            "allowed_models": meta.allowed_models,
            "expires_at": policy.and_then(|p| p.expires_at),
        }))
    }
    .await;
    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &error.to_string(),
        ),
    }
}

async fn forward(
    State(state): State<Arc<ProxyState>>,
    AxumPath(rest): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let meta = match find_key(&state, &headers) {
        Ok(meta) => meta,
        Err(refusal) => return refusal_response(&refusal),
    };
    let model = match serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_owned))
    {
        Some(model) => model,
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "body needs a model",
            )
        }
    };
    let lock = state.key_lock(&meta.key_id).await;
    let _guard = lock.lock().await;
    if let Some(until) = state.cooldown.lock().await.get(&meta.key_id).copied() {
        let now = now_secs();
        if now < until {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "mint_busy",
                &format!(
                    "last request failed at the mint; retry in {} s",
                    until - now
                ),
            );
        }
    }
    match pay_and_forward(&state, &meta, &model, &rest, body).await {
        Ok(response) => response,
        Err(Outcome::Refused(refusal)) => {
            log(json!({
                "key": meta.label, "model": model, "refused": refusal.status_and_code().1,
                "reason": refusal.message(), "ecash_sent": 0,
            }));
            refusal_response(&refusal)
        }
        Err(Outcome::Failed(error)) => {
            // A mint error mid-swap can leave proofs reserved: recover them.
            state
                .cooldown
                .lock()
                .await
                .insert(meta.key_id.clone(), now_secs() + FAILURE_COOLDOWN_SECS);
            let recovered = recover_pocket(&state, &meta.key_id, false).await;
            log(
                json!({ "key": meta.label, "model": model, "error": format!("{error:#}"),
                "recovered": recovered }),
            );
            error_response(
                StatusCode::BAD_GATEWAY,
                "pocket_proxy_error",
                &format!("{error:#}"),
            )
        }
    }
}

enum Outcome {
    Refused(Refusal),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for Outcome {
    fn from(error: anyhow::Error) -> Self {
        Self::Failed(error)
    }
}

impl From<PocketError> for Outcome {
    fn from(error: PocketError) -> Self {
        Self::Failed(error.into())
    }
}

impl From<cdk::Error> for Outcome {
    fn from(error: cdk::Error) -> Self {
        Self::Failed(error.into())
    }
}

async fn pay_and_forward(
    state: &ProxyState,
    meta: &KeyMeta,
    model: &str,
    rest: &str,
    body: Bytes,
) -> Result<Response, Outcome> {
    let now = now_secs();
    // 1. key status
    let policies = state.policies()?;
    check_key(policies.iter().find(|p| p.id == meta.key_id), now).map_err(Outcome::Refused)?;
    let registry = state.registry()?;
    let config = registry.get(&meta.key_id)?;
    if config.swept_at.is_some() {
        return Err(Outcome::Refused(Refusal::Revoked));
    }
    // 2-3. model and price rules, before any ecash moves
    if !meta.allowed_models.is_empty() && !meta.allowed_models.iter().any(|m| m == model) {
        return Err(Outcome::Refused(Refusal::ModelNotAllowed(model.to_owned())));
    }
    let models = state.provider_models().await?;
    let price_sat =
        check_request(meta, model, max_cost_sat(&models, model)).map_err(Outcome::Refused)?;

    // 4. budget: pocket balance, refilled from main within the refill cap.
    // 5. take exact ecash from this pocket only.
    let pocket = state.pocket(config.index).await?;
    let before_sat = u64::from(pocket.total_balance().await?);
    let mut refilled = false;
    let mut refilled_sat = 0;
    let prepared = loop {
        let balance_sat = u64::from(pocket.total_balance().await?);
        let attempt = if balance_sat >= price_sat + SEND_FEE_MARGIN_SAT {
            match pocket
                .prepare_send(
                    Amount::from(price_sat),
                    SendOptions {
                        include_fee: true,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(prepared) => Some(prepared),
                Err(cdk::Error::InsufficientFunds) => None,
                Err(error) => return Err(error.into()),
            }
        } else {
            None
        };
        if let Some(prepared) = attempt {
            break prepared;
        }
        // Short: refill once from main, within the refill cap.
        let refused = Refusal::BudgetExhausted {
            balance_sat,
            price_sat,
        };
        let Some(auto) = config.refill.auto_refill.filter(|_| !refilled) else {
            return Err(Outcome::Refused(refused));
        };
        refilled = true;
        let need_msat = (price_sat + 2 * SEND_FEE_MARGIN_SAT) * MSAT_PER_SAT;
        let wanted_msat = need_msat
            .max(auto.target_msat)
            .saturating_sub(balance_sat * MSAT_PER_SAT);
        match state
            .refiller
            .refill(&meta.key_id, &pocket, wanted_msat, now)
            .await
        {
            Ok(added) if added > 0 => {
                refilled_sat = added / MSAT_PER_SAT;
                log(
                    json!({ "key": meta.label, "refilled_sat": added / MSAT_PER_SAT,
                    "refill_remaining_sat": registry.remaining_msat(&meta.key_id, now)? / MSAT_PER_SAT }),
                );
            }
            Ok(_) | Err(PocketError::RefillCapReached) => return Err(Outcome::Refused(refused)),
            Err(error) => return Err(error.into()),
        }
    };
    let token = prepared.confirm(None).await?.to_string();

    // 6. forward with the token
    let url = format!("{}/v1/{rest}", state.config.provider.trim_end_matches('/'));
    let sent = state
        .http
        .post(url)
        .header("content-type", "application/json")
        .header("x-cashu", &token)
        .body(body)
        .send()
        .await;
    let (status, content_type, change_token, reply) = match sent {
        Ok(response) => {
            let status = response.status();
            let content_type = response.headers().get("content-type").cloned();
            let change = response
                .headers()
                .get("x-cashu")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let reply = response.bytes().await.unwrap_or_default();
            (status.as_u16(), content_type, change, reply)
        }
        Err(error) => {
            // Never reached the provider: take the ecash back.
            let (reclaimed_sat, parked) =
                receive_or_park(state, &meta.key_id, &pocket, &token).await;
            log(
                json!({ "key": meta.label, "model": model, "provider_unreachable": error.to_string(),
                "reclaimed_sat": reclaimed_sat, "parked": parked }),
            );
            return Err(Outcome::Failed(anyhow::anyhow!(
                "provider unreachable: {error}"
            )));
        }
    };

    // 7. change back into the same pocket
    let mut change_sat = 0;
    let mut reclaimed = false;
    let mut parked = false;
    if let Some(change) = &change_token {
        (change_sat, parked) = receive_or_park(state, &meta.key_id, &pocket, change).await;
    } else if !(200..300).contains(&status) {
        // A refusal before redemption leaves the token unspent: take it back.
        if let Ok(amount) = pocket.receive(&token, ReceiveOptions::default()).await {
            change_sat = u64::from(amount);
            reclaimed = true;
        }
    }
    let after_sat = u64::from(pocket.total_balance().await?);
    let net_sat = (before_sat + refilled_sat).saturating_sub(after_sat);
    log(json!({
        "key": meta.label, "model": model, "status": status, "price_sat": price_sat,
        "sent_sat": price_sat, "change_sat": change_sat, "reclaimed": reclaimed, "change_parked": parked,
        "net_spent_sat": net_sat, "refilled_sat": refilled_sat, "balance_before_sat": before_sat, "balance_after_sat": after_sat,
    }));

    let mut response = Response::new(axum::body::Body::from(reply));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = response.headers_mut();
    if let Some(content_type) = content_type {
        if let Ok(value) = HeaderValue::from_bytes(content_type.as_bytes()) {
            headers.insert("content-type", value);
        }
    }
    for (name, value) in [
        ("x-pocket-price-sat", price_sat),
        ("x-pocket-change-sat", change_sat),
        ("x-pocket-net-sat", net_sat),
        ("x-pocket-balance-sat", after_sat),
    ] {
        headers.insert(name, HeaderValue::from(value));
    }
    Ok(response)
}

/// Release proofs stuck by an interrupted swap and retry parked tokens.
async fn recover_pocket(state: &ProxyState, key_id: &str, restore: bool) -> Value {
    let result: Result<Value> = async {
        let config = state.registry()?.get(key_id)?;
        let pocket = state.pocket(config.index).await?;
        let report = pocket.recover_incomplete_sagas().await?;
        // Proofs left pending by a failed call go back to unspent if the
        // mint says they are.
        pocket.check_all_pending_proofs().await?;
        // A swap the mint completed but whose reply was lost leaves the
        // outputs only at the mint: pockets have deterministic seeds, so
        // NUT-09/13 restore gets them back.
        let restored = if restore {
            u64::from(pocket.restore().await?.unspent)
        } else {
            0
        };
        let parked = retry_parked(state, key_id, &pocket).await;
        Ok(
            json!({ "sagas": format!("{report:?}"), "restored_unspent_sat": restored,
            "parked_received_sat": parked,
            "balance_sat": u64::from(pocket.total_balance().await?) }),
        )
    }
    .await;
    result.unwrap_or_else(|error| json!({ "error": format!("{error:#}") }))
}

/// Sweep pockets of revoked or expired keys home.
async fn sweep_closed(state: &ProxyState) -> Result<()> {
    let now = now_secs();
    let policies = state.policies()?;
    let registry = state.registry()?;
    for meta in load_keys(&state.config.keys_path)? {
        let Ok(config) = registry.get(&meta.key_id) else {
            continue;
        };
        if config.swept_at.is_some() {
            continue;
        }
        let policy = policies.iter().find(|p| p.id == meta.key_id);
        let lock = state.key_lock(&meta.key_id).await;
        let _guard = lock.lock().await;
        let pocket = state.pocket(config.index).await?;
        let parked = retry_parked(state, &meta.key_id, &pocket).await;
        if parked > 0 {
            log(json!({ "key": meta.label, "parked_received_sat": parked }));
        }
        if check_key(policy, now).is_ok() {
            continue;
        }
        pocket.recover_incomplete_sagas().await?;
        let report = sweep_pocket(&pocket, &state.main).await?;
        if report.is_complete() {
            registry.mark_swept(&meta.key_id, now)?;
        }
        log(json!({
            "key": meta.label, "closed": if policy.is_some_and(|p| p.revoked) { "revoked" } else { "expired" },
            "swept_home_sat": report.swept_msat / MSAT_PER_SAT,
            "pending_sat": report.pending_msat / MSAT_PER_SAT,
        }));
    }
    Ok(())
}

/// Run the proxy on `listen` until Ctrl-C.
pub async fn run(config: ProxyConfig, main: Arc<Wallet>, listen: &str) -> Result<()> {
    let registry = PocketRegistry::new(&config.pockets_path)?;
    let state = Arc::new(ProxyState {
        refiller: Refiller::new(Arc::clone(&main), registry),
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?,
        main,
        wallets: Mutex::new(HashMap::new()),
        locks: Mutex::new(HashMap::new()),
        cooldown: Mutex::new(HashMap::new()),
        config,
    });
    let main_recovery = async {
        let report = state.main.recover_incomplete_sagas().await?;
        state.main.check_all_pending_proofs().await?;
        Ok::<_, cdk::Error>(json!({ "sagas": format!("{report:?}"),
            "balance_sat": u64::from(state.main.total_balance().await?) }))
    }
    .await;
    log(json!({ "startup_recovery": "main", "result": main_recovery.map_err(|e| e.to_string()) }));
    for meta in load_keys(&state.config.keys_path)? {
        let recovered = recover_pocket(&state, &meta.key_id, true).await;
        log(json!({ "startup_recovery": meta.label, "result": recovered }));
    }
    let sweeper = {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(state.config.sweep_interval);
            loop {
                ticker.tick().await;
                if let Err(error) = sweep_closed(&state).await {
                    eprintln!("sweep failed: {error:#}");
                }
            }
        })
    };
    let app = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/pocket", get(pocket_info))
        .route("/v1/{*rest}", post(forward))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    println!("{}", json!({ "listening": listen }));
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    sweeper.abort();
    served.context("serve")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(models: &[&str], cap: Option<u64>) -> KeyMeta {
        KeyMeta {
            key_id: "k".into(),
            label: "k".into(),
            allowed_models: models.iter().map(|m| (*m).to_owned()).collect(),
            max_per_payment_sats: cap,
            created_at: 0,
            api_key_sha256: None,
        }
    }

    fn policy(revoked: bool, expires_at: Option<u64>) -> ConnectionPolicy {
        ConnectionPolicy {
            id: "k".into(),
            name: "k".into(),
            client_pubkey: "k".into(),
            allowed_methods: vec![],
            budget_msat: None,
            budget_period: cdk_nwc_policy::BudgetPeriod::Never,
            max_single_payment_msat: None,
            max_fee_reserve_msat: 0,
            fee_overage_msat: 0,
            expires_at,
            revoked,
            used_msat: 0,
            period_start_secs: 0,
        }
    }

    #[test]
    fn key_status() {
        assert_eq!(check_key(Some(&policy(false, None)), 100), Ok(()));
        assert_eq!(check_key(Some(&policy(false, Some(101))), 100), Ok(()));
        assert_eq!(
            check_key(Some(&policy(false, Some(100))), 100),
            Err(Refusal::Expired)
        );
        assert_eq!(
            check_key(Some(&policy(true, None)), 100),
            Err(Refusal::Revoked)
        );
        assert_eq!(check_key(None, 100), Err(Refusal::Revoked));
    }

    #[test]
    fn price_is_rounded_up_max_cost() {
        let models = json!({ "data": [
            { "id": "a", "sats_pricing": { "max_cost": 44.16 } },
            { "id": "b", "sats_pricing": { "max_cost": 0.2 } },
            { "id": "c" },
        ]});
        assert_eq!(max_cost_sat(&models, "a"), Some(45));
        assert_eq!(max_cost_sat(&models, "b"), Some(1));
        assert_eq!(max_cost_sat(&models, "c"), None);
        assert_eq!(max_cost_sat(&models, "zz"), None);
    }

    #[test]
    fn request_rules() {
        let m = meta(&["a"], Some(50));
        assert_eq!(check_request(&m, "a", Some(45)), Ok(45));
        assert_eq!(
            check_request(&m, "b", Some(1)),
            Err(Refusal::ModelNotAllowed("b".into()))
        );
        assert_eq!(
            check_request(&m, "a", Some(51)),
            Err(Refusal::OverRequestCap {
                price_sat: 51,
                cap_sat: 50
            })
        );
        assert_eq!(
            check_request(&m, "a", None),
            Err(Refusal::UnknownModel("a".into()))
        );
        // No list and no cap: anything priced goes.
        assert_eq!(check_request(&meta(&[], None), "x", Some(999)), Ok(999));
    }
}
