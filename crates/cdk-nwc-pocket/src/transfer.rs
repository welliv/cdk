//! Moving value between the main wallet and a pocket.
//!
//! All transfers are same-mint swaps: no Lightning round-trip, and the only
//! cost is the keyset input fee. Sweeping swaps the pocket's proofs into the
//! main wallet, so any copy of those proofs becomes worthless at the mint.

use cdk::nuts::nut00::ProofsMethods;
use cdk::nuts::{CurrencyUnit, Proofs};
use cdk::wallet::{ReceiveOptions, SendOptions};
use cdk::{Amount, Wallet};

use crate::error::Error;
use crate::MSAT_PER_SAT;

/// Result of a sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SweepReport {
    /// Value received into the main wallet, after input fees.
    pub swept_msat: u64,
    /// Value still pending or reserved in the pocket (for example an
    /// unresolved melt). It counts as spent until the mint resolves it and is
    /// picked up by a later sweep.
    pub pending_msat: u64,
}

impl SweepReport {
    /// Whether nothing is left in the pocket.
    pub fn is_complete(&self) -> bool {
        self.pending_msat == 0
    }
}

pub(crate) fn ensure_sat(wallet: &Wallet) -> Result<(), Error> {
    match wallet.unit {
        CurrencyUnit::Sat => Ok(()),
        _ => Err(Error::UnsupportedUnit),
    }
}

/// Unspent pocket balance in millisatoshis.
pub async fn balance_msat(wallet: &Wallet) -> Result<u64, Error> {
    Ok(u64::from(wallet.total_balance().await?).saturating_mul(MSAT_PER_SAT))
}

/// Move up to `amount` sats from `main` into `pocket`.
///
/// The main wallet pays the swap fees, so the pocket receives the amount
/// sent. Returns `(cost, received)` in sats, where `cost` is what left the
/// main wallet. If `max_cost` is set, the amount is lowered until the cost
/// fits; if nothing fits, [`Error::RefillCapReached`] is returned and nothing
/// is sent.
pub async fn transfer_to_pocket(
    main: &Wallet,
    pocket: &Wallet,
    amount: Amount,
    max_cost: Option<Amount>,
) -> Result<(Amount, Amount), Error> {
    ensure_sat(main)?;
    ensure_sat(pocket)?;
    let options = SendOptions {
        include_fee: true,
        ..Default::default()
    };
    let mut amount = amount;
    let mut prepared = None;
    for _ in 0..4 {
        if amount == Amount::ZERO {
            break;
        }
        let candidate = main.prepare_send(amount, options.clone()).await?;
        let cost = candidate.amount() + candidate.fee();
        match max_cost {
            Some(max_cost) if cost > max_cost => {
                let over = cost - max_cost;
                candidate.cancel().await?;
                amount = amount.checked_sub(over).unwrap_or(Amount::ZERO);
            }
            _ => {
                prepared = Some((candidate, cost));
                break;
            }
        }
    }
    let Some((prepared, cost)) = prepared else {
        return Err(Error::RefillCapReached);
    };
    let token = prepared.confirm(None).await?.to_string();
    match pocket.receive(&token, ReceiveOptions::default()).await {
        Ok(received) => Ok((cost, received)),
        Err(error) => {
            // Put the value back rather than leaving an unclaimed token.
            if let Err(reclaim) = main.receive(&token, ReceiveOptions::default()).await {
                tracing::error!("failed to reclaim pocket transfer: {}", reclaim);
            }
            Err(error.into())
        }
    }
}

async fn move_proofs(pocket: &Wallet, main: &Wallet, proofs: Proofs) -> Result<u64, Error> {
    if proofs.is_empty() {
        return Ok(0);
    }
    let received = main
        .receive_proofs(proofs.clone(), ReceiveOptions::default(), None, None)
        .await?;
    // The proofs are now spent at the mint; record that in the pocket.
    pocket.check_proofs_spent(proofs).await?;
    Ok(u64::from(received).saturating_mul(MSAT_PER_SAT))
}

/// Sweep every unspent pocket proof into `main`.
///
/// Pending and reserved proofs are left alone (pending-as-spent) and reported
/// in [`SweepReport::pending_msat`].
pub async fn sweep_pocket(pocket: &Wallet, main: &Wallet) -> Result<SweepReport, Error> {
    ensure_sat(pocket)?;
    ensure_sat(main)?;
    let proofs = pocket.get_unspent_proofs().await?;
    let swept_msat = move_proofs(pocket, main, proofs).await?;
    let pending = pocket
        .total_pending_balance()
        .await?
        .checked_add(pocket.total_reserved_balance().await?)
        .unwrap_or(Amount::ZERO);
    Ok(SweepReport {
        swept_msat,
        pending_msat: u64::from(pending).saturating_mul(MSAT_PER_SAT),
    })
}

/// Move pocket value above `ceiling_msat` into `main`.
///
/// Proofs are moved whole, largest first, without exceeding the excess, so
/// the pocket may end slightly above the ceiling until a later pass. Returns
/// the value received by `main` in millisatoshis.
pub async fn sweep_excess(pocket: &Wallet, main: &Wallet, ceiling_msat: u64) -> Result<u64, Error> {
    ensure_sat(pocket)?;
    ensure_sat(main)?;
    let mut proofs = pocket.get_unspent_proofs().await?;
    let balance =
        u64::from(proofs.total_amount().map_err(cdk::Error::from)?).saturating_mul(MSAT_PER_SAT);
    if balance <= ceiling_msat {
        return Ok(0);
    }
    let excess_sat = (balance - ceiling_msat) / MSAT_PER_SAT;
    proofs.sort_by_key(|proof| std::cmp::Reverse(proof.amount));
    let mut selected = Proofs::new();
    let mut total = 0u64;
    for proof in proofs {
        let value = u64::from(proof.amount);
        if total.saturating_add(value) <= excess_sat {
            total = total.saturating_add(value);
            selected.push(proof);
        }
    }
    move_proofs(pocket, main, selected).await
}
