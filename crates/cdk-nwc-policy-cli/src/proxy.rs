//! `key proxy`: an OpenAI-compatible endpoint where every request is paid
//! with ecash from the calling key's own pocket.
//!
//! Per request:
//! 1. the key is found by its API key; revoked or expired keys are refused;
//! 2. the model must be on the key's allow-list;
//! 3. the price is the provider's published `sats_pricing.max_cost` for the
//!    model, and must not exceed the key's per-request cap;
//! 4. one exact bundle of ecash is taken from this key's pocket (refilled
//!    from main within the refill cap if needed) and sent as `X-Cashu`;
//! 5. the provider's change (`X-Cashu` response header) goes back into the
//!    same pocket, and the reply is streamed through.
//!
//! All checks in 1-3 happen before any ecash leaves the pocket. Keys,
//! policies and the pocket registry are re-read on every request, so new,
//! revoked and expired keys take effect without a restart.
//!
//! `POST /v1/pocket/send` and `/v1/pocket/receive` expose the same rules to a
//! client that pays the provider itself (routstrd in `xcashu` mode): it gets
//! one capped token per request and hands the change back.
//!
//! Mint calls are few and never looped: sends are offline when a bundle is
//! ready, a transient mint error (429, 5xx, network) starts a shared backoff
//! during which only offline sends run and change is parked on disk, and
//! parked tokens are retried by the sweeper once the backoff is over.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use cdk::wallet::{ReceiveOptions, Wallet};
use cdk_nwc_pocket::{
    is_transient, sweep_pocket, Error as PocketError, PocketRegistry, Refiller, SpendLock,
    MSAT_PER_SAT,
};
use cdk_nwc_policy::{ConnectionPolicy, PolicyStore};
use serde_json::{json, Value};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::ecash::{
    bundle_for, complement_target, forget_sent, is_own_token, loose_amounts, net_receive_sat,
    reclaim_outstanding, take_exact, take_offline, token_value_sat, unmark_sent, Taken,
};
use crate::key::{api_key_digest, load_keys, KeyMeta};
use crate::pocket::{is_closed, open_pocket};

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
    ModelNotAllowed {
        /// Requested model.
        model: String,
        /// The key's allow-list.
        allowed: Vec<String>,
    },
    /// The key has a model allow-list, so a raw token request must name
    /// the model it pays for.
    ModelRequired,
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
            Self::ModelNotAllowed { .. } => (StatusCode::FORBIDDEN, "model_not_allowed"),
            Self::ModelRequired => (StatusCode::FORBIDDEN, "model_required"),
            Self::UnknownModel(_) => (StatusCode::NOT_FOUND, "model_not_found"),
            Self::OverRequestCap { .. } => (StatusCode::PAYMENT_REQUIRED, "over_request_cap"),
            Self::BudgetExhausted { .. } => (StatusCode::PAYMENT_REQUIRED, "budget_exhausted"),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::InvalidKey => "invalid API key (use the sk-pocket-… key from `key create`)".into(),
            Self::Revoked => "this key was revoked; its pocket was swept home".into(),
            Self::Expired => "this key has expired; its pocket was swept home".into(),
            Self::ModelNotAllowed { model, allowed } => format!(
                "model '{model}' is not allowed for this key (allowed: {}). No ecash was spent",
                allowed.join(", ")
            ),
            Self::ModelRequired => {
                "this key only pays for listed models; name the model you pay for".into()
            }
            Self::UnknownModel(model) => format!("the provider lists no price for model '{model}'"),
            Self::OverRequestCap { price_sat, cap_sat } => format!(
                "a request to this model can cost up to {price_sat} sat, above this key's \
                 {cap_sat} sat per-request cap. No ecash was spent"
            ),
            Self::BudgetExhausted {
                balance_sat,
                price_sat,
            } => format!(
                "budget used up: the pocket holds {balance_sat} sat, a request needs {price_sat} sat \
                 + fee, and the refill cap allows no more. No ecash was spent"
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
        return Err(Refusal::ModelNotAllowed {
            model: model.to_owned(),
            allowed: meta.allowed_models.clone(),
        });
    }
    let price_sat = price_sat.ok_or_else(|| Refusal::UnknownModel(model.to_owned()))?;
    if let Some(cap_sat) = meta.max_per_payment_sats {
        if price_sat > cap_sat {
            return Err(Refusal::OverRequestCap { price_sat, cap_sat });
        }
    }
    Ok(price_sat)
}

