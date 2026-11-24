//! Per-connection policy enforcement for Cashu NWC wallet services.
//!
//! This crate is a prototype policy module. It does not own a wallet or talk
//! to relays. It enforces bounded, revocable authority for an NWC wallet
//! service and persists its ledger to disk.

#![allow(missing_docs)]

pub mod manager;

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cdk_nostr::nwc::handler::{NwcRequestHandler, RequestContext};
use cdk_nostr::nwc::nip47::{
    ErrorCode, GetBalanceResponse, GetInfoResponse, ListTransactionsRequest, LookupInvoiceRequest,
    LookupInvoiceResponse, MakeInvoiceRequest, MakeInvoiceResponse, NIP47Error, PayInvoiceRequest,
    PayInvoiceResponse,
};
use fs2::FileExt;
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Budget renewal period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetPeriod {
    /// Budget never resets automatically.
    Never,
    /// Budget resets every day.
    Daily,
    /// Budget resets every week.
    Weekly,
    /// Budget resets every 30 days.
    Monthly,
    /// Budget resets every 365 days.
    Yearly,
}

impl BudgetPeriod {
    fn seconds(self) -> Option<u64> {
        match self {
            Self::Never => None,
            Self::Daily => Some(86_400),
            Self::Weekly => Some(604_800),
            Self::Monthly => Some(2_592_000),
            Self::Yearly => Some(31_536_000),
        }
    }
}

/// Policy for one NWC app connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionPolicy {
    /// Stable connection ID.
    pub id: String,
    /// Human-readable app name.
    pub name: String,
    /// Public key of the authorized NWC client.
    pub client_pubkey: String,
    /// NIP-47 methods this app may call.
    pub allowed_methods: Vec<String>,
    /// Total budget in millisatoshis.
    pub budget_msat: Option<u64>,
    /// Budget renewal period.
    pub budget_period: BudgetPeriod,
    /// Maximum size of one payment in millisatoshis.
    pub max_single_payment_msat: Option<u64>,
    /// Fee reserve reserved in addition to the invoice amount.
    pub max_fee_reserve_msat: u64,
    /// Fees actually paid above the configured reserve.
    pub fee_overage_msat: u64,
    /// Unix timestamp when this connection expires.
    pub expires_at: Option<u64>,
    /// Whether this connection has been revoked.
    pub revoked: bool,
    /// Millisatoshis used in the current budget period.
    pub used_msat: u64,
    /// Unix timestamp when the current budget period started.
    pub period_start_secs: u64,
}

/// A reserved spend authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authorization {
    /// Unique authorization ID.
    pub id: String,
    /// Connection that made the request.
    pub connection_id: String,
    /// Budget period this authorization belongs to.
    pub period_start_secs: u64,
    /// Reserved amount in millisatoshis.
    pub amount_msat: u64,
}

