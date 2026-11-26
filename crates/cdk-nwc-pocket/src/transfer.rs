//! Moving value between the main wallet and a pocket.
//!
//! All transfers are same-mint swaps: no Lightning round-trip, and the only
//! cost is the keyset input fee. Sweeping swaps the pocket's proofs into the
//! main wallet, so any copy of those proofs becomes worthless at the mint.

use std::time::Duration;

use cdk::amount::SplitTarget;
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

/// Whether a mint error may go away if the same request is sent again: a
/// rate limit (429), a server error (5xx) or a transport failure. A rejected
/// request moved no value and cost no fee, so retrying it later is free.
pub fn is_transient(error: &cdk::Error) -> bool {
    match error {
        cdk::Error::HttpError(Some(status), _) => *status == 429 || *status >= 500,
        cdk::Error::HttpError(None, _)
        | cdk::Error::Timeout
        | cdk::Error::CouldNotGetMintInfo
        | cdk::Error::UnknownErrorResponse(_) => true,
        _ => false,
    }
}

/// Backoff between receive attempts on a transient mint error.
const RECEIVE_BACKOFF: [Duration; 3] = [
    Duration::from_millis(750),
    Duration::from_millis(1_500),
    Duration::from_millis(3_000),
];

/// Receive `token` into `wallet`, retrying the same swap on transient mint
/// errors (no new fee: a refused swap costs nothing).
///
/// If a retry finds the token already spent, an earlier attempt reached the
/// mint but its reply was lost. The outputs were derived from the wallet's
/// seed, so a NUT-09/13 restore gets them back; the restored value is
/// returned.
pub async fn receive_with_retry(
    wallet: &Wallet,
    token: &str,
    options: ReceiveOptions,
) -> Result<Amount, cdk::Error> {
    let mut transient_seen = false;
    let mut delays = RECEIVE_BACKOFF.iter();
    loop {
        match wallet.receive(token, options.clone()).await {
            Ok(amount) => return Ok(amount),
            Err(cdk::Error::TokenAlreadySpent) if transient_seen => {
                tracing::warn!("receive reply was lost; restoring from seed");
                let before = wallet.total_balance().await?;
                wallet.restore().await?;
                let after = wallet.total_balance().await?;
                return Ok(after.checked_sub(before).unwrap_or(Amount::ZERO));
            }
            Err(error) if is_transient(&error) => {
                transient_seen = true;
                match delays.next() {
                    Some(delay) => tokio::time::sleep(*delay).await,
                    None => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// Move up to `amount` sats from `main` into `pocket`.
///
/// The main wallet pays the swap fees, so the pocket receives the amount
/// sent. Returns `(cost, received)` in sats, where `cost` is what left the
/// main wallet. If `max_cost` is set, the amount is lowered until the cost
/// fits the bound and the main balance; if nothing fits,
/// [`Error::RefillCapReached`] (or `InsufficientFunds` when main is the
/// limit) is returned and nothing is sent.
pub async fn transfer_to_pocket(
    main: &Wallet,
    pocket: &Wallet,
    amount: Amount,
    max_cost: Option<Amount>,
) -> Result<(Amount, Amount), Error> {
    transfer_to_pocket_split(main, pocket, amount, max_cost, |_| SplitTarget::None).await
}

/// [`transfer_to_pocket`], choosing how the pocket splits what it receives.
///
/// `split` gets the amount the pocket will receive and returns the target
/// denominations (see [`SplitTarget::Values`]); the pocket's receive swap
/// happens anyway, so shaping its outputs is free. A target that does not
/// fit falls back to the default split.
pub async fn transfer_to_pocket_split(
    main: &Wallet,
    pocket: &Wallet,
    amount: Amount,
    max_cost: Option<Amount>,
    split: impl Fn(Amount) -> SplitTarget,
) -> Result<(Amount, Amount), Error> {
    ensure_sat(main)?;
    ensure_sat(pocket)?;
    let options = SendOptions {
        include_fee: true,
        ..Default::default()
    };
    let mut amount = amount;
    let mut prepared = None;
    // When main holds about as much as the refill wants, `amount + fee` may
    // not fit; with a cost bound we step the amount down instead of failing.
    let mut short_step = Amount::ONE;
    let mut main_short = false;
    for _ in 0..8 {
        if amount == Amount::ZERO {
            break;
        }
        let candidate = match main.prepare_send(amount, options.clone()).await {
            Ok(candidate) => candidate,
            Err(cdk::Error::InsufficientFunds) if max_cost.is_some() => {
                amount = amount.checked_sub(short_step).unwrap_or(Amount::ZERO);
                short_step = short_step + short_step;
                main_short = true;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
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
        if main_short {
            return Err(Error::Wallet(cdk::Error::InsufficientFunds));
        }
        return Err(Error::RefillCapReached);
    };
    let sent = prepared.amount();
    let token = prepared.confirm(None).await?.to_string();
    let target = split(sent);
    let shaped = ReceiveOptions {
        amount_split_target: target.clone(),
        ..Default::default()
    };
    let first = match receive_with_retry(pocket, &token, shaped).await {
        // A target that does not fit fails locally, before the mint is asked.
        Err(error) if target != SplitTarget::None && !is_transient(&error) => {
            tracing::debug!("split target refused ({error}); using the default split");
            receive_with_retry(pocket, &token, ReceiveOptions::default()).await
        }
        other => other,
    };
    match first {
        Ok(received) => Ok((cost, received)),
        Err(error) => {
            // Put the value back rather than leaving an unclaimed token. The
            // receive above already retried transient errors, so this runs
            // once, not in a loop.
            if let Err(reclaim) = receive_with_retry(main, &token, ReceiveOptions::default()).await
            {
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