/// Rules for a raw token (`/v1/pocket/send`): the model, when the key has
/// an allow-list, and the per-request cap on the amount.
pub fn check_send(meta: &KeyMeta, model: Option<&str>, amount_sat: u64) -> Result<(), Refusal> {
    if !meta.allowed_models.is_empty() {
        check_request(meta, model.ok_or(Refusal::ModelRequired)?, Some(amount_sat))?;
    } else if let Some(cap_sat) = meta.max_per_payment_sats {
        if amount_sat > cap_sat {
            return Err(Refusal::OverRequestCap {
                price_sat: amount_sat,
                cap_sat,
            });
        }
    }
    Ok(())
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
    /// Seconds between sweeper ticks (expiry/revoke sweeps, parked tokens).
    pub sweep_interval: Duration,
}

/// First and longest backoff after a transient mint error, in seconds.
const BACKOFF_SECS: (u64, u64) = (2, 60);

#[derive(Debug, Default)]
struct Backoff {
    until: u64,
    step: u64,
}

struct ProxyState {
    config: ProxyConfig,
    http: reqwest::Client,
    main: Arc<Wallet>,
    refiller: Refiller,
    wallets: Mutex<HashMap<u32, Arc<Wallet>>>,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// Shared across keys: a mint rate-limits by client, not by pocket.
    backoff: std::sync::Mutex<Backoff>,
    /// Pockets whose last swap may have reached the mint without a reply;
    /// the sweeper restores them from their seed.
    restore: std::sync::Mutex<HashSet<String>>,
    /// Last price paid per key, to cut change into the next bundle.
    prices: std::sync::Mutex<HashMap<String, u64>>,
}

/// Held while a key's proofs are selected or swapped: in-process first, then
/// the cross-process [`SpendLock`] shared with `serve`.
struct SpendGuard {
    _file: SpendLock,
    _task: OwnedMutexGuard<()>,
}

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

    async fn spend(&self, key_id: &str) -> Result<SpendGuard> {
        let lock = {
            let mut locks = self.locks.lock().await;
            Arc::clone(locks.entry(key_id.to_owned()).or_default())
        };
        let task = lock.lock_owned().await;
        let file = self.refiller.registry().lock_spend(key_id).await?;
        Ok(SpendGuard {
            _file: file,
            _task: task,
        })
    }

    fn registry(&self) -> &PocketRegistry {
        self.refiller.registry()
    }

    fn policies(&self) -> Result<Vec<ConnectionPolicy>> {
        Ok(PolicyStore::new(&self.config.policy_path)?.connections()?)
    }

    /// Seconds left in the mint backoff, if any.
    fn mint_busy(&self) -> Option<u64> {
        let now = now_secs();
        let backoff = self.backoff.lock().unwrap_or_else(|e| e.into_inner());
        (backoff.until > now).then(|| backoff.until - now)
    }

    /// Record a mint error; returns whether it was transient (backoff grows).
    fn mint_error(&self, error: &cdk::Error) -> bool {
        if !is_transient(error) {
            return false;
        }
        let mut backoff = self.backoff.lock().unwrap_or_else(|e| e.into_inner());
        backoff.step = (backoff.step * 2).clamp(BACKOFF_SECS.0, BACKOFF_SECS.1);
        backoff.until = now_secs() + backoff.step;
        log(json!({ "mint_backoff_secs": backoff.step, "mint_error": error.to_string() }));
        true
    }

    fn mint_ok(&self) {
        let mut backoff = self.backoff.lock().unwrap_or_else(|e| e.into_inner());
        backoff.step = 0;
    }

    fn remember_price(&self, key_id: &str, price_sat: u64) {
        self.prices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key_id.to_owned(), price_sat);
    }

    fn last_price(&self, key_id: &str) -> Option<u64> {
        self.prices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key_id)
            .copied()
    }

    fn flag_restore(&self, key_id: &str) {
        self.restore
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key_id.to_owned());
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
/// token), retried by the sweeper so a mint hiccup never loses it.
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

/// How a token offered to a pocket ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Landed {
    /// Received; sats after the mint fee.
    Received(u64),
    /// Saved on disk for the sweeper (mint busy or failing).
    Parked,
    /// Already spent: nothing to receive.
    Spent,
    /// The provider handed our own token back unredeemed: its proofs are
    /// unspent in the pocket again, no swap (sats = face value).
    Returned(u64),
}

/// Receive options that shape `token`'s outputs so that, with the loose
/// proofs already in the pocket, they complete bundles for `price_sat`.
async fn shaped(pocket: &Wallet, token: &str, price_sat: Option<u64>) -> ReceiveOptions {
    let target = async {
        let bundle = bundle_for(pocket, price_sat?).await.ok()?;
        let net = net_receive_sat(pocket, token).await?;
        let loose = loose_amounts(pocket).await.ok()?;
        Some(complement_target(&bundle, &loose, net))
    }
    .await;
    ReceiveOptions {
        amount_split_target: target.unwrap_or_default(),
        ..Default::default()
    }
}