/// Policy enforcement errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicyError {
    /// The client public key is unknown or revoked.
    #[error("client is not authorized")]
    Unauthorized,
    /// The method is not allowed for this connection.
    #[error("method not allowed: {0}")]
    Restricted(String),
    /// The request exceeds a budget cap.
    #[error("budget exceeded")]
    QuotaExceeded,
    /// The connection has expired.
    #[error("connection expired")]
    Expired,
    /// The Nostr event ID has already been processed.
    #[error("event replay detected")]
    Replayed,
    /// The authorization ID is unknown.
    #[error("authorization not found")]
    UnknownAuthorization,
    /// The connection ID is unknown.
    #[error("connection not found")]
    UnknownConnection,
    /// A connection ID or client key is duplicated.
    #[error("duplicate connection")]
    DuplicateClient,
    /// Disk input/output error.
    #[error("io error: {0}")]
    Io(String),
    /// Ledger serialization error.
    #[error("serialization error: {0}")]
    Serde(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PolicyFile {
    connections: Vec<ConnectionPolicy>,
    events: Vec<String>,
    reservations: Vec<Authorization>,
}

/// Persistent policy store and budget ledger.
#[derive(Debug)]
pub struct PolicyStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl PolicyStore {
    /// Open a policy store at `path`, creating an empty ledger if absent.
    ///
    /// The lock lives in a sidecar file so atomic ledger renames cannot defeat
    /// it. Every operation re-reads the ledger while holding the lock.
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, PolicyError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| PolicyError::Io(error.to_string()))?;
        }
        let lock_path = path.with_extension("policy.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| PolicyError::Io(error.to_string()))?;
        lock.lock_exclusive()
            .map_err(|error| PolicyError::Io(error.to_string()))?;
        let file = read_file(&path)?;
        validate_connections(&file)?;
        drop(lock);
        Ok(Self { path, lock_path })
    }

    /// Add or replace a connection policy.
    pub fn add_connection(&mut self, policy: ConnectionPolicy) -> Result<(), PolicyError> {
        self.mutate(|file| {
            if file.connections.iter().any(|existing| {
                existing.client_pubkey == policy.client_pubkey && existing.id != policy.id
            }) {
                return Err(PolicyError::DuplicateClient);
            }
            file.connections.retain(|existing| existing.id != policy.id);
            file.connections.push(policy);
            validate_connections(file)?;
            Ok(())
        })
    }

    /// Revoke a connection by ID.
    pub fn revoke(&mut self, id: &str) -> Result<(), PolicyError> {
        self.mutate(|file| {
            let policy = file
                .connections
                .iter_mut()
                .find(|existing| existing.id == id)
                .ok_or(PolicyError::UnknownConnection)?;
            policy.revoked = true;
            Ok(())
        })
    }

    /// Return all connection policies.
    pub fn connections(&self) -> Result<Vec<ConnectionPolicy>, PolicyError> {
        self.read(|file| Ok(file.connections.clone()))
    }

    /// Authorize a request and reserve its amount in the ledger.
    pub fn authorize(
        &mut self,
        client_pubkey: &str,
        method: &str,
        event_id: &str,
        amount_msat: u64,
        now: u64,
    ) -> Result<Authorization, PolicyError> {
        self.mutate(|file| {
            let index = file
                .connections
                .iter()
                .position(|policy| policy.client_pubkey == client_pubkey)
                .ok_or(PolicyError::Unauthorized)?;

            if file.connections[index].revoked {
                return Err(PolicyError::Unauthorized);
            }
            if let Some(expires_at) = file.connections[index].expires_at {
                if now >= expires_at {
                    return Err(PolicyError::Expired);
                }
            }
            if !file.connections[index]
                .allowed_methods
                .iter()
                .any(|allowed| allowed == method)
            {
                return Err(PolicyError::Restricted(method.to_owned()));
            }
            if file.events.iter().any(|seen| seen == event_id) {
                return Err(PolicyError::Replayed);
            }
            if let Some(cap) = file.connections[index].max_single_payment_msat {
                if amount_msat > cap {
                    return Err(PolicyError::QuotaExceeded);
                }
            }

            rollover_if_needed(file, index, now);

            if let Some(budget) = file.connections[index].budget_msat {
                if file.connections[index]
                    .used_msat
                    .saturating_add(amount_msat)
                    > budget
                {
                    return Err(PolicyError::QuotaExceeded);
                }
            }

            let connection_id = file.connections[index].id.clone();
            let period_start_secs = file.connections[index].period_start_secs;
            file.connections[index].used_msat = file.connections[index]
                .used_msat
                .saturating_add(amount_msat);
            file.events.push(event_id.to_owned());

            let authorization = Authorization {
                id: format!("{}:{}", connection_id, event_id),
                connection_id,
                period_start_secs,
                amount_msat,
            };
            file.reservations.push(authorization.clone());
            Ok(authorization)
        })
    }

    /// Commit an authorization after a successful spend.
    pub fn commit(&mut self, authorization_id: &str, actual_msat: u64) -> Result<(), PolicyError> {
        self.mutate(|file| {
            let index = file
                .reservations
                .iter()
                .position(|auth| auth.id == authorization_id)
                .ok_or(PolicyError::UnknownAuthorization)?;
            let reservation = file.reservations[index].clone();
            let charged_msat = actual_msat.min(reservation.amount_msat);
            let overage_msat = actual_msat.saturating_sub(reservation.amount_msat);
            adjust_used(
                file,
                &reservation.connection_id,
                reservation.period_start_secs,
                reservation.amount_msat,
                charged_msat,
                overage_msat,
            );
            file.reservations.remove(index);
            Ok(())
        })
    }

    /// Release an authorization after a failed spend.
    pub fn release(&mut self, authorization_id: &str) -> Result<(), PolicyError> {
        self.mutate(|file| {
            let index = file
                .reservations
                .iter()
                .position(|auth| auth.id == authorization_id)
                .ok_or(PolicyError::UnknownAuthorization)?;
            let reservation = file.reservations[index].clone();
            adjust_used(
                file,
                &reservation.connection_id,
                reservation.period_start_secs,
                reservation.amount_msat,
                0,
                0,
            );
            file.reservations.remove(index);
            Ok(())
        })
    }

    /// Return the configured fee reserve for a client public key.
    pub fn fee_reserve_for(&self, client_pubkey: &str) -> Result<Option<u64>, PolicyError> {
        self.read(|file| {
            Ok(file
                .connections
                .iter()
                .find(|policy| policy.client_pubkey == client_pubkey)
                .map(|policy| policy.max_fee_reserve_msat))
        })
    }

    /// Return the used budget for a connection ID.
    pub fn used_msat(&self, connection_id: &str) -> Result<Option<u64>, PolicyError> {
        self.read(|file| {
            Ok(file
                .connections
                .iter()
                .find(|policy| policy.id == connection_id)
                .map(|policy| policy.used_msat))
        })
    }

    /// Return recorded fee overage for a connection ID.
    pub fn fee_overage_msat(&self, connection_id: &str) -> Result<Option<u64>, PolicyError> {
        self.read(|file| {
            Ok(file
                .connections
                .iter()
                .find(|policy| policy.id == connection_id)
                .map(|policy| policy.fee_overage_msat))
        })
    }

    fn read<T>(
        &self,
        operation: impl FnOnce(&PolicyFile) -> Result<T, PolicyError>,
    ) -> Result<T, PolicyError> {
        let lock = self.open_lock()?;
        lock.lock_exclusive()
            .map_err(|error| PolicyError::Io(error.to_string()))?;
        let file = read_file(&self.path)?;
        validate_connections(&file)?;
        operation(&file)
    }

    fn mutate<T>(
        &mut self,
        operation: impl FnOnce(&mut PolicyFile) -> Result<T, PolicyError>,
    ) -> Result<T, PolicyError> {
        let lock = self.open_lock()?;
        lock.lock_exclusive()
            .map_err(|error| PolicyError::Io(error.to_string()))?;
        let mut file = read_file(&self.path)?;
        validate_connections(&file)?;
        let result = operation(&mut file)?;
        write_file(&self.path, &file)?;
        Ok(result)
    }

    fn open_lock(&self) -> Result<File, PolicyError> {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(|error| PolicyError::Io(error.to_string()))
    }
}

