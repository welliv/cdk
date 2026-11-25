//! NWC request handler that can only reach one pocket.

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use cdk::wallet::WalletNwcHandler;
use cdk::Wallet;
use cdk_nostr::nwc::nip47::{
    ErrorCode, GetBalanceResponse, GetInfoResponse, ListTransactionsRequest, LookupInvoiceRequest,
    LookupInvoiceResponse, MakeInvoiceRequest, MakeInvoiceResponse, NIP47Error, PayInvoiceRequest,
    PayInvoiceResponse,
};
use cdk_nostr::nwc::NwcRequestHandler;
use lightning_invoice::Bolt11Invoice;
use tokio::sync::Mutex;

use crate::error::Error;
use crate::event::{OutOfFundsReason, PocketEvent};
use crate::refill::Refiller;
use crate::transfer::{balance_msat, ensure_sat, sweep_excess, sweep_pocket, SweepReport};

/// Margin requested above the invoice amount when refilling for a payment,
/// covering the melt fee reserve and input fees.
const PAYMENT_FEE_MARGIN_MSAT: u64 = 10_000;

/// A [`NwcRequestHandler`] backed by a single pocket wallet.
///
/// The handler never holds the main wallet. It can ask the [`Refiller`] for a
/// refill, which enforces the cap on the main-wallet side. Payments on one
/// pocket are serialized so the balance drop of each payment is exact; that
/// drop is reported as `fees_paid` (minus the invoice amount) and added to
/// [`Self::spent_msat`].
#[derive(Debug)]
pub struct PocketNwcHandler {
    connection_id: String,
    wallet: Arc<Wallet>,
    inner: WalletNwcHandler,
    refiller: Option<Arc<Refiller>>,
    lock: Mutex<()>,
    spent_msat: AtomicU64,
}

impl PocketNwcHandler {
    /// Create a handler for the pocket of `connection_id`.
    ///
    /// Without a `refiller` the pocket never refills; when it runs dry the
    /// handler answers `INSUFFICIENT_BALANCE`.
    pub fn new(
        connection_id: String,
        wallet: Arc<Wallet>,
        refiller: Option<Arc<Refiller>>,
    ) -> Result<Self, Error> {
        ensure_sat(&wallet)?;
        Ok(Self {
            connection_id,
            inner: WalletNwcHandler::new(Arc::clone(&wallet), None),
            wallet,
            refiller,
            lock: Mutex::new(()),
            spent_msat: AtomicU64::new(0),
        })
    }

    /// The connection this pocket belongs to.
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    /// The pocket wallet.
    pub fn wallet(&self) -> &Arc<Wallet> {
        &self.wallet
    }

    /// Total drop in pocket balance caused by payments since creation,
    /// including all fees and anything left pending.
    pub fn spent_msat(&self) -> u64 {
        self.spent_msat.load(Ordering::SeqCst)
    }

    /// Sweep the pocket into `main` (revoke or expiry), waiting for any
    /// in-flight payment on this pocket to finish first.
    pub async fn sweep_to(&self, main: &Wallet) -> Result<SweepReport, Error> {
        let _guard = self.lock.lock().await;
        sweep_pocket(&self.wallet, main).await
    }

    /// Claim paid incoming invoices into the pocket, then move anything above
    /// `ceiling_msat` to `main`. Returns `(claimed_msat, moved_to_main_msat)`.
    pub async fn claim_and_rebalance(
        &self,
        main: &Wallet,
        ceiling_msat: u64,
    ) -> Result<(u64, u64), Error> {
        let _guard = self.lock.lock().await;
        let claimed = u64::from(self.wallet.mint_unissued_quotes().await?)
            .saturating_mul(crate::MSAT_PER_SAT);
        let moved = sweep_excess(&self.wallet, main, ceiling_msat).await?;
        Ok((claimed, moved))
    }

    async fn pay_once(
        &self,
        request: PayInvoiceRequest,
        invoice_msat: u64,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        let before = balance_msat(&self.wallet).await.map_err(internal)?;
        let result = self.inner.pay_invoice(request).await;
        let after = balance_msat(&self.wallet).await.map_err(internal)?;
        let spent = before.saturating_sub(after);
        self.spent_msat.fetch_add(spent, Ordering::SeqCst);
        result.map(|mut response| {
            response.fees_paid = Some(spent.saturating_sub(invoice_msat));
            response
        })
    }

