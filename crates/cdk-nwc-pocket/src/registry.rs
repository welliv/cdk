//! Main-wallet-side ledger of pockets and refill grants.
//!
//! The refill counter must live where the link holder cannot touch it. The
//! registry is a JSON file owned by the main wallet process, guarded by a
//! sidecar file lock and rewritten atomically.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Automatic refill settings for a pocket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AutoRefill {
    /// Refill when the pocket balance falls to or below this.
    pub threshold_msat: u64,
    /// Refill up to this balance (never above the ceiling or the cap).
    pub target_msat: u64,
}

/// Refill cap on a rolling window.
///
/// At most `cap_msat` may leave the main wallet for this pocket in any
/// `window_secs` interval. A rolling window is the true bound: unlike a
/// calendar period it cannot allow `2 × cap` across a boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct RefillPolicy {
    /// Maximum refilled per window, including transfer fees.
    pub cap_msat: u64,
    /// Window length in seconds.
    pub window_secs: u64,
    /// Optional automatic refill.
    pub auto_refill: Option<AutoRefill>,
}

/// One pocket, keyed by NWC connection ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PocketConfig {
    /// NWC connection ID.
    pub connection_id: String,
    /// Seed derivation index (never reused).
    pub index: u32,
    /// Maximum pocket balance; incoming funds above it are swept to main.
    pub ceiling_msat: u64,
    /// Refill policy.
    pub refill: RefillPolicy,
    /// When the pocket was fully swept after revoke or expiry.
    #[serde(default)]
    pub swept_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Grant {
    id: u64,
    connection_id: String,
    at: u64,
    msat: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    pockets: Vec<PocketConfig>,
    #[serde(default)]
    grants: Vec<Grant>,
    #[serde(default)]
    next_grant_id: u64,
}

/// File-backed pocket registry.
#[derive(Debug, Clone)]
pub struct PocketRegistry {
    path: PathBuf,
    lock_path: PathBuf,
}