/// Receive `token` into `pocket` with one mint call, or park it. During a
/// mint backoff it is parked without calling the mint at all. With a price,
/// the outputs are cut to complete the next bundle (free: the swap happens
/// anyway).
async fn land(
    state: &ProxyState,
    key_id: &str,
    pocket: &Wallet,
    token: &str,
    price_sat: Option<u64>,
) -> Landed {
    if state.mint_busy().is_none() {
        let options = shaped(pocket, token, price_sat).await;
        let targeted = options.amount_split_target != cdk::amount::SplitTarget::None;
        let mut result = pocket.receive(token, options).await;
        if let Err(error) = &result {
            // A target that does not fit fails locally: retry plain, once.
            if targeted && !is_transient(error) && !matches!(error, cdk::Error::TokenAlreadySpent) {
                result = pocket.receive(token, ReceiveOptions::default()).await;
            }
        }
        match result {
            Ok(amount) => {
                state.mint_ok();
                return Landed::Received(u64::from(amount));
            }
            Err(cdk::Error::TokenAlreadySpent) => return Landed::Spent,
            Err(error) => {
                if state.mint_error(&error) {
                    // The swap may have happened with the reply lost.
                    state.flag_restore(key_id);
                }
                log(json!({ "key_id": key_id, "receive_error": error.to_string() }));
            }
        }
    }
    let parked = park_token(&state.config.keys_path, key_id, token);
    log(json!({ "key_id": key_id, "parked_token": parked.is_ok() }));
    Landed::Parked
}

/// Retry parked tokens for `key_id`. Returns sats received.
async fn retry_parked(state: &ProxyState, key_id: &str, pocket: &Wallet) -> u64 {
    let Ok(entries) = std::fs::read_dir(pending_dir(&state.config.keys_path)) else {
        return 0;
    };
    let mut received = 0;
    for entry in entries.flatten() {
        if state.mint_busy().is_some() {
            break;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&format!("{key_id}-")) {
            continue;
        }
        let Ok(token) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Shaped like any change, so the next request can send offline.
        let options = shaped(pocket, token.trim(), state.last_price(key_id)).await;
        let targeted = options.amount_split_target != cdk::amount::SplitTarget::None;
        let mut result = pocket.receive(token.trim(), options).await;
        if let Err(error) = &result {
            if targeted && !is_transient(error) && !matches!(error, cdk::Error::TokenAlreadySpent) {
                result = pocket
                    .receive(token.trim(), ReceiveOptions::default())
                    .await;
            }
        }
        match result {
            Ok(amount) => {
                state.mint_ok();
                received += u64::from(amount);
                let _ = std::fs::remove_file(&path);
            }
            Err(cdk::Error::TokenAlreadySpent) => {
                // Either the provider took it back or our earlier attempt
                // landed without a reply: restore covers the second case.
                state.flag_restore(key_id);
                let _ = std::fs::remove_file(&path);
            }
            Err(error) => {
                state.mint_error(&error);
            }
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

fn refusal_response(label: Option<&str>, refusal: &Refusal) -> Response {
    let (status, code) = refusal.status_and_code();
    let message = match label {
        Some(label) => format!("pocket key '{label}': {}", refusal.message()),
        None => format!("pocket: {}", refusal.message()),
    };
    error_response(status, code, &message)
}

fn busy_response(secs: u64) -> Response {
    let mut response = error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "mint_busy",
        &format!("pocket: the mint is rate-limiting; no ecash was spent. Retry in {secs} s"),
    );
    response
        .headers_mut()
        .insert("retry-after", HeaderValue::from(secs.max(1)));
    response
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
        Err(refusal) => return refusal_response(None, &refusal),
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
            &format!("pocket: provider unreachable: {error}"),
        ),
    }
}

