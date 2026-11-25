//! Helpers for the `nwc-policy-cli` binary, split out so they can be tested.

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

/// Spawn a background task that claims paid mint quotes every `interval`.
pub fn spawn_quote_claimer(wallet: Arc<Wallet>, interval: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match claim_paid_quotes(&wallet).await {
                Ok(0) => {}
                Ok(amount) => println!("claimed {amount} sat from paid invoices"),
                Err(error) => eprintln!("claiming paid invoices failed: {error:#}"),
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
}