fn validate_connections(file: &PolicyFile) -> Result<(), PolicyError> {
    let mut ids = HashSet::new();
    let mut clients = HashSet::new();
    for policy in &file.connections {
        if !ids.insert(policy.id.as_str()) || !clients.insert(policy.client_pubkey.as_str()) {
            return Err(PolicyError::DuplicateClient);
        }
    }
    Ok(())
}

fn adjust_used(
    file: &mut PolicyFile,
    connection_id: &str,
    period_start_secs: u64,
    reserved: u64,
    actual: u64,
    overage: u64,
) {
    if let Some(policy) = file
        .connections
        .iter_mut()
        .find(|existing| existing.id == connection_id)
    {
        if policy.period_start_secs == period_start_secs {
            policy.used_msat = policy.used_msat.saturating_sub(reserved - actual);
        }
        policy.fee_overage_msat = policy.fee_overage_msat.saturating_add(overage);
    }
}

fn rollover_if_needed(file: &mut PolicyFile, index: usize, now: u64) {
    let Some(period) = file.connections[index].budget_period.seconds() else {
        return;
    };
    let start = file.connections[index].period_start_secs;
    if now >= start.saturating_add(period) {
        file.connections[index].period_start_secs = now;
        file.connections[index].used_msat = 0;
    }
}