/// `GET /v1/pocket[?model=…]`: status and balances; with a model, also its
/// price for this key or the refusal a request to it would get, so a client
/// (routstrd) can show the key's own reason before trying to pay.
async fn pocket_info(
    State(state): State<Arc<ProxyState>>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let meta = match find_key(&state, &headers) {
        Ok(meta) => meta,
        Err(refusal) => return refusal_response(None, &refusal),
    };
    let result: Result<Value> = async {
        let config = state.registry().get(&meta.key_id)?;
        let pocket = state.pocket(config.index).await?;
        let now = now_secs();
        let policies = state.policies()?;
        let policy = policies.iter().find(|p| p.id == meta.key_id);
        let status = match check_key(policy, now) {
            Ok(()) if config.swept_at.is_none() => "active",
            Err(Refusal::Expired) => "expired",
            _ => "revoked",
        };
        let balance_sat = u64::from(pocket.total_balance().await?);
        let refill_sat = if status == "active" && config.refill.auto_refill.is_some() {
            let main_sat = u64::from(state.main.total_balance().await?);
            (state.registry().remaining_msat(&meta.key_id, now)? / MSAT_PER_SAT).min(main_sat)
        } else {
            0
        };
        let mut body = json!({
            "key_id": meta.key_id,
            "label": meta.label,
            "status": status,
            "balance_sat": balance_sat,
            "refill_remaining_sat": refill_sat,
            "spendable_sat": if status == "active" { balance_sat + refill_sat } else { 0 },
            "max_per_request_sat": meta.max_per_payment_sats,
            "allowed_models": meta.allowed_models,
            "expires_at": policy.and_then(|p| p.expires_at),
            "mint_url": state.config.mint_url,
            "mint_busy_secs": state.mint_busy(),
        });
        if let Some(model) = query.get("model") {
            let checked = match check_key(policy, now) {
                Err(refusal) => Err(Outcome::Refused(refusal)),
                Ok(()) if config.swept_at.is_some() => Err(Outcome::Refused(Refusal::Revoked)),
                Ok(()) => price_for(&state, &meta, model).await,
            };
            match checked {
                Ok(price_sat) => body["price_sat"] = json!(price_sat),
                Err(Outcome::Refused(refusal)) => {
                    let (code, name) = refusal.status_and_code();
                    body["refusal"] = json!({
                        "status": code.as_u16(), "code": name,
                        "message": format!("pocket key '{}': {}", meta.label, refusal.message()),
                    });
                }
                Err(Outcome::Busy(_)) | Err(Outcome::Failed(_)) => {}
            }
        }
        Ok(body)
    }
    .await;
    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &format!("{error:#}"),
        ),
    }
}

enum Outcome {
    Refused(Refusal),
    /// The mint is backing off; nothing was spent.
    Busy(u64),
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

/// Ecash taken from a pocket for one request.
struct Paid {
    pocket: Arc<Wallet>,
    taken: Taken,
    /// Drop in pocket balance for the send (bundle + any re-bundle fee),
    /// net of a refill made for it.
    spent_sat: u64,
    refilled_sat: u64,
}

/// Key status, then take one bundle for `price_sat` from the key's pocket,
/// refilling once within the refill cap. Rules on model and price must be
/// checked by the caller first.
async fn take_for(state: &ProxyState, meta: &KeyMeta, price_sat: u64) -> Result<Paid, Outcome> {
    let now = now_secs();
    let policies = state.policies()?;
    check_key(policies.iter().find(|p| p.id == meta.key_id), now).map_err(Outcome::Refused)?;
    let config = state.registry().get(&meta.key_id)?;
    if config.swept_at.is_some() {
        return Err(Outcome::Refused(Refusal::Revoked));
    }
    let _guard = state.spend(&meta.key_id).await?;
    state.remember_price(&meta.key_id, price_sat);
    let pocket = state.pocket(config.index).await?;
    let before_sat = u64::from(pocket.total_balance().await?);
    let mut refilled_sat = 0;
    // A token may carry more than the price (the provider returns the rest),
    // but never more than the key's per-request cap: that bounds what one
    // request can lose even to a dishonest provider.
    let max_net_sat = meta
        .max_per_payment_sats
        .unwrap_or(price_sat.saturating_mul(2))
        .max(price_sat);
    let taken = loop {
        let attempt = match state.mint_busy() {
            // Backoff: only an offline send, no mint call.
            Some(secs) => match take_offline(&pocket, price_sat, max_net_sat).await? {
                Some(taken) => Ok(taken),
                None => return Err(Outcome::Busy(secs)),
            },
            None => take_exact(&pocket, price_sat, max_net_sat).await,
        };
        match attempt {
            Ok(taken) => break taken,
            Err(cdk::Error::InsufficientFunds) => {}
            Err(error) => {
                if state.mint_error(&error) {
                    state.flag_restore(&meta.key_id);
                    return Err(Outcome::Busy(state.mint_busy().unwrap_or(1)));
                }
                return Err(error.into());
            }
        }
        let balance_sat = u64::from(pocket.total_balance().await?);
        let refused = Refusal::BudgetExhausted {
            balance_sat,
            price_sat,
        };
        let Some(auto) = config.refill.auto_refill.filter(|_| refilled_sat == 0) else {
            return Err(Outcome::Refused(refused));
        };
        let bundle = bundle_for(&pocket, price_sat).await?;
        let wanted_msat = ((bundle.total_sat + 2) * MSAT_PER_SAT)
            .max(auto.target_msat)
            .saturating_sub(balance_sat * MSAT_PER_SAT);
        // The pocket's receive swap happens anyway: cut it into bundles,
        // completing them with the loose proofs already there.
        let loose = loose_amounts(&pocket).await?;
        let split = |amount: cdk::Amount| complement_target(&bundle, &loose, u64::from(amount));
        match state
            .refiller
            .refill_split(&meta.key_id, &pocket, wanted_msat, now, split)
            .await
        {
            Ok(added) if added > 0 => {
                state.mint_ok();
                refilled_sat = added / MSAT_PER_SAT;
                log(json!({ "key": meta.label, "refilled_sat": refilled_sat,
                    "refill_remaining_sat": state.registry().remaining_msat(&meta.key_id, now)? / MSAT_PER_SAT }));
            }
            Ok(_) | Err(PocketError::RefillCapReached) => return Err(Outcome::Refused(refused)),
            Err(PocketError::Wallet(cdk::Error::InsufficientFunds)) => {
                return Err(Outcome::Refused(refused))
            }
            Err(PocketError::Wallet(error)) if state.mint_error(&error) => {
                return Err(Outcome::Busy(state.mint_busy().unwrap_or(1)))
            }
            Err(error) => return Err(error.into()),
        }
    };
    let after_sat = u64::from(pocket.total_balance().await?);
    Ok(Paid {
        pocket,
        taken,
        spent_sat: (before_sat + refilled_sat).saturating_sub(after_sat),
        refilled_sat,
    })
}

fn outcome_response(state: &ProxyState, meta: &KeyMeta, what: &str, outcome: Outcome) -> Response {
    match outcome {
        Outcome::Refused(refusal) => {
            log(
                json!({ "key": meta.label, "what": what, "refused": refusal.status_and_code().1,
                "reason": refusal.message(), "ecash_sent": 0 }),
            );
            refusal_response(Some(&meta.label), &refusal)
        }
        Outcome::Busy(secs) => {
            log(
                json!({ "key": meta.label, "what": what, "mint_busy_secs": secs, "ecash_sent": 0 }),
            );
            busy_response(secs)
        }
        Outcome::Failed(error) => {
            log(json!({ "key": meta.label, "what": what, "error": format!("{error:#}") }));
            let _ = state;
            error_response(
                StatusCode::BAD_GATEWAY,
                "pocket_proxy_error",
                &format!("pocket key '{}': {error:#}", meta.label),
            )
        }
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
        Err(refusal) => return refusal_response(None, &refusal),
    };
    let Some(model) = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_owned))
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "pocket: the request body needs a model",
        );
    };
    match pay_and_forward(&state, &meta, &model, &rest, body).await {
        Ok(response) => response,
        Err(outcome) => outcome_response(&state, &meta, &model, outcome),
    }
}

