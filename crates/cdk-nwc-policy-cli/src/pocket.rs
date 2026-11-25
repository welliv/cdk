//! Pocket mode: each connection gets its own Cashu wallet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use cdk::nuts::CurrencyUnit;
use cdk::wallet::Wallet;
use cdk_nwc_pocket::{derive_pocket_seed, PocketNwcHandler, PocketRegistry, MSAT_PER_SAT};
use cdk_nwc_policy::ConnectionPolicy;

use crate::{ensure_private_file, restrict_sqlite_sidecars, CLAIM_TIMEOUT};

/// Database path of pocket `index`, next to the main wallet database.
pub fn pocket_db_path(main_db: &Path, index: u32) -> PathBuf {
    let mut name = main_db.as_os_str().to_owned();
    name.push(format!(".pocket-{index}.sqlite"));
    PathBuf::from(name)
}

/// Default pocket registry path, next to the policy ledger.
pub fn registry_path(policy_path: &Path) -> PathBuf {
    policy_path.with_extension("pockets.json")
}

/// Open a private sqlite-backed sat wallet.
pub async fn open_wallet(mint_url: &str, db: &Path, seed: [u8; 64]) -> Result<Arc<Wallet>> {
    ensure_private_file(db)?;
    let database = cdk_sqlite::WalletSqliteDatabase::new(db.to_path_buf())
        .await
        .context("open wallet database")?;
    restrict_sqlite_sidecars(db)?;
    let wallet = Wallet::new(mint_url, CurrencyUnit::Sat, Arc::new(database), seed, None)
        .context("build CDK wallet")?;
    Ok(Arc::new(wallet))
}

/// Open the wallet of pocket `index`.
pub async fn open_pocket(
    mint_url: &str,
    main_db: &Path,
    main_seed: &[u8; 64],
    index: u32,
) -> Result<Arc<Wallet>> {
    open_wallet(
        mint_url,
        &pocket_db_path(main_db, index),
        derive_pocket_seed(main_seed, index),
    )
    .await
}

/// Whether a connection may no longer spend: revoked, expired or gone.
pub fn is_closed(policy: Option<&ConnectionPolicy>, now: u64) -> bool {
    match policy {
        None => true,
        Some(policy) => policy.revoked || policy.expires_at.is_some_and(|at| now >= at),
    }
}

/// One maintenance pass over running pockets.
///
/// - Closed (revoked/expired) pockets are swept to `main` and marked swept
///   once nothing is pending.
/// - Open pockets mint paid `make_invoice` quotes and send anything above
///   their ceiling to `main`.
pub async fn maintain_pockets(
    main: &Wallet,
    registry: &PocketRegistry,
    policies: &[ConnectionPolicy],
    pockets: &HashMap<String, Arc<PocketNwcHandler>>,
    now: u64,
) -> Result<()> {
    for config in registry.list()? {
        if config.swept_at.is_some() {
            continue;
        }
        let Some(handler) = pockets.get(&config.connection_id) else {
            continue;
        };
        let policy = policies.iter().find(|p| p.id == config.connection_id);
        if is_closed(policy, now) {
            let report = tokio::time::timeout(CLAIM_TIMEOUT, handler.sweep_to(main))
                .await
                .context("pocket sweep timed out")??;
            if report.is_complete() {
                registry.mark_swept(&config.connection_id, now)?;
            }
            println!(
                "pocket {}: closed, swept {} sat to main ({} sat pending)",
                config.connection_id,
                report.swept_msat / MSAT_PER_SAT,
                report.pending_msat / MSAT_PER_SAT
            );
            continue;
        }
        let pass = tokio::time::timeout(
            CLAIM_TIMEOUT,
            handler.claim_and_rebalance(main, config.ceiling_msat),
        )
        .await;
        match pass {
            Ok(Ok((claimed, moved))) => {
                if claimed > 0 || moved > 0 {
                    println!(
                        "pocket {}: claimed {} sat, moved {} sat above ceiling to main",
                        config.connection_id,
                        claimed / MSAT_PER_SAT,
                        moved / MSAT_PER_SAT
                    );
                }
            }
            Ok(Err(error)) => eprintln!(
                "pocket {} maintenance failed: {error}",
                config.connection_id
            ),
            Err(_) => eprintln!("pocket {} maintenance timed out", config.connection_id),
        }
    }
    Ok(())
}

/// Interval for [`maintain_pockets`] when no claim interval is configured.
pub const DEFAULT_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pocket_db_sits_next_to_main_db() {
        assert_eq!(
            pocket_db_path(Path::new("/w/wallet.sqlite"), 3),
            PathBuf::from("/w/wallet.sqlite.pocket-3.sqlite")
        );
        assert_eq!(
            registry_path(Path::new("/w/nwc-policy.json")),
            PathBuf::from("/w/nwc-policy.pockets.json")
        );
    }

    #[test]
    fn closed_means_revoked_expired_or_missing() {
        let mut policy = ConnectionPolicy {
            id: "a".to_owned(),
            name: "a".to_owned(),
            client_pubkey: "a".to_owned(),
            allowed_methods: vec![],
            budget_msat: None,
            budget_period: cdk_nwc_policy::BudgetPeriod::Never,
            max_single_payment_msat: None,
            max_fee_reserve_msat: 0,
            fee_overage_msat: 0,
            expires_at: Some(100),
            revoked: false,
            used_msat: 0,
            period_start_secs: 0,
        };
        assert!(!is_closed(Some(&policy), 99));
        assert!(is_closed(Some(&policy), 100));
        policy.expires_at = None;
        policy.revoked = true;
        assert!(is_closed(Some(&policy), 0));
        assert!(is_closed(None, 0));
    }
}
