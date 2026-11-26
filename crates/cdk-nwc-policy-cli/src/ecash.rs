//! Paying exact amounts from a pocket without a swap.
//!
//! A swap costs the keyset input fee (on cashu.mutinynet.com, 100 ppk per
//! input, rounded up per swap: at least 1 sat). Paying a request needs at
//! most two mint operations that cannot be avoided: the provider redeems our
//! token (its input fee is included in the token) and we swap the change it
//! returns. The send itself needs no swap if the pocket already holds proofs
//! that add up to exactly `amount + fee`.
//!
//! So pockets keep **bundles**: groups of proofs whose denominations are the
//! exact split for one request at a given price. Bundles are cut for free on
//! swaps that happen anyway (refills into the pocket), and otherwise by one
//! re-bundle swap that prepares several requests at once. A request then
//! sends one bundle offline: no mint call, no fee, and it still works while
//! the mint is rate-limiting.

use cdk::amount::SplitTarget;
use cdk::nuts::nut00::ProofsMethods;
use cdk::nuts::{Id, Proofs, State, Token};
use cdk::wallet::Wallet;
use cdk::Amount;

/// Most bundles cut by one swap; more inputs later cost more to sweep.
pub const MAX_BUNDLES: u64 = 6;

/// The exact proofs one request at `price` needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    /// Keyset the bundle is cut in (the active one).
    pub keyset_id: Id,
    /// Denominations, as the active keyset splits them.
    pub denominations: Vec<Amount>,
    /// Their total: what leaves the pocket for one request.
    pub total_sat: u64,
    /// The token's face value after the receiver's input fee (`>= price`).
    pub send_sat: u64,
}

impl Bundle {
    /// A split target with up to `count` bundles, the rest split normally.
    pub fn target(&self, count: u64) -> SplitTarget {
        if count == 0 {
            return SplitTarget::None;
        }
        SplitTarget::Values(
            (0..count)
                .flat_map(|_| self.denominations.iter().copied())
                .collect(),
        )
    }

    /// How many bundles fit in `amount_sat`, capped at [`MAX_BUNDLES`].
    pub fn fit(&self, amount_sat: u64) -> u64 {
        amount_sat
            .checked_div(self.total_sat)
            .unwrap_or(0)
            .min(MAX_BUNDLES)
    }
}

/// A split target for `amount_sat` arriving in a pocket that already holds
/// `loose` proofs: the outputs complete as many bundles as fit, reusing loose
/// proofs, and the rest is split normally.
///
/// Change from the provider has to be swapped anyway; shaping that swap so
/// that "change + what is lying around" forms the next bundle means the next
/// request sends offline. Example: price 45, bundle `[32, 8, 4, 2]`, a loose
/// `2` in the pocket, change 44: the outputs are `[32, 8, 4]`.
pub fn complement_target(bundle: &Bundle, loose: &[u64], amount_sat: u64) -> SplitTarget {
    let mut available: Vec<u64> = loose.to_vec();
    let mut budget = amount_sat;
    let mut values = Vec::new();
    for _ in 0..MAX_BUNDLES {
        let mut taken = Vec::new();
        let mut need = Vec::new();
        for denomination in &bundle.denominations {
            let d = u64::from(*denomination);
            match available.iter().position(|a| *a == d) {
                Some(i) => taken.push(available.swap_remove(i)),
                None => need.push(d),
            }
        }
        let need_sat: u64 = need.iter().sum();
        if need.is_empty() || need_sat > budget {
            // A complete bundle already lying around needs no outputs; stop
            // when the next one does not fit either.
            if need.is_empty() {
                continue;
            }
            break;
        }
        budget -= need_sat;
        values.extend(need.into_iter().map(Amount::from));
    }
    if values.is_empty() {
        SplitTarget::None
    } else {
        SplitTarget::Values(values)
    }
}

/// Sats a wallet will hold after receiving `token` (face value minus the
/// active keyset's input fee for its proofs).
pub async fn net_receive_sat(wallet: &Wallet, token: &str) -> Option<u64> {
    use std::str::FromStr;
    let parsed = Token::from_str(token).ok()?;
    let face = u64::from(parsed.value().ok()?);
    let inputs = parsed.token_secrets().len() as u64;
    let keyset = wallet.active_keyset().await.ok()?;
    let ppk = wallet.get_keyset_fees_by_id(keyset.id).await.ok()?;
    face.checked_sub((inputs * ppk).div_ceil(1000))
}

