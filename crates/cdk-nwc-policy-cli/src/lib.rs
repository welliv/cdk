//! Helpers for the `nwc-policy-cli` binary, split out so they can be tested.

use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cdk::wallet::Wallet;
use cdk_nwc_policy::BudgetPeriod;
use tokio::task::JoinHandle;

/// Parse a `--budget-period` value.
pub fn parse_budget_period(value: &str) -> Result<BudgetPeriod> {
    match value.trim().to_ascii_lowercase().as_str() {
        "never" => Ok(BudgetPeriod::Never),
        "daily" => Ok(BudgetPeriod::Daily),
        "weekly" => Ok(BudgetPeriod::Weekly),
        "monthly" => Ok(BudgetPeriod::Monthly),
        "yearly" => Ok(BudgetPeriod::Yearly),
        other => {
            bail!("unknown budget period {other:?}; use never, daily, weekly, monthly or yearly")
        }
    }
}

/// Parse a duration such as `3600`, `90s`, `30m`, `12h`, `7d` or `2w` into seconds.
pub fn parse_duration_secs(value: &str) -> Result<u64> {
    let value = value.trim();
    if value.is_empty() {
        bail!("duration must not be empty");
    }
    let (number, multiplier) = match value.char_indices().last() {
        Some((index, 's')) => (&value[..index], 1),
        Some((index, 'm')) => (&value[..index], 60),
        Some((index, 'h')) => (&value[..index], 3_600),
        Some((index, 'd')) => (&value[..index], 86_400),
        Some((index, 'w')) => (&value[..index], 604_800),
        _ => (value, 1),
    };
    let number: u64 = number
        .parse()
        .with_context(|| format!("invalid duration {value:?}"))?;
    let secs = number
        .checked_mul(multiplier)
        .context("duration overflows")?;
    if secs == 0 {
        bail!("duration must be greater than zero");
    }
    Ok(secs)
}

/// Resolve `--expires-at` / `--expires-in` into an absolute unix timestamp.
pub fn resolve_expiry(
    expires_at: Option<u64>,
    expires_in: Option<&str>,
    now: u64,
) -> Result<Option<u64>> {
    match (expires_at, expires_in) {
        (Some(_), Some(_)) => bail!("use only one of --expires-at and --expires-in"),
        (Some(at), None) => {
            if at <= now {
                bail!("--expires-at must be in the future");
            }
            Ok(Some(at))
        }
        (None, Some(duration)) => {
            let secs = parse_duration_secs(duration)?;
            Ok(Some(now.checked_add(secs).context("expiry overflows")?))
        }
        (None, None) => Ok(None),
    }
}

/// Mint ecash for every paid-but-unissued mint quote once.
///
/// `make_invoice` creates a mint quote, but nothing turns a paid quote into
/// proofs on its own. Without this the wallet never sees incoming payments.
pub async fn claim_paid_quotes(wallet: &Wallet) -> Result<u64> {
    let minted = wallet
        .mint_unissued_quotes()
        .await
        .context("mint unissued quotes")?;
    Ok(u64::from(minted))
}

/// Create parent directories (`0700` on Unix) and ensure `path` exists with
/// `0600` permissions before SQLite opens it, preventing a world-readable DB.
pub fn ensure_private_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).context("create database directory")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                    .context("restrict database directory permissions")?;
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .context("create private wallet database file")?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .context("restrict wallet database permissions")?;
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .context("create wallet database file")?;
    }
    Ok(())
}

/// Restrict permissions on SQLite WAL/SHM sidecar files if present.
pub fn restrict_sqlite_sidecars(db_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let base = db_path.as_os_str().to_string_lossy();
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar_str = format!("{base}{suffix}");
            let sidecar = Path::new(&sidecar_str);
            if sidecar.exists() {
                fs::set_permissions(sidecar, fs::Permissions::from_mode(0o600))
                    .with_context(|| format!("restrict permissions on {}", sidecar.display()))?;
            }
        }
    }
    Ok(())
}

/// Bound a single claim pass so a stalled mint HTTP request cannot wedge the
/// claimer (and, with a shared per-host HTTP connection, every other mint call).
pub const CLAIM_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawn a background task that claims paid mint quotes every `interval`.
pub fn spawn_quote_claimer(wallet: Arc<Wallet>, interval: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match tokio::time::timeout(CLAIM_TIMEOUT, claim_paid_quotes(&wallet)).await {
                Ok(Ok(0)) => {}
                Ok(Ok(amount)) => println!("claimed {amount} sat from paid invoices"),
                Ok(Err(error)) => eprintln!("claiming paid invoices failed: {error:#}"),
                Err(_) => eprintln!(
                    "claiming paid invoices timed out after {}s",
                    CLAIM_TIMEOUT.as_secs()
                ),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_budget_period() {
        assert_eq!(parse_budget_period("never").unwrap(), BudgetPeriod::Never);
        assert_eq!(parse_budget_period("Daily").unwrap(), BudgetPeriod::Daily);
        assert_eq!(parse_budget_period("weekly").unwrap(), BudgetPeriod::Weekly);
        assert_eq!(
            parse_budget_period("monthly").unwrap(),
            BudgetPeriod::Monthly
        );
        assert_eq!(parse_budget_period("yearly").unwrap(), BudgetPeriod::Yearly);
        assert!(parse_budget_period("hourly").is_err());
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration_secs("3600").unwrap(), 3_600);
        assert_eq!(parse_duration_secs("90s").unwrap(), 90);
        assert_eq!(parse_duration_secs("30m").unwrap(), 1_800);
        assert_eq!(parse_duration_secs("12h").unwrap(), 43_200);
        assert_eq!(parse_duration_secs("7d").unwrap(), 604_800);
        assert_eq!(parse_duration_secs("2w").unwrap(), 1_209_600);
        assert!(parse_duration_secs("0").is_err());
        assert!(parse_duration_secs("abc").is_err());
        assert!(parse_duration_secs("-5m").is_err());
        assert!(parse_duration_secs("99999999999999999999w").is_err());
    }

    #[test]
    fn resolves_expiry() {
        assert_eq!(resolve_expiry(None, None, 1_000).unwrap(), None);
        assert_eq!(
            resolve_expiry(Some(2_000), None, 1_000).unwrap(),
            Some(2_000)
        );
        assert_eq!(
            resolve_expiry(None, Some("1h"), 1_000).unwrap(),
            Some(4_600)
        );
        assert!(resolve_expiry(Some(500), None, 1_000).is_err());
        assert!(resolve_expiry(Some(2_000), Some("1h"), 1_000).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wallet_db_and_sidecars_have_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("nested/wallet.sqlite");
        ensure_private_file(&db_path).expect("prepare private file");
        let _db = cdk_sqlite::WalletSqliteDatabase::new(&db_path)
            .await
            .expect("open sqlite");
        restrict_sqlite_sidecars(&db_path).expect("restrict sidecars");

        let mode = fs::metadata(&db_path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "wallet.sqlite must be 0600, got {mode:o}");
        for suffix in ["-wal", "-shm"] {
            let sidecar_str = format!("{}{suffix}", db_path.display());
            let sidecar = Path::new(&sidecar_str);
            if sidecar.exists() {
                let side_mode = fs::metadata(sidecar).expect("meta").permissions().mode() & 0o777;
                assert_eq!(side_mode, 0o600, "sidecar {suffix} must be 0600");
            }
        }
    }
}