/// Price of `model` for `meta`, after the model and cap rules.
async fn price_for(state: &ProxyState, meta: &KeyMeta, model: &str) -> Result<u64, Outcome> {
    if !meta.allowed_models.is_empty() && !meta.allowed_models.iter().any(|m| m == model) {
        return Err(Outcome::Refused(Refusal::ModelNotAllowed {
            model: model.to_owned(),
            allowed: meta.allowed_models.clone(),
        }));
    }
    let models = state.provider_models().await?;
    check_request(meta, model, max_cost_sat(&models, model)).map_err(Outcome::Refused)
}

async fn pay_and_forward(
    state: &ProxyState,
    meta: &KeyMeta,
    model: &str,
    rest: &str,
    body: Bytes,
) -> Result<Response, Outcome> {
    // Key status first, so a revoked key never reaches the provider.
    check_key(
        state.policies()?.iter().find(|p| p.id == meta.key_id),
        now_secs(),
    )
    .map_err(Outcome::Refused)?;
    let price_sat = price_for(state, meta, model).await?;
    let paid = take_for(state, meta, price_sat).await?;
    let token = paid.taken.token.clone();

    // Forward with the token. The pocket lock is not held while the model
    // runs, so other requests on this key are not blocked.
    let url = format!("{}/v1/{rest}", state.config.provider.trim_end_matches('/'));
    let sent = state
        .http
        .post(url)
        .header("content-type", "application/json")
        .header("x-cashu", &token)
        .body(body)
        .send()
        .await;
    let upstream = match sent {
        Ok(upstream) => upstream,
        Err(error) => {
            // Never reached the provider: take the ecash back.
            let landed = land(state, &meta.key_id, &paid.pocket, &token, Some(price_sat)).await;
            if landed != Landed::Parked {
                let _ = forget_sent(&paid.pocket, &paid.taken.proofs).await;
            }
            log(
                json!({ "key": meta.label, "model": model, "provider_unreachable": error.to_string(),
                "reclaimed": format!("{landed:?}") }),
            );
            return Err(Outcome::Failed(anyhow::anyhow!(
                "provider unreachable ({error}); the ecash was taken back"
            )));
        }
    };
    let status = upstream.status().as_u16();
    let content_type = upstream.headers().get("content-type").cloned();
    let change = upstream
        .headers()
        .get("x-cashu")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // Change back into the same pocket. Routstr sends it in the response
    // header, before the body, so streaming is not delayed.
    // Refused with our own token handed back (routstr-core does this when
    // the mint rate-limits it): reuse the proofs, no swap, nothing spent.
    let own_back = !(200..300).contains(&status)
        && change
            .as_deref()
            .is_some_and(|c| is_own_token(c, &paid.taken.proofs))
        && unmark_sent(&paid.pocket, &paid.taken.proofs).await.is_ok();
    let (landed, change_face_sat) = match &change {
        _ if own_back => (Landed::Returned(paid.taken.token_sat), paid.taken.token_sat),
        Some(change) => (
            land(state, &meta.key_id, &paid.pocket, change, Some(price_sat)).await,
            token_value_sat(change).unwrap_or(0),
        ),
        // Refused before redemption (no change header): the token is unspent.
        None if !(200..300).contains(&status) => (
            land(state, &meta.key_id, &paid.pocket, &token, Some(price_sat)).await,
            paid.taken.token_sat,
        ),
        None => (Landed::Spent, 0),
    };
    // The sent proofs are settled unless our own token was parked for a
    // retry: forget them so they are not counted as outstanding.
    if !own_back && (change.is_some() || landed != Landed::Parked) {
        let _ = forget_sent(&paid.pocket, &paid.taken.proofs).await;
    }
    let change_sat = match landed {
        Landed::Received(sat) | Landed::Returned(sat) => sat,
        _ => 0,
    };
    let provider_kept_sat = paid.taken.token_sat.saturating_sub(change_face_sat);
    let cost_sat = paid.spent_sat.saturating_sub(change_sat);
    let balance_sat = u64::from(paid.pocket.total_balance().await?);
    // Parked change lands later (the sweeper logs it): fees are not known yet.
    let parked_sat = (landed == Landed::Parked).then_some(change_face_sat);
    log(json!({
        "key": meta.label, "model": model, "status": status, "price_sat": price_sat,
        "sent_sat": paid.taken.token_sat, "send_swap": paid.taken.swapped,
        "change_sat": change_sat, "change": format!("{landed:?}"),
        "change_parked_sat": parked_sat,
        "provider_kept_sat": provider_kept_sat,
        "mint_fees_sat": parked_sat.is_none().then(|| cost_sat.saturating_sub(provider_kept_sat)),
        "cost_sat": cost_sat, "refilled_sat": paid.refilled_sat, "balance_sat": balance_sat,
    }));

    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = response.headers_mut();
    if let Some(content_type) = content_type {
        headers.insert("content-type", content_type);
    }
    for (name, value) in [
        ("x-pocket-price-sat", price_sat),
        ("x-pocket-sent-sat", paid.taken.token_sat),
        ("x-pocket-change-sat", change_sat),
        ("x-pocket-cost-sat", cost_sat),
        ("x-pocket-balance-sat", balance_sat),
    ] {
        headers.insert(name, HeaderValue::from(value));
    }
    headers.insert(
        "x-pocket-change",
        HeaderValue::from_static(match landed {
            Landed::Received(_) => "received",
            Landed::Parked => "parked",
            Landed::Spent => "none",
            Landed::Returned(_) => "returned",
        }),
    );
    Ok(response)
}