fn read_file(path: &Path) -> Result<PolicyFile, PolicyError> {
    match fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|error| PolicyError::Serde(error.to_string()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PolicyFile::default()),
        Err(error) => Err(PolicyError::Io(error.to_string())),
    }
}

fn write_file(path: &Path, file: &PolicyFile) -> Result<(), PolicyError> {
    let json = serde_json::to_string_pretty(file)
        .map_err(|error| PolicyError::Serde(error.to_string()))?;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("policy-{}-{unique}.tmp", std::process::id()));

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|error| PolicyError::Io(error.to_string()))?;
        handle
            .write_all(json.as_bytes())
            .map_err(|error| PolicyError::Io(error.to_string()))?;
        handle
            .sync_all()
            .map_err(|error| PolicyError::Io(error.to_string()))?;
    }
    #[cfg(not(unix))]
    fs::write(&temp, &json).map_err(|error| PolicyError::Io(error.to_string()))?;

    fs::rename(&temp, path).map_err(|error| PolicyError::Io(error.to_string()))?;
    Ok(())
}

fn nip47_error(code: ErrorCode, message: &str) -> NIP47Error {
    NIP47Error {
        code,
        message: message.to_owned(),
    }
}

fn map_policy_error(error: PolicyError) -> NIP47Error {
    match error {
        PolicyError::Unauthorized | PolicyError::Expired => {
            nip47_error(ErrorCode::Unauthorized, "connection not authorized")
        }
        PolicyError::Restricted(method) => nip47_error(
            ErrorCode::Restricted,
            &format!("method not allowed: {method}"),
        ),
        PolicyError::QuotaExceeded => nip47_error(ErrorCode::QuotaExceeded, "budget exceeded"),
        PolicyError::Replayed => nip47_error(ErrorCode::RateLimited, "event replay detected"),
        other => nip47_error(ErrorCode::Internal, &other.to_string()),
    }
}

/// Wraps a CDK NWC handler with per-connection policy enforcement.
#[derive(Debug)]
pub struct PolicyNwcHandler<H> {
    store: Arc<Mutex<PolicyStore>>,
    inner: Arc<H>,
}

impl<H> Clone for PolicyNwcHandler<H> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            inner: self.inner.clone(),
        }
    }
}

