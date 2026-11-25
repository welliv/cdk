//! Per-app NWC connection manager.

#![allow(missing_docs)]
#![allow(missing_debug_implementations)]

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cdk_nostr::nwc::{NwcRequestHandler, NwcService, NwcServiceConfig};
use fs2::FileExt;
use nostr::prelude::{Keys, RelayUrl, SecretKey, Timestamp};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{ConnectionPolicy, PolicyError, PolicyNwcHandler, PolicyStore};

/// Manager errors.
#[derive(Debug, Error)]
pub enum ManagerError {
    /// Disk input/output error.
    #[error("io error: {0}")]
    Io(String),
    /// Ledger serialization error.
    #[error("serialization error: {0}")]
    Serde(String),
    /// Policy layer error.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// NWC service error.
    #[error("nwc service error: {0}")]
    Nwc(String),
    /// Invalid relay URL.
    #[error("invalid relay: {0}")]
    InvalidRelay(String),
    /// Invalid stored key.
    #[error("invalid stored key: {0}")]
    InvalidKey(String),
    /// Unknown connection ID.
    #[error("connection not found")]
    UnknownConnection,
    /// Connection is already running.
    #[error("connection already running")]
    AlreadyRunning,
    /// Connection is revoked, expired or has no policy.
    #[error("connection not authorized")]
    Unauthorized,
}

/// Persisted metadata for one NWC app connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionRecord {
    /// Connection ID, equal to the client public key.
    pub id: String,
    /// Human-readable app name.
    pub name: String,
    /// Client public key in hex.
    pub client_pubkey: String,
    /// Client secret in hex, needed only to rebuild the service config.
    pub client_secret_hex: String,
    /// Wallet service public key in hex.
    pub service_pubkey: String,
    /// Wallet service secret in hex.
    pub service_secret_hex: String,
    /// Relays used by this connection.
    pub relays: Vec<String>,
    /// Creation time in unix seconds.
    pub created_at: u64,
}

/// Redacted connection view for listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionRecordView {
    /// Connection ID.
    pub id: String,
    /// Human-readable app name.
    pub name: String,
    /// Client public key in hex.
    pub client_pubkey: String,
    /// Wallet service public key in hex.
    pub service_pubkey: String,
    /// Relays used by this connection.
    pub relays: Vec<String>,
    /// Creation time in unix seconds.
    pub created_at: u64,
}