    async fn out_of_funds(&self, invoice_msat: u64, reason: OutOfFundsReason) -> NIP47Error {
        let now = now_secs();
        let balance = balance_msat(&self.wallet).await.unwrap_or(0);
        let (remaining, resets_at) = match &self.refiller {
            Some(refiller) => (
                refiller
                    .registry()
                    .remaining_msat(&self.connection_id, now)
                    .unwrap_or(0),
                refiller
                    .registry()
                    .resets_at(&self.connection_id, now)
                    .unwrap_or(None),
            ),
            None => (0, None),
        };
        let event = PocketEvent::OutOfFunds {
            connection_id: self.connection_id.clone(),
            pocket_balance_msat: balance,
            invoice_msat,
            refill_remaining_msat: remaining,
            refill_resets_at: resets_at,
            reason,
        };
        match &self.refiller {
            Some(refiller) => refiller.emit(event),
            None => tracing::info!("pocket event: {:?}", event),
        }
        match reason {
            OutOfFundsReason::RefillCapReached => NIP47Error {
                code: ErrorCode::QuotaExceeded,
                message: "pocket refill cap reached".to_owned(),
            },
            OutOfFundsReason::NoAutoRefill | OutOfFundsReason::CeilingReached => NIP47Error {
                code: ErrorCode::InsufficientBalance,
                message: "insufficient pocket balance".to_owned(),
            },
        }
    }

    /// Auto-refill configuration, if any.
    fn auto_refill(&self) -> Option<(Arc<Refiller>, crate::AutoRefill)> {
        let refiller = self.refiller.as_ref()?;
        let config = refiller.registry().get(&self.connection_id).ok()?;
        config
            .refill
            .auto_refill
            .map(|auto| (Arc::clone(refiller), auto))
    }

    async fn top_up_after_payment(&self) {
        let Some((refiller, auto)) = self.auto_refill() else {
            return;
        };
        let Ok(balance) = balance_msat(&self.wallet).await else {
            return;
        };
        if balance > auto.threshold_msat {
            return;
        }
        let wanted = auto.target_msat.saturating_sub(balance);
        if let Err(error) = refiller
            .refill(&self.connection_id, &self.wallet, wanted, now_secs())
            .await
        {
            tracing::debug!("pocket auto-refill skipped: {}", error);
        }
    }
}

fn internal(error: Error) -> NIP47Error {
    NIP47Error {
        code: ErrorCode::Internal,
        message: error.to_string(),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[async_trait]
impl NwcRequestHandler for PocketNwcHandler {
    async fn get_info(&self) -> Result<GetInfoResponse, NIP47Error> {
        self.inner.get_info().await
    }

    /// Returns the pocket balance, never the main wallet's.
    async fn get_balance(&self) -> Result<GetBalanceResponse, NIP47Error> {
        self.inner.get_balance().await
    }

    async fn make_invoice(
        &self,
        request: MakeInvoiceRequest,
    ) -> Result<MakeInvoiceResponse, NIP47Error> {
        self.inner.make_invoice(request).await
    }

    async fn pay_invoice(
        &self,
        request: PayInvoiceRequest,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        let invoice_msat = Bolt11Invoice::from_str(&request.invoice)
            .ok()
            .and_then(|invoice| invoice.amount_milli_satoshis())
            .or(request.amount)
            .unwrap_or(0);

        let guard = self.lock.lock().await;
        let first = self.pay_once(request.clone(), invoice_msat).await;
        let result = match first {
            Err(error) if error.code == ErrorCode::InsufficientBalance => {
                let Some((refiller, auto)) = self.auto_refill() else {
                    return Err(self
                        .out_of_funds(invoice_msat, OutOfFundsReason::NoAutoRefill)
                        .await);
                };
                let balance = balance_msat(&self.wallet).await.map_err(internal)?;
                let wanted = invoice_msat
                    .saturating_add(PAYMENT_FEE_MARGIN_MSAT)
                    .max(auto.target_msat)
                    .saturating_sub(balance);
                match refiller
                    .refill(&self.connection_id, &self.wallet, wanted, now_secs())
                    .await
                {
                    Ok(added) if added > 0 => match self.pay_once(request, invoice_msat).await {
                        Err(error) if error.code == ErrorCode::InsufficientBalance => {
                            let remaining = refiller
                                .registry()
                                .remaining_msat(&self.connection_id, now_secs())
                                .unwrap_or(0);
                            let reason = if remaining == 0 {
                                OutOfFundsReason::RefillCapReached
                            } else {
                                OutOfFundsReason::CeilingReached
                            };
                            return Err(self.out_of_funds(invoice_msat, reason).await);
                        }
                        other => other,
                    },
                    Ok(_) => {
                        return Err(self
                            .out_of_funds(invoice_msat, OutOfFundsReason::CeilingReached)
                            .await)
                    }
                    Err(Error::RefillCapReached) => {
                        return Err(self
                            .out_of_funds(invoice_msat, OutOfFundsReason::RefillCapReached)
                            .await)
                    }
                    Err(other) => {
                        tracing::warn!("pocket refill failed: {}", other);
                        return Err(error);
                    }
                }
            }
            other => other,
        };
        drop(guard);
        if result.is_ok() {
            self.top_up_after_payment().await;
        }
        result
    }

    async fn lookup_invoice(
        &self,
        request: LookupInvoiceRequest,
    ) -> Result<LookupInvoiceResponse, NIP47Error> {
        self.inner.lookup_invoice(request).await
    }

    async fn list_transactions(
        &self,
        request: ListTransactionsRequest,
    ) -> Result<Vec<LookupInvoiceResponse>, NIP47Error> {
        self.inner.list_transactions(request).await
    }
}
