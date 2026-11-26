//! Main-wallet-side refills, capped on a rolling window.

use std::sync::Arc;

use cdk::amount::SplitTarget;
use cdk::{Amount, Wallet};
use tokio::sync::{broadcast, Mutex};

use crate::error::Error;
use crate::event::PocketEvent;
use crate::registry::{PocketRegistry, MAIN_SPEND_LOCK};
use crate::transfer::{balance_msat, transfer_to_pocket, transfer_to_pocket_split};
use crate::MSAT_PER_SAT;

/// Capacity of the event channel.
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Fee margin requested on top of a refill, in millisatoshis.
const REFILL_FEE_MARGIN_MSAT: u64 = 3_000;

/// Moves funds from the main wallet into pockets, never more than each
/// pocket's refill cap per rolling window.
///
/// The refiller holds the main wallet and the registry. A pocket handler can
/// only *ask* for a refill; the cap is checked and recorded here, on the main
/// wallet side.
#[derive(Debug)]
pub struct Refiller {
    main: Arc<Wallet>,
    registry: PocketRegistry,
    events: broadcast::Sender<PocketEvent>,
    lock: Mutex<()>,
}

impl Refiller {
    /// Create a refiller for `main` using `registry` as the refill ledger.
    pub fn new(main: Arc<Wallet>, registry: PocketRegistry) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            main,
            registry,
            events,
            lock: Mutex::new(()),
        }
    }

    /// Subscribe to [`PocketEvent`]s.
    pub fn subscribe(&self) -> broadcast::Receiver<PocketEvent> {
        self.events.subscribe()
    }

    /// The pocket registry.
    pub fn registry(&self) -> &PocketRegistry {
        &self.registry
    }

    /// The main wallet.
    pub fn main_wallet(&self) -> &Arc<Wallet> {
        &self.main
    }

    /// Emit an event to subscribers. Having no subscribers is not an error.
    pub fn emit(&self, event: PocketEvent) {
        tracing::info!("pocket event: {:?}", event);
        let _ = self.events.send(event);
    }

    /// Refill `pocket` by up to `wanted_msat`, bounded by the pocket ceiling
    /// and the remaining refill allowance. Returns the value added to the
    /// pocket in millisatoshis (zero when the ceiling is reached).
    ///
    /// Returns [`Error::RefillCapReached`] when the window allowance is used
    /// up.
    pub async fn refill(
        &self,
        connection_id: &str,
        pocket: &Wallet,
        wanted_msat: u64,
        now: u64,
    ) -> Result<u64, Error> {
        self.refill_split(connection_id, pocket, wanted_msat, now, |_| {
            SplitTarget::None
        })
        .await
    }

    /// [`Self::refill`], choosing the denominations the pocket receives
    /// (see [`transfer_to_pocket_split`]).
    pub async fn refill_split(
        &self,
        connection_id: &str,
        pocket: &Wallet,
        wanted_msat: u64,
        now: u64,
        split: impl Fn(Amount) -> SplitTarget,
    ) -> Result<u64, Error> {
        let _guard = self.lock.lock().await;
        // Another process (`serve` or `key proxy`) may spend main too.
        let _main = self.registry.lock_spend(MAIN_SPEND_LOCK).await?;
        let config = self.registry.get(connection_id)?;
        if config.swept_at.is_some() {
            return Err(Error::PocketSwept {
                connection_id: connection_id.to_owned(),
            });
        }
        let balance = balance_msat(pocket).await?;
        let want = wanted_msat.min(config.ceiling_msat.saturating_sub(balance));
        if want == 0 {
            return Ok(0);
        }
        let (grant_id, granted) = self.registry.try_grant(
            connection_id,
            want.saturating_add(REFILL_FEE_MARGIN_MSAT),
            now,
        )?;
        if granted == 0 {
            return Err(Error::RefillCapReached);
        }
        // Never ask the main wallet for more than it holds: a refill that
        // fits the grant but not the main balance would otherwise fail
        // outright and leave the rest of the cap unused. Send what is there.
        let main_sat = match self.main.total_balance().await {
            Ok(balance) => u64::from(balance),
            Err(error) => {
                self.registry.settle_grant(grant_id, 0)?;
                return Err(error.into());
            }
        };
        let grant_sat = granted / MSAT_PER_SAT;
        let max_cost_sat = grant_sat.min(main_sat);
        let amount_sat = want.div_ceil(MSAT_PER_SAT).min(max_cost_sat);
        if amount_sat == 0 {
            self.registry.settle_grant(grant_id, 0)?;
            return Err(if grant_sat == 0 {
                Error::RefillCapReached
            } else {
                Error::Wallet(cdk::Error::InsufficientFunds)
            });
        }
        match transfer_to_pocket_split(
            &self.main,
            pocket,
            Amount::from(amount_sat),
            Some(Amount::from(max_cost_sat)),
            split,
        )
        .await
        {
            Ok((cost, received)) => {
                self.registry
                    .settle_grant(grant_id, u64::from(cost).saturating_mul(MSAT_PER_SAT))?;
                Ok(u64::from(received).saturating_mul(MSAT_PER_SAT))
            }
            Err(error) => {
                self.registry.settle_grant(grant_id, 0)?;
                Err(error)
            }
        }
    }

    /// Manual top-up chosen by the user.
    ///
    /// It does not count against the refill cap (the user is deciding to put
    /// more at risk), but it does respect the pocket ceiling.
    pub async fn top_up(
        &self,
        connection_id: &str,
        pocket: &Wallet,
        amount_msat: u64,
    ) -> Result<u64, Error> {
        let _guard = self.lock.lock().await;
        let _main = self.registry.lock_spend(MAIN_SPEND_LOCK).await?;
        let config = self.registry.get(connection_id)?;
        let balance = balance_msat(pocket).await?;
        if balance.saturating_add(amount_msat) > config.ceiling_msat {
            return Err(Error::CeilingExceeded {
                balance_msat: balance,
                amount_msat,
                ceiling_msat: config.ceiling_msat,
            });
        }
        let (_, received) = transfer_to_pocket(
            &self.main,
            pocket,
            Amount::from(amount_msat / MSAT_PER_SAT),
            None,
        )
        .await?;
        Ok(u64::from(received).saturating_mul(MSAT_PER_SAT))
    }
}