impl PocketRegistry {
    /// Open a registry at `path`, creating parent directories if needed.
    pub fn new<P>(path: P) -> Result<Self, Error>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| Error::Io(e.to_string()))?;
            }
        }
        let lock_path = path.with_extension("pockets.lock");
        Ok(Self { path, lock_path })
    }

    /// Register a pocket. Fails if the connection ID or index is taken.
    pub fn add(&self, config: PocketConfig) -> Result<(), Error> {
        self.mutate(|file| {
            if file
                .pockets
                .iter()
                .any(|p| p.connection_id == config.connection_id || p.index == config.index)
            {
                return Err(Error::DuplicatePocket);
            }
            file.pockets.push(config);
            Ok(())
        })
    }

    /// Return the pocket for a connection.
    pub fn get(&self, connection_id: &str) -> Result<PocketConfig, Error> {
        self.read(|file| {
            file.pockets
                .iter()
                .find(|p| p.connection_id == connection_id)
                .cloned()
                .ok_or_else(|| Error::UnknownPocket {
                    connection_id: connection_id.to_owned(),
                })
        })
    }

    /// Return every pocket.
    pub fn list(&self) -> Result<Vec<PocketConfig>, Error> {
        self.read(|file| Ok(file.pockets.clone()))
    }

    /// Next unused derivation index. Indexes of removed or swept pockets are
    /// never reused.
    pub fn next_index(&self) -> Result<u32, Error> {
        self.read(|file| {
            Ok(file
                .pockets
                .iter()
                .map(|p| p.index.saturating_add(1))
                .max()
                .unwrap_or(0))
        })
    }

    /// Replace the refill policy and ceiling of a pocket. Past grants keep
    /// counting against the window.
    pub fn update_limits(
        &self,
        connection_id: &str,
        ceiling_msat: u64,
        refill: RefillPolicy,
    ) -> Result<(), Error> {
        self.mutate(|file| {
            let pocket = find_mut(file, connection_id)?;
            pocket.ceiling_msat = ceiling_msat;
            pocket.refill = refill;
            Ok(())
        })
    }

    /// Mark a pocket as fully swept.
    pub fn mark_swept(&self, connection_id: &str, now: u64) -> Result<(), Error> {
        self.mutate(|file| {
            find_mut(file, connection_id)?.swept_at = Some(now);
            Ok(())
        })
    }

    /// Refill allowance left in the window ending at `now`.
    pub fn remaining_msat(&self, connection_id: &str, now: u64) -> Result<u64, Error> {
        self.read(|file| {
            let pocket = find(file, connection_id)?;
            Ok(remaining(file, pocket, now))
        })
    }

    /// When the oldest grant in the current window expires, if any.
    pub fn resets_at(&self, connection_id: &str, now: u64) -> Result<Option<u64>, Error> {
        self.read(|file| {
            let pocket = find(file, connection_id)?;
            let start = now.saturating_sub(pocket.refill.window_secs);
            Ok(file
                .grants
                .iter()
                .filter(|g| g.connection_id == connection_id && g.at > start)
                .map(|g| g.at.saturating_add(pocket.refill.window_secs))
                .min())
        })
    }

    /// Atomically grant up to `requested_msat` from the window allowance.
    ///
    /// Returns the grant ID and granted amount. A granted amount of zero
    /// records nothing.
    pub fn try_grant(
        &self,
        connection_id: &str,
        requested_msat: u64,
        now: u64,
    ) -> Result<(u64, u64), Error> {
        self.mutate(|file| {
            let pocket = find(file, connection_id)?;
            let granted = requested_msat.min(remaining(file, pocket, now));
            if granted == 0 {
                return Ok((0, 0));
            }
            // Grant ID 0 is reserved for "nothing granted".
            let id = file.next_grant_id.max(1);
            file.next_grant_id = id.saturating_add(1);
            file.grants.push(Grant {
                id,
                connection_id: connection_id.to_owned(),
                at: now,
                msat: granted,
            });
            Ok((id, granted))
        })
    }

    /// Lower a grant to what was actually spent, or drop it if zero.
    pub fn settle_grant(&self, grant_id: u64, actual_msat: u64) -> Result<(), Error> {
        if grant_id == 0 {
            return Ok(());
        }
        self.mutate(|file| {
            if actual_msat == 0 {
                file.grants.retain(|g| g.id != grant_id);
            } else if let Some(grant) = file.grants.iter_mut().find(|g| g.id == grant_id) {
                grant.msat = grant.msat.min(actual_msat);
            }
            Ok(())
        })
    }

    fn read<T>(&self, op: impl FnOnce(&RegistryFile) -> Result<T, Error>) -> Result<T, Error> {
        let lock = self.open_lock()?;
        FileExt::lock_shared(&lock).map_err(|e| Error::Io(e.to_string()))?;
        let file = read_file(&self.path)?;
        op(&file)
    }

    fn mutate<T>(
        &self,
        op: impl FnOnce(&mut RegistryFile) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let lock = self.open_lock()?;
        FileExt::lock_exclusive(&lock).map_err(|e| Error::Io(e.to_string()))?;
        let mut file = read_file(&self.path)?;
        let result = op(&mut file)?;
        write_file(&self.path, &file)?;
        Ok(result)
    }

    fn open_lock(&self) -> Result<File, Error> {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(|e| Error::Io(e.to_string()))
    }
}

fn find<'a>(file: &'a RegistryFile, connection_id: &str) -> Result<&'a PocketConfig, Error> {
    file.pockets
        .iter()
        .find(|p| p.connection_id == connection_id)
        .ok_or_else(|| Error::UnknownPocket {
            connection_id: connection_id.to_owned(),
        })
}

fn find_mut<'a>(
    file: &'a mut RegistryFile,
    connection_id: &str,
) -> Result<&'a mut PocketConfig, Error> {
    file.pockets
        .iter_mut()
        .find(|p| p.connection_id == connection_id)
        .ok_or_else(|| Error::UnknownPocket {
            connection_id: connection_id.to_owned(),
        })
}

fn remaining(file: &RegistryFile, pocket: &PocketConfig, now: u64) -> u64 {
    let start = now.saturating_sub(pocket.refill.window_secs);
    let used: u64 = file
        .grants
        .iter()
        .filter(|g| g.connection_id == pocket.connection_id && g.at > start)
        .map(|g| g.msat)
        .fold(0, u64::saturating_add);
    pocket.refill.cap_msat.saturating_sub(used)
}

fn read_file(path: &Path) -> Result<RegistryFile, Error> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| Error::Serde(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RegistryFile::default()),
        Err(e) => Err(Error::Io(e.to_string())),
    }
}