/// `POST /v1/pocket/send {"amount_sat": n, "model": "..."}`: one capped
/// token for a client that pays the provider itself.
async fn pocket_send(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let meta = match find_key(&state, &headers) {
        Ok(meta) => meta,
        Err(refusal) => return refusal_response(None, &refusal),
    };
    let Some(amount_sat) = body
        .get("amount_sat")
        .and_then(Value::as_u64)
        .filter(|a| *a > 0)
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "pocket: amount_sat must be a positive integer",
        );
    };
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            headers
                .get("x-pocket-model")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        });
    let result = async {
        if let Err(refusal) = check_send(&meta, model.as_deref(), amount_sat) {
            return Err(Outcome::Refused(refusal));
        }
        take_for(&state, &meta, amount_sat).await
    }
    .await;
    match result {
        Ok(paid) => {
            log(
                json!({ "key": meta.label, "what": "send", "model": model, "amount_sat": amount_sat,
                "sent_sat": paid.taken.token_sat, "send_swap": paid.taken.swapped,
                "spent_sat": paid.spent_sat, "refilled_sat": paid.refilled_sat }),
            );
            Json(json!({
                "token": paid.taken.token,
                "amount_sat": paid.taken.token_sat,
                "spent_sat": paid.spent_sat,
                "swap": paid.taken.swapped,
            }))
            .into_response()
        }
        Err(outcome) => outcome_response(&state, &meta, "send", outcome),
    }
}