impl<H> PolicyNwcHandler<H>
where
    H: NwcRequestHandler,
{
    /// Create a policy handler backed by a persistent ledger at `path`.
    pub fn new<P: AsRef<Path>>(path: P, inner: Arc<H>) -> Result<Self, PolicyError> {
        Ok(Self {
            store: Arc::new(Mutex::new(PolicyStore::new(path)?)),
            inner,
        })
    }

    /// Add or replace a connection policy.
    pub fn add_connection(&self, policy: ConnectionPolicy) -> Result<(), PolicyError> {
        self.store
            .lock()
            .map_err(|_| PolicyError::Io("policy lock poisoned".to_owned()))?
            .add_connection(policy)
    }

    /// Revoke a connection by ID.
    pub fn revoke(&self, id: &str) -> Result<(), PolicyError> {
        self.store
            .lock()
            .map_err(|_| PolicyError::Io("policy lock poisoned".to_owned()))?
            .revoke(id)
    }

    /// Return used millisatoshis for a connection ID.
    pub fn used_msat(&self, id: &str) -> Option<u64> {
        self.store.lock().ok()?.used_msat(id).ok().flatten()
    }

    /// Return recorded fee overage for a connection ID.
    pub fn fee_overage_msat(&self, id: &str) -> Option<u64> {
        self.store.lock().ok()?.fee_overage_msat(id).ok().flatten()
    }

    /// Return all connection policies.
    pub fn connections(&self) -> Result<Vec<ConnectionPolicy>, PolicyError> {
        self.store
            .lock()
            .map_err(|_| PolicyError::Io("policy lock poisoned".to_owned()))?
            .connections()
    }

    fn authorize(
        &self,
        ctx: &RequestContext,
        method: &str,
        amount_msat: u64,
    ) -> Result<Authorization, NIP47Error> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| nip47_error(ErrorCode::Internal, "policy lock poisoned"))?;
        store
            .authorize(
                &ctx.client_pubkey,
                method,
                &ctx.event_id,
                amount_msat,
                ctx.received_at,
            )
            .map_err(map_policy_error)
    }

    fn finish(&self, authorization: &Authorization, actual_msat: u64) -> Result<(), NIP47Error> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| nip47_error(ErrorCode::Internal, "policy lock poisoned"))?;
        store
            .commit(&authorization.id, actual_msat)
            .map_err(map_policy_error)
    }

    fn cancel(&self, authorization: &Authorization) {
        if let Ok(mut store) = self.store.lock() {
            let _ = store.release(&authorization.id);
        }
    }
}