fn write_file(path: &Path, file: &RegistryFile) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(file).map_err(|e| Error::Serde(e.to_string()))?;
    let temp = path.with_extension(format!("pockets-{}.tmp", std::process::id()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut handle = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|e| Error::Io(e.to_string()))?;
        handle
            .write_all(json.as_bytes())
            .map_err(|e| Error::Io(e.to_string()))?;
        handle.sync_all().map_err(|e| Error::Io(e.to_string()))?;
    }
    #[cfg(not(unix))]
    fs::write(&temp, &json).map_err(|e| Error::Io(e.to_string()))?;
    fs::rename(&temp, path).map_err(|e| Error::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pocket(id: &str, index: u32, cap: u64, window: u64) -> PocketConfig {
        PocketConfig {
            connection_id: id.to_owned(),
            index,
            ceiling_msat: 1_000_000,
            refill: RefillPolicy {
                cap_msat: cap,
                window_secs: window,
                auto_refill: None,
            },
            swept_at: None,
        }
    }

    #[test]
    fn rolling_window_caps_grants_and_frees_them_later() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = PocketRegistry::new(dir.path().join("pockets.json")).expect("registry");
        registry.add(pocket("a", 0, 10_000, 100)).expect("add");

        assert_eq!(
            registry.try_grant("a", 6_000, 1_000).expect("grant").1,
            6_000
        );
        // Only 4_000 left in the window, however much is asked for.
        assert_eq!(
            registry.try_grant("a", 9_000, 1_050).expect("grant").1,
            4_000
        );
        assert_eq!(registry.try_grant("a", 1, 1_060).expect("grant").1, 0);
        assert_eq!(registry.resets_at("a", 1_060).expect("resets"), Some(1_100));

        // The first grant leaves the window; the second still counts.
        assert_eq!(
            registry.remaining_msat("a", 1_100).expect("remaining"),
            6_000
        );
        // A calendar boundary would allow a fresh cap here; a rolling window
        // still counts the grant made at 1_050.
        assert_eq!(
            registry.try_grant("a", 10_000, 1_101).expect("grant").1,
            6_000
        );
        assert_eq!(
            registry.remaining_msat("a", 1_151).expect("remaining"),
            4_000
        );
    }

    /// Regression: settling a "nothing granted" result must not drop the
    /// first real grant (which once also had ID 0) and free its allowance.
    #[test]
    fn empty_grant_settle_keeps_existing_grants() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = PocketRegistry::new(dir.path().join("pockets.json")).expect("registry");
        registry.add(pocket("a", 0, 10_000, 100)).expect("add");

        let (first, granted) = registry.try_grant("a", 10_000, 1_000).expect("grant");
        assert_eq!(granted, 10_000);
        assert_ne!(first, 0);
        let (none, nothing) = registry.try_grant("a", 5_000, 1_001).expect("grant");
        assert_eq!((none, nothing), (0, 0));
        registry.settle_grant(none, 0).expect("settle");
        assert_eq!(registry.remaining_msat("a", 1_002).expect("remaining"), 0);
    }

    #[test]
    fn settle_grant_returns_unused_allowance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = PocketRegistry::new(dir.path().join("pockets.json")).expect("registry");
        registry.add(pocket("a", 0, 10_000, 100)).expect("add");
        let (id, granted) = registry.try_grant("a", 8_000, 10).expect("grant");
        assert_eq!(granted, 8_000);
        registry.settle_grant(id, 5_000).expect("settle");
        assert_eq!(registry.remaining_msat("a", 11).expect("remaining"), 5_000);
        registry.settle_grant(id, 0).expect("drop");
        assert_eq!(registry.remaining_msat("a", 11).expect("remaining"), 10_000);
    }

    #[test]
    fn indexes_are_unique_and_never_reused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = PocketRegistry::new(dir.path().join("pockets.json")).expect("registry");
        assert_eq!(registry.next_index().expect("index"), 0);
        registry.add(pocket("a", 0, 1, 1)).expect("add");
        registry.add(pocket("b", 3, 1, 1)).expect("add");
        assert_eq!(registry.next_index().expect("index"), 4);
        assert!(matches!(
            registry.add(pocket("c", 3, 1, 1)),
            Err(Error::DuplicatePocket)
        ));
        assert!(matches!(
            registry.add(pocket("a", 9, 1, 1)),
            Err(Error::DuplicatePocket)
        ));
        registry.mark_swept("a", 5).expect("swept");
        assert_eq!(registry.get("a").expect("get").swept_at, Some(5));
        assert_eq!(registry.next_index().expect("index"), 4);
    }
}