/// Amounts of the unspent proofs in `wallet`.
pub async fn loose_amounts(wallet: &Wallet) -> Result<Vec<u64>, cdk::Error> {
    Ok(wallet
        .get_unspent_proofs()
        .await?
        .iter()
        .map(|p| u64::from(p.amount))
        .collect())
}

/// The bundle for one payment of `price_sat` in `wallet`'s active keyset.
pub async fn bundle_for(wallet: &Wallet, price_sat: u64) -> Result<Bundle, cdk::Error> {
    let keyset = wallet.active_keyset().await?;
    let fees = wallet.get_keyset_fees_and_amounts_by_id(keyset.id).await?;
    let denominations = Amount::from(price_sat.max(1)).split_with_fee(&fees)?;
    let total_sat: u64 = denominations.iter().map(|a| u64::from(*a)).sum();
    let fee_sat = (denominations.len() as u64 * fees.fee()).div_ceil(1000);
    Ok(Bundle {
        keyset_id: keyset.id,
        total_sat,
        send_sat: total_sat.saturating_sub(fee_sat).max(price_sat),
        denominations,
    })
}

/// A token taken from a pocket.
#[derive(Debug)]
pub struct Taken {
    /// Encoded token.
    pub token: String,
    /// Face value of the token (the receiver's fee is included in it).
    pub token_sat: u64,
    /// Whether a swap was needed (`false`: the bundle was sent offline).
    pub swapped: bool,
    /// The proofs in the token. They stay `PendingSpent` in the pocket
    /// until [`forget_sent`] (the provider redeemed them) or a check.
    pub proofs: Proofs,
}

/// Send exactly one bundle for `price_sat` from `wallet`.
///
/// 1. Offline: if the pocket holds proofs adding up to the bundle, they are
///    sent as they are. No mint call.
/// 2. Offline, overpaying: the smallest set of loose proofs worth at least
///    the price, if its value after fees stays within `max_net_sat` (the
///    key's per-request cap). The provider returns the rest as change, which
///    is swapped anyway, so this costs nothing a swap would not.
/// 3. Otherwise one re-bundle swap turns all loose proofs into up to
///    [`MAX_BUNDLES`] bundles plus change, and step 1 runs again.
///
/// Returns `InsufficientFunds` without touching the mint when the pocket
/// cannot cover one bundle.
pub async fn take_exact(
    wallet: &Wallet,
    price_sat: u64,
    max_net_sat: u64,
) -> Result<Taken, cdk::Error> {
    let bundle = bundle_for(wallet, price_sat).await?;
    if let Some(taken) = take_ready(wallet, &bundle, price_sat, max_net_sat).await? {
        return Ok(taken);
    }
    let proofs = wallet.get_unspent_proofs().await?;
    let total = u64::from(proofs.total_amount()?);
    let fee = u64::from(wallet.get_proofs_fee(&proofs).await?.total);
    let out = total.saturating_sub(fee);
    if out < bundle.total_sat {
        return Err(cdk::Error::InsufficientFunds);
    }
    wallet
        .swap(
            None,
            bundle.target(bundle.fit(out)),
            proofs,
            None,
            false,
            false,
        )
        .await?;
    // The swap's outputs include at least one bundle, so this matches.
    let mut taken = send_offline(wallet, &bundle)
        .await?
        .ok_or(cdk::Error::InsufficientFunds)?;
    taken.swapped = true;
    Ok(taken)
}

/// Send one bundle for `price_sat` only if it is ready: no mint call at
/// all. `None` when the pocket has no exact bundle (or too little).
pub async fn take_offline(
    wallet: &Wallet,
    price_sat: u64,
    max_net_sat: u64,
) -> Result<Option<Taken>, cdk::Error> {
    let bundle = bundle_for(wallet, price_sat).await?;
    take_ready(wallet, &bundle, price_sat, max_net_sat).await
}