/// `POST /v1/pocket/receive {"token": "..."}`: change back into the pocket.
async fn pocket_receive(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let meta = match find_key(&state, &headers) {
        Ok(meta) => meta,
        Err(refusal) => return refusal_response(None, &refusal),
    };
    let Some(token) = body.get("token").and_then(Value::as_str) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "pocket: body needs a token",
        );
    };
    let result: Result<(Landed, u64)> = async {
        let config = state.registry().get(&meta.key_id)?;
        let pocket = state.pocket(config.index).await?;
        let price = state.last_price(&meta.key_id);
        let landed = land(&state, &meta.key_id, &pocket, token, price).await;
        Ok((landed, u64::from(pocket.total_balance().await?)))
    }
    .await;
    match result {
        Ok((landed, balance_sat)) => {
            log(
                json!({ "key": meta.label, "what": "receive", "landed": format!("{landed:?}"),
                "balance_sat": balance_sat }),
            );
            Json(json!({
                "received_sat": match landed { Landed::Received(sat) => sat, _ => 0 },
                "parked": landed == Landed::Parked,
                "already_spent": landed == Landed::Spent,
                "balance_sat": balance_sat,
            }))
            .into_response()
        }
        Err(error) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &format!("{error:#}"),
        ),
    }
}

/// Release proofs stuck by an interrupted swap, restore lost outputs from
/// the pocket seed (NUT-09/13) and retry parked tokens.
async fn recover_pocket(state: &ProxyState, key_id: &str, restore: bool) -> Value {
    let result: Result<Value> = async {
        let config = state.registry().get(key_id)?;
        let _guard = state.spend(key_id).await?;
        let pocket = state.pocket(config.index).await?;
        let report = pocket.recover_incomplete_sagas().await?;
        pocket.check_all_pending_proofs().await?;
        // `restore().unspent` counts every proof the scan saw, including
        // ones the pocket already had: report what the restore added.
        let restored = if restore {
            let before = u64::from(pocket.total_balance().await?);
            pocket.restore().await?;
            u64::from(pocket.total_balance().await?).saturating_sub(before)
        } else {
            0
        };
        let parked = retry_parked(state, key_id, &pocket).await;
        Ok(
            json!({ "sagas": format!("{report:?}"), "restored_sat": restored,
            "parked_received_sat": parked,
            "balance_sat": u64::from(pocket.total_balance().await?) }),
        )
    }
    .await;
    result.unwrap_or_else(|error| json!({ "error": format!("{error:#}") }))
}

/// One sweeper pass: parked tokens, flagged restores, and sweeping pockets
/// of revoked or expired keys (again if late change landed after a sweep).
/// Skipped entirely while the mint is backing off.
async fn sweep_tick(state: &ProxyState) -> Result<()> {
    if state.mint_busy().is_some() {
        return Ok(());
    }
    let now = now_secs();
    let policies = state.policies()?;
    let flagged: Vec<String> = state
        .restore
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .collect();
    for key_id in flagged {
        let result = recover_pocket(state, &key_id, true).await;
        log(json!({ "restore": key_id, "result": result }));
    }
    for meta in load_keys(&state.config.keys_path)? {
        let Ok(config) = state.registry().get(&meta.key_id) else {
            continue;
        };
        let policy = policies.iter().find(|p| p.id == meta.key_id);
        let _guard = state.spend(&meta.key_id).await?;
        let pocket = state.pocket(config.index).await?;
        let parked = retry_parked(state, &meta.key_id, &pocket).await;
        if parked > 0 {
            log(json!({ "key": meta.label, "parked_received_sat": parked }));
        }
        let closed = check_key(policy, now).is_err() || config.swept_at.is_some();
        if !closed {
            // Tokens handed out by /v1/pocket/send and since redeemed.
            if let Err(error) = pocket.check_all_pending_proofs().await {
                state.mint_error(&error);
            }
            continue;
        }
        if config.swept_at.is_some()
            && pocket.total_balance().await? == cdk::Amount::ZERO
            && pocket.get_pending_spent_proofs().await?.is_empty()
        {
            continue;
        }
        pocket.recover_incomplete_sagas().await?;
        let report = sweep_pocket(&pocket, &state.main).await?;
        // Revoke-by-swap also covers tokens already handed out.
        let reclaimed_sat = reclaim_outstanding(&pocket, &state.main).await?;
        if report.is_complete() && config.swept_at.is_none() {
            state.registry().mark_swept(&meta.key_id, now)?;
        }
        log(json!({
            "key": meta.label, "closed": if policy.is_some_and(|p| p.revoked) { "revoked" } else { "expired" },
            "swept_home_sat": report.swept_msat / MSAT_PER_SAT,
            "outstanding_reclaimed_sat": reclaimed_sat,
            "pending_sat": report.pending_msat / MSAT_PER_SAT,
        }));
    }
    Ok(())
}

/// A key proxy: build with [`Proxy::new`], serve [`Proxy::router`].
#[derive(Clone)]
pub struct Proxy {
    state: Arc<ProxyState>,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy")
            .field("provider", &self.state.config.provider)
            .finish_non_exhaustive()
    }
}