#[async_trait]
impl<H> NwcRequestHandler for PolicyNwcHandler<H>
where
    H: NwcRequestHandler + Send + Sync,
{
    async fn get_info(&self) -> Result<GetInfoResponse, NIP47Error> {
        Err(nip47_error(ErrorCode::Internal, "policy context required"))
    }

    async fn get_balance(&self) -> Result<GetBalanceResponse, NIP47Error> {
        Err(nip47_error(ErrorCode::Internal, "policy context required"))
    }

    async fn make_invoice(
        &self,
        _request: MakeInvoiceRequest,
    ) -> Result<MakeInvoiceResponse, NIP47Error> {
        Err(nip47_error(ErrorCode::Internal, "policy context required"))
    }

    async fn pay_invoice(
        &self,
        _request: PayInvoiceRequest,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        Err(nip47_error(ErrorCode::Internal, "policy context required"))
    }

    async fn lookup_invoice(
        &self,
        _request: LookupInvoiceRequest,
    ) -> Result<LookupInvoiceResponse, NIP47Error> {
        Err(nip47_error(ErrorCode::Internal, "policy context required"))
    }

    async fn list_transactions(
        &self,
        _request: ListTransactionsRequest,
    ) -> Result<Vec<LookupInvoiceResponse>, NIP47Error> {
        Err(nip47_error(ErrorCode::Internal, "policy context required"))
    }

    async fn get_info_with_context(
        &self,
        ctx: &RequestContext,
    ) -> Result<GetInfoResponse, NIP47Error> {
        let auth = self.authorize(ctx, "get_info", 0)?;
        let result = self.inner.get_info().await;
        match result {
            Ok(mut response) => {
                let allowed = self
                    .store
                    .lock()
                    .ok()
                    .and_then(|store| store.connections().ok())
                    .and_then(|connections| {
                        connections
                            .into_iter()
                            .find(|policy| policy.client_pubkey == ctx.client_pubkey)
                            .map(|policy| policy.allowed_methods)
                    })
                    .unwrap_or_default();
                response.methods.retain(|method| {
                    allowed
                        .iter()
                        .any(|candidate| candidate == &method.to_string())
                });
                self.finish(&auth, 0)?;
                Ok(response)
            }
            Err(error) => {
                self.cancel(&auth);
                Err(error)
            }
        }
    }

    async fn get_balance_with_context(
        &self,
        ctx: &RequestContext,
    ) -> Result<GetBalanceResponse, NIP47Error> {
        let auth = self.authorize(ctx, "get_balance", 0)?;
        let result = self.inner.get_balance().await;
        match result {
            Ok(response) => {
                self.finish(&auth, 0)?;
                Ok(response)
            }
            Err(error) => {
                self.cancel(&auth);
                Err(error)
            }
        }
    }

    async fn make_invoice_with_context(
        &self,
        request: MakeInvoiceRequest,
        ctx: &RequestContext,
    ) -> Result<MakeInvoiceResponse, NIP47Error> {
        let auth = self.authorize(ctx, "make_invoice", 0)?;
        let result = self.inner.make_invoice(request).await;
        match result {
            Ok(response) => {
                self.finish(&auth, 0)?;
                Ok(response)
            }
            Err(error) => {
                self.cancel(&auth);
                Err(error)
            }
        }
    }

    async fn pay_invoice_with_context(
        &self,
        request: PayInvoiceRequest,
        ctx: &RequestContext,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        let invoice = Bolt11Invoice::from_str(&request.invoice).map_err(|error| {
            nip47_error(
                ErrorCode::Other,
                &format!("invalid bolt11 invoice: {error}"),
            )
        })?;
        let invoice_msat = invoice.amount_milli_satoshis().ok_or_else(|| {
            nip47_error(ErrorCode::Other, "amountless invoices are not supported")
        })?;
        if let Some(requested) = request.amount {
            if requested != invoice_msat {
                return Err(nip47_error(
                    ErrorCode::Other,
                    "requested amount does not match the invoice amount",
                ));
            }
        }

        let fee_reserve = self
            .store
            .lock()
            .ok()
            .and_then(|store| store.fee_reserve_for(&ctx.client_pubkey).ok().flatten())
            .unwrap_or(0);
        let auth = self.authorize(ctx, "pay_invoice", invoice_msat.saturating_add(fee_reserve))?;
        let result = self.inner.pay_invoice(request).await;
        match result {
            Ok(response) => {
                let actual = invoice_msat.saturating_add(response.fees_paid.unwrap_or(0));
                self.finish(&auth, actual)?;
                Ok(response)
            }
            Err(error) => {
                // A payment error is ambiguous: the wallet may have dispatched
                // the payment before failing to return a preimage. Charge the
                // reserved amount instead of releasing the budget.
                let reserved = invoice_msat.saturating_add(fee_reserve);
                let _ = self.finish(&auth, reserved);
                Err(error)
            }
        }
    }

    async fn lookup_invoice_with_context(
        &self,
        request: LookupInvoiceRequest,
        ctx: &RequestContext,
    ) -> Result<LookupInvoiceResponse, NIP47Error> {
        let auth = self.authorize(ctx, "lookup_invoice", 0)?;
        let result = self.inner.lookup_invoice(request).await;
        match result {
            Ok(response) => {
                self.finish(&auth, 0)?;
                Ok(response)
            }
            Err(error) => {
                self.cancel(&auth);
                Err(error)
            }
        }
    }

    async fn list_transactions_with_context(
        &self,
        request: ListTransactionsRequest,
        ctx: &RequestContext,
    ) -> Result<Vec<LookupInvoiceResponse>, NIP47Error> {
        let auth = self.authorize(ctx, "list_transactions", 0)?;
        let result = self.inner.list_transactions(request).await;
        match result {
            Ok(response) => {
                self.finish(&auth, 0)?;
                Ok(response)
            }
            Err(error) => {
                self.cancel(&auth);
                Err(error)
            }
        }
    }
}