/// Steps 1 and 2 of [`take_exact`]: no mint call.
async fn take_ready(
    wallet: &Wallet,
    bundle: &Bundle,
    price_sat: u64,
    max_net_sat: u64,
) -> Result<Option<Taken>, cdk::Error> {
    if let Some(taken) = send_offline(wallet, bundle).await? {
        return Ok(Some(taken));
    }
    let ppk = wallet.get_keyset_fees_by_id(bundle.keyset_id).await?;
    let unspent: Proofs = wallet
        .get_unspent_proofs()
        .await?
        .into_iter()
        .filter(|p| p.keyset_id == bundle.keyset_id)
        .collect();
    let amounts: Vec<u64> = unspent.iter().map(|p| u64::from(p.amount)).collect();
    let Some(chosen) = overpay_set(&amounts, ppk, price_sat, max_net_sat.max(bundle.send_sat))
    else {
        return Ok(None);
    };
    let picked: Proofs = chosen.into_iter().map(|i| unspent[i].clone()).collect();
    send_picked(wallet, picked).await.map(Some)
}

/// The cheapest subset of `amounts` (indices) whose value after the input
/// fee (`ppk` per input, rounded up) is at least `price_sat` and at most
/// `max_net_sat`: the smallest total, then the fewest inputs.
pub fn overpay_set(
    amounts: &[u64],
    ppk: u64,
    price_sat: u64,
    max_net_sat: u64,
) -> Option<Vec<usize>> {
    let fee = |n: usize| (n as u64 * ppk).div_ceil(1000);
    let limit = usize::try_from(max_net_sat.saturating_add(fee(amounts.len()))).ok()?;
    // best[s] = fewest inputs reaching total s; take[i][s]: item i used.
    let mut best: Vec<Option<usize>> = vec![None; limit + 1];
    best[0] = Some(0);
    let mut take = vec![vec![false; limit + 1]; amounts.len()];
    for (i, &a) in amounts.iter().enumerate() {
        let a = usize::try_from(a).ok()?;
        if a > limit {
            continue;
        }
        for s in (a..=limit).rev() {
            if let Some(n) = best[s - a] {
                if best[s].is_none_or(|m| n + 1 < m) {
                    best[s] = Some(n + 1);
                    take[i][s] = true;
                }
            }
        }
    }
    let total = (1..=limit).find(|&s| {
        best[s].is_some_and(|n| {
            let net = (s as u64).saturating_sub(fee(n));
            net >= price_sat && net <= max_net_sat
        })
    })?;
    // Walk back: the last item that improved a total is the one used for it.
    let mut chosen = Vec::new();
    let mut s = total;
    for i in (0..amounts.len()).rev() {
        if s > 0 && take[i][s] {
            chosen.push(i);
            s -= amounts[i] as usize;
        }
    }
    (s == 0 && !chosen.is_empty()).then_some(chosen)
}

/// Pick one unspent proof per bundle denomination (active keyset only) and
/// send them as they are: marked `PendingSpent`, no swap, no mint call.
///
/// The selection is ours rather than `prepare_send`'s, whose fee-inclusive
/// selection asks for the input fee twice and so never matches a bundle.
/// The caller holds the pocket's spend lock, so no other task or process
/// selects the same proofs.
async fn send_offline(wallet: &Wallet, bundle: &Bundle) -> Result<Option<Taken>, cdk::Error> {
    let mut unspent = wallet.get_unspent_proofs().await?;
    let mut picked = Proofs::new();
    for denomination in &bundle.denominations {
        let Some(position) = unspent
            .iter()
            .position(|p| p.amount == *denomination && p.keyset_id == bundle.keyset_id)
        else {
            return Ok(None);
        };
        picked.push(unspent.swap_remove(position));
    }
    send_picked(wallet, picked).await.map(Some)
}

/// Mark `picked` `PendingSpent` and encode them as a token.
async fn send_picked(wallet: &Wallet, picked: Proofs) -> Result<Taken, cdk::Error> {
    wallet
        .localstore
        .update_proofs_state(picked.ys()?, State::PendingSpent)
        .await?;
    let token = Token::new(
        wallet.mint_url.clone(),
        picked.clone(),
        None,
        wallet.unit.clone(),
    );
    Ok(Taken {
        token_sat: u64::from(picked.total_amount()?),
        token: token.to_string(),
        swapped: false,
        proofs: picked,
    })
}

/// The provider redeemed a token we sent: drop its proofs from the pocket.
pub async fn forget_sent(wallet: &Wallet, proofs: &Proofs) -> Result<(), cdk::Error> {
    wallet
        .localstore
        .update_proofs(vec![], proofs.ys()?)
        .await?;
    Ok(())
}