impl From<&ConnectionRecord> for ConnectionRecordView {
    fn from(record: &ConnectionRecord) -> Self {
        Self {
            id: record.id.clone(),
            name: record.name.clone(),
            client_pubkey: record.client_pubkey.clone(),
            service_pubkey: record.service_pubkey.clone(),
            relays: record.relays.clone(),
            created_at: record.created_at,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ManagerFile {
    connections: Vec<ConnectionRecord>,
}

struct ServiceHandle {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

/// Creates, stores and runs per-app NWC connections.
pub struct ConnectionManager<H> {
    manager_path: PathBuf,
    lock_path: PathBuf,
    policy_handler: Arc<PolicyNwcHandler<H>>,
    records: Vec<ConnectionRecord>,
    tasks: HashMap<String, ServiceHandle>,
}

impl<H> ConnectionManager<H>
where
    H: NwcRequestHandler + Send + Sync + 'static,
{
    /// Open a manager using separate manager and policy ledger paths.
    pub fn new<P: AsRef<Path>, Q: AsRef<Path>>(
        manager_path: P,
        policy_path: Q,
        inner: Arc<H>,
    ) -> Result<Self, ManagerError> {
        let manager_path = manager_path.as_ref().to_path_buf();
        let lock_path = manager_path.with_extension("manager.lock");
        let lock = open_lock(&lock_path)?;
        lock.lock_exclusive()
            .map_err(|error| ManagerError::Io(error.to_string()))?;
        let file = read_manager_file(&manager_path)?;
        drop(lock);

        let policy_handler = Arc::new(PolicyNwcHandler::new(policy_path, inner)?);
        Ok(Self {
            manager_path,
            lock_path,
            policy_handler,
            records: file.connections,
            tasks: HashMap::new(),
        })
    }

    /// Create a connection and return its `nostr+walletconnect://` URI.
    pub fn create_connection(
        &mut self,
        name: String,
        relays: Vec<String>,
        mut policy: ConnectionPolicy,
    ) -> Result<String, ManagerError> {
        if relays.is_empty() {
            return Err(ManagerError::InvalidRelay(
                "at least one relay is required".to_owned(),
            ));
        }

        let client_secret = SecretKey::generate();
        let client_keys = Keys::new(client_secret.clone());
        let client_pubkey = client_keys.public_key().to_hex();

        let service_secret = SecretKey::generate();
        let service_keys = Keys::new(service_secret.clone());

        let id = client_pubkey.clone();
        policy.id = id.clone();
        policy.name = name.clone();
        policy.client_pubkey = client_pubkey.clone();
        policy.revoked = false;

        let parsed_relays = relays
            .iter()
            .map(|relay| RelayUrl::parse(relay))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| ManagerError::InvalidRelay(error.to_string()))?;
        let service = NwcService::new(NwcServiceConfig {
            service_keys: service_keys.clone(),
            client_secret: client_secret.clone(),
            relays: parsed_relays,
            lud16: None,
        })
        .map_err(|error| ManagerError::Nwc(error.to_string()))?
        .with_allowed_methods(policy.allowed_methods.clone());

        let record = ConnectionRecord {
            id: id.clone(),
            name,
            client_pubkey,
            client_secret_hex: client_secret.to_secret_hex(),
            service_pubkey: service_keys.public_key().to_hex(),
            service_secret_hex: service_secret.to_secret_hex(),
            relays,
            created_at: Timestamp::now().as_secs(),
        };
        let uri = service.connection_uri().to_string();

        // Locked read-modify-write so concurrent `create_connection` calls from
        // separate processes cannot overwrite each other's records.
        self.mutate_records(|records| {
            records.retain(|existing| existing.id != id);
            records.push(record.clone());
        })?;
        if let Err(error) = self.policy_handler.add_connection(policy) {
            let _ = self.mutate_records(|records| {
                records.retain(|existing| existing.id != id);
            });
            return Err(error.into());
        }
        Ok(uri)
    }

    /// List full internal records.
    pub fn list_connections(&self) -> Vec<ConnectionRecord> {
        self.records.clone()
    }

    /// List redacted records for operators.
    pub fn list_connections_view(&self) -> Vec<ConnectionRecordView> {
        self.records
            .iter()
            .map(ConnectionRecordView::from)
            .collect()
    }

    /// Find a stored connection by ID.
    pub fn connection(&self, id: &str) -> Option<&ConnectionRecord> {
        self.records.iter().find(|record| record.id == id)
    }

    /// Read redacted records without constructing a wallet handler.
    pub fn list_records<P: AsRef<Path>>(
        manager_path: P,
    ) -> Result<Vec<ConnectionRecordView>, ManagerError> {
        let manager_path = manager_path.as_ref();
        let lock = open_lock(&manager_path.with_extension("manager.lock"))?;
        lock.lock_exclusive()
            .map_err(|error| ManagerError::Io(error.to_string()))?;
        let file = read_manager_file(manager_path)?;
        Ok(file
            .connections
            .iter()
            .map(ConnectionRecordView::from)
            .collect())
    }

    /// Revoke a policy without constructing a wallet handler.
    pub fn revoke_policy<P: AsRef<Path>>(policy_path: P, id: &str) -> Result<(), ManagerError> {
        let mut store = PolicyStore::new(policy_path)?;
        store.revoke(id)?;
        Ok(())
    }

    /// Revoke a connection. The NWC service is left running so subsequent
    /// requests receive a NIP-47 `UNAUTHORIZED` reply instead of timing out.
    pub async fn revoke(&mut self, id: &str) -> Result<(), ManagerError> {
        self.policy_handler.revoke(id)?;
        Ok(())
    }

    /// Start one NWC service for a stored connection.
    pub async fn start(&mut self, id: &str) -> Result<(), ManagerError> {
        if self.tasks.contains_key(id) {
            return Err(ManagerError::AlreadyRunning);
        }
        // Start revoked and expired connections too: the policy handler still
        // rejects every request, but the running service can return a NIP-47
        // UNAUTHORIZED reply instead of leaving the client to time out.
        let policy = self.policy_for(id)?;
        self.refresh_records()?;
        let record = self
            .records
            .iter()
            .find(|record| record.id == id)
            .ok_or(ManagerError::UnknownConnection)?
            .clone();
        let (service_secret, client_secret, relays) = validate_record(&record)?;

        let service = NwcService::new(NwcServiceConfig {
            service_keys: Keys::new(service_secret),
            client_secret,
            relays,
            lud16: None,
        })
        .map_err(|error| ManagerError::Nwc(error.to_string()))?
        .with_allowed_methods(policy.allowed_methods);

        let cancel = CancellationToken::new();
        let handler = self.policy_handler.clone();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            let _ = service.run(handler, task_cancel).await;
        });
        self.tasks
            .insert(id.to_owned(), ServiceHandle { cancel, task });
        Ok(())
    }

    /// Start every stored connection that has a policy, including revoked and
    /// expired ones, and return the number started. Revoked/expired services
    /// still reject every request, but they can answer with `UNAUTHORIZED`.
    pub async fn start_all(&mut self) -> Result<usize, ManagerError> {
        self.refresh_records()?;
        let policies = self.policy_handler.connections()?;
        let mut ids = Vec::new();
        for record in &self.records {
            if policies.iter().any(|policy| policy.id == record.id) {
                validate_record(record)?;
                ids.push(record.id.clone());
            }
        }

        let mut started = 0;
        for id in ids {
            if let Err(error) = self.start(&id).await {
                self.stop_all().await;
                return Err(error);
            }
            started += 1;
        }
        Ok(started)
    }

    /// Number of currently running services.
    pub fn active_count(&self) -> usize {
        self.tasks.len()
    }

    /// Stop a running connection service.
    pub async fn stop(&mut self, id: &str) -> Result<(), ManagerError> {
        let handle = self
            .tasks
            .remove(id)
            .ok_or(ManagerError::UnknownConnection)?;
        handle.cancel.cancel();
        let _ = handle.task.await;
        Ok(())
    }

    /// Stop every running connection service.
    pub async fn stop_all(&mut self) {
        let handles: Vec<(String, ServiceHandle)> = self.tasks.drain().collect();
        for (_, handle) in handles {
            handle.cancel.cancel();
            let _ = handle.task.await;
        }
    }

    /// Return the shared policy handler.
    pub fn policy_handler(&self) -> &Arc<PolicyNwcHandler<H>> {
        &self.policy_handler
    }

    fn policy_for(&self, id: &str) -> Result<ConnectionPolicy, ManagerError> {
        self.policy_handler
            .connections()?
            .into_iter()
            .find(|policy| policy.id == id)
            .ok_or(ManagerError::UnknownConnection)
    }

    fn refresh_records(&mut self) -> Result<(), ManagerError> {
        let lock = open_lock(&self.lock_path)?;
        lock.lock_exclusive()
            .map_err(|error| ManagerError::Io(error.to_string()))?;
        let file = read_manager_file(&self.manager_path)?;
        self.records = file.connections;
        Ok(())
    }

    fn mutate_records(
        &mut self,
        operation: impl FnOnce(&mut Vec<ConnectionRecord>),
    ) -> Result<(), ManagerError> {
        let lock = open_lock(&self.lock_path)?;
        lock.lock_exclusive()
            .map_err(|error| ManagerError::Io(error.to_string()))?;
        let mut file = read_manager_file(&self.manager_path)?;
        operation(&mut file.connections);
        write_manager_file(&self.manager_path, &file)?;
        self.records = file.connections;
        Ok(())
    }
}

impl<H> Drop for ConnectionManager<H> {
    fn drop(&mut self) {
        for handle in self.tasks.values() {
            handle.cancel.cancel();
        }
    }
}

fn validate_record(
    record: &ConnectionRecord,
) -> Result<(SecretKey, SecretKey, Vec<RelayUrl>), ManagerError> {
    let service_secret = SecretKey::from_slice(
        &hex::decode(&record.service_secret_hex)
            .map_err(|error| ManagerError::InvalidKey(error.to_string()))?,
    )
    .map_err(|error| ManagerError::InvalidKey(error.to_string()))?;
    let client_secret = SecretKey::from_slice(
        &hex::decode(&record.client_secret_hex)
            .map_err(|error| ManagerError::InvalidKey(error.to_string()))?,
    )
    .map_err(|error| ManagerError::InvalidKey(error.to_string()))?;
    let relays = record
        .relays
        .iter()
        .map(|relay| RelayUrl::parse(relay))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ManagerError::InvalidRelay(error.to_string()))?;
    Ok((service_secret, client_secret, relays))
}

fn open_lock(path: &Path) -> Result<File, ManagerError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| ManagerError::Io(error.to_string()))?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| ManagerError::Io(error.to_string()))
}

fn read_manager_file(path: &Path) -> Result<ManagerFile, ManagerError> {
    match fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|error| ManagerError::Serde(error.to_string()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ManagerFile::default()),
        Err(error) => Err(ManagerError::Io(error.to_string())),
    }
}

fn write_manager_file(path: &Path, file: &ManagerFile) -> Result<(), ManagerError> {
    let json = serde_json::to_string_pretty(file)
        .map_err(|error| ManagerError::Serde(error.to_string()))?;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("manager-{}-{unique}.tmp", std::process::id()));

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|error| ManagerError::Io(error.to_string()))?;
        handle
            .write_all(json.as_bytes())
            .map_err(|error| ManagerError::Io(error.to_string()))?;
        handle
            .sync_all()
            .map_err(|error| ManagerError::Io(error.to_string()))?;
    }
    #[cfg(not(unix))]
    fs::write(&temp, &json).map_err(|error| ManagerError::Io(error.to_string()))?;

    fs::rename(&temp, path).map_err(|error| ManagerError::Io(error.to_string()))?;
    Ok(())
}