impl Proxy {
    /// Create a proxy. `pockets` pre-opens pocket wallets by index (tests);
    /// others are opened from `config` on first use.
    pub fn new(
        config: ProxyConfig,
        main: Arc<Wallet>,
        pockets: HashMap<u32, Arc<Wallet>>,
    ) -> Result<Self> {
        let registry = PocketRegistry::new(&config.pockets_path)?;
        Ok(Self {
            state: Arc::new(ProxyState {
                refiller: Refiller::new(Arc::clone(&main), registry),
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(600))
                    .build()?,
                main,
                wallets: Mutex::new(pockets),
                locks: Mutex::new(HashMap::new()),
                backoff: std::sync::Mutex::new(Backoff::default()),
                restore: std::sync::Mutex::new(HashSet::new()),
                prices: std::sync::Mutex::new(HashMap::new()),
                config,
            }),
        })
    }

    /// The HTTP routes.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/models", get(models))
            .route("/v1/pocket", get(pocket_info))
            .route("/v1/pocket/send", post(pocket_send))
            .route("/v1/pocket/receive", post(pocket_receive))
            .route("/v1/{*rest}", post(forward))
            .with_state(Arc::clone(&self.state))
    }

    /// One sweeper pass (see the module docs).
    pub async fn sweep_once(&self) -> Result<()> {
        sweep_tick(&self.state).await
    }

    /// Startup recovery of main and every pocket.
    pub async fn recover_all(&self) -> Result<()> {
        let state = &self.state;
        let main_recovery = async {
            let report = state.main.recover_incomplete_sagas().await?;
            state.main.check_all_pending_proofs().await?;
            Ok::<_, cdk::Error>(json!({ "sagas": format!("{report:?}"),
                "balance_sat": u64::from(state.main.total_balance().await?) }))
        }
        .await;
        log(
            json!({ "startup_recovery": "main", "result": main_recovery.map_err(|e| e.to_string()) }),
        );
        for meta in load_keys(&state.config.keys_path)? {
            let recovered = recover_pocket(state, &meta.key_id, true).await;
            log(json!({ "startup_recovery": meta.label, "result": recovered }));
        }
        Ok(())
    }
}

/// Run the proxy on `listen` until Ctrl-C.
pub async fn run(config: ProxyConfig, main: Arc<Wallet>, listen: &str) -> Result<()> {
    let interval = config.sweep_interval;
    let proxy = Proxy::new(config, main, HashMap::new())?;
    proxy.recover_all().await?;
    let sweeper = {
        let proxy = proxy.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                if let Err(error) = proxy.sweep_once().await {
                    eprintln!("sweep failed: {error:#}");
                }
            }
        })
    };
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    println!("{}", json!({ "listening": listen }));
    let served = axum::serve(listener, proxy.router())
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
            Err(Refusal::ModelNotAllowed {
                model: "b".into(),
                allowed: vec!["a".into()]
            })
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

    #[test]
    fn raw_token_rules() {
        let listed = meta(&["a"], Some(50));
        assert_eq!(check_send(&listed, Some("a"), 50), Ok(()));
        assert_eq!(check_send(&listed, None, 10), Err(Refusal::ModelRequired));
        assert!(matches!(
            check_send(&listed, Some("b"), 10),
            Err(Refusal::ModelNotAllowed { .. })
        ));
        assert_eq!(
            check_send(&listed, Some("a"), 51),
            Err(Refusal::OverRequestCap {
                price_sat: 51,
                cap_sat: 50
            })
        );
        let open = meta(&[], Some(20));
        assert_eq!(check_send(&open, None, 20), Ok(()));
        assert!(check_send(&open, None, 21).is_err());
        assert_eq!(check_send(&meta(&[], None), None, 10_000), Ok(()));
    }

    #[test]
    fn refusals_map_to_openai_errors() {
        let cases = [
            (Refusal::InvalidKey, 401, "invalid_api_key"),
            (Refusal::Revoked, 401, "key_revoked"),
            (Refusal::Expired, 401, "key_expired"),
            (Refusal::ModelRequired, 403, "model_required"),
            (Refusal::UnknownModel("m".into()), 404, "model_not_found"),
            (
                Refusal::OverRequestCap {
                    price_sat: 2,
                    cap_sat: 1,
                },
                402,
                "over_request_cap",
            ),
            (
                Refusal::BudgetExhausted {
                    balance_sat: 1,
                    price_sat: 2,
                },
                402,
                "budget_exhausted",
            ),
        ];
        for (refusal, status, code) in cases {
            assert_eq!(
                refusal.status_and_code(),
                (StatusCode::from_u16(status).unwrap(), code)
            );
            assert!(!refusal.message().is_empty());
        }
    }
}