/// Swap every sent-but-unredeemed proof of `pocket` into `main`, so a token
/// handed out earlier (e.g. by `/v1/pocket/send`) is worthless after revoke.
/// Returns the sats `main` received.
pub async fn reclaim_outstanding(pocket: &Wallet, main: &Wallet) -> Result<u64, cdk::Error> {
    let outstanding = pocket.get_pending_spent_proofs().await?;
    if outstanding.is_empty() {
        return Ok(0);
    }
    // Drops the ones already redeemed; the rest are still live.
    let states = pocket.check_proofs_spent(outstanding.clone()).await?;
    let live: Proofs = outstanding
        .into_iter()
        .zip(states)
        .filter(|(_, state)| state.state == State::Unspent)
        .map(|(proof, _)| proof)
        .collect();
    if live.is_empty() {
        return Ok(0);
    }
    let received = main
        .receive_proofs(live.clone(), Default::default(), None, None)
        .await?;
    forget_sent(pocket, &live).await?;
    Ok(u64::from(received))
}

/// Face value of an encoded token, if it parses.
pub fn token_value_sat(token: &str) -> Option<u64> {
    use std::str::FromStr;
    cdk::nuts::Token::from_str(token)
        .ok()?
        .value()
        .ok()
        .map(u64::from)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn bundle() -> Bundle {
        Bundle {
            keyset_id: Id::from_str("00916bbf7ef91a36").expect("id"),
            denominations: [32, 8, 4, 2].map(Amount::from).to_vec(),
            total_sat: 46,
            send_sat: 45,
        }
    }

    #[test]
    fn bundles_fit_and_target() {
        let b = bundle();
        assert_eq!(b.fit(45), 0);
        assert_eq!(b.fit(92), 2);
        assert_eq!(b.fit(10_000), MAX_BUNDLES);
        assert_eq!(b.target(0), SplitTarget::None);
        let SplitTarget::Values(values) = b.target(2) else {
            panic!("values");
        };
        assert_eq!(values.len(), 8);
        assert_eq!(values.iter().map(|a| u64::from(*a)).sum::<u64>(), 92);
    }

    fn values(target: SplitTarget) -> Vec<u64> {
        match target {
            SplitTarget::Values(v) => v.into_iter().map(u64::from).collect(),
            _ => vec![],
        }
    }

    #[test]
    fn overpay_picks_the_cheapest_set_within_the_cap() {
        let sum = |a: &[u64], i: &[usize]| i.iter().map(|&i| a[i]).sum::<u64>();
        // No exact [32, 8, 4, 2]: 64 alone (net 63) beats 32+16+... sets.
        let a = [64, 1, 128];
        let i = overpay_set(&a, 100, 45, 100).expect("set");
        assert_eq!(sum(&a, &i), 64);
        // Cap 60: 64 nets 63 > 60, so nothing (the caller swaps instead).
        assert_eq!(overpay_set(&a, 100, 45, 60), None);
        // 32 + 16 = 48 nets 47: smallest total that covers 45 + fee.
        let a = [32, 16, 16, 2, 1];
        let i = overpay_set(&a, 100, 45, 60).expect("set");
        assert_eq!(sum(&a, &i), 48);
        // Exactly price + fee is reachable: 32 + 8 + 4 + 2 = 46.
        let a = [2, 4, 8, 32, 64];
        let i = overpay_set(&a, 100, 45, 60).expect("set");
        assert_eq!(sum(&a, &i), 46);
        assert_eq!(i.len(), 4);
        // Too little.
        assert_eq!(overpay_set(&[16, 8], 100, 45, 60), None);
        // Fees count: 11 inputs at 100 ppk cost 2 sat.
        let a = [4; 12];
        let i = overpay_set(&a, 100, 40, 60).expect("set");
        assert_eq!(sum(&a, &i), 44);
    }

    #[test]
    fn change_completes_the_next_bundle() {
        let b = bundle();
        // A loose 2 + change 44 → [32, 8, 4]: the next send is offline.
        assert_eq!(values(complement_target(&b, &[2], 44)), vec![32, 8, 4]);
        // Nothing loose: 44 cannot make a 46 bundle.
        assert_eq!(values(complement_target(&b, &[], 44)), Vec::<u64>::new());
        // Enough for two bundles, reusing what is there.
        assert_eq!(
            values(complement_target(&b, &[32, 2], 60)),
            vec![8, 4, 32, 8, 4, 2]
        );
        // A complete bundle already lying around needs no new outputs.
        assert_eq!(
            values(complement_target(&b, &[32, 8, 4, 2], 10)),
            Vec::<u64>::new()
        );
    }
}
