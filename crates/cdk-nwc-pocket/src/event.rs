//! The single user-facing pocket event.

use serde::{Deserialize, Serialize};

/// Why a pocket could not cover a payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OutOfFundsReason {
    /// The refill cap for the current window is used up.
    RefillCapReached,
    /// The connection has no automatic refill configured.
    NoAutoRefill,
    /// The pocket is at its ceiling and still cannot cover the payment.
    CeilingReached,
}

/// Event emitted to wallet UIs.
///
/// There is deliberately only one: the moment a payment is refused because
/// the pocket cannot cover it and no refill is available. That is the only
/// point at which the user's decision (top up, raise the cap, or revoke)
/// changes anything.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PocketEvent {
    /// A payment was refused because the pocket is out of funds.
    OutOfFunds {
        /// Connection ID.
        connection_id: String,
        /// Pocket balance when the payment was refused.
        pocket_balance_msat: u64,
        /// Amount of the refused invoice.
        invoice_msat: u64,
        /// Refill allowance left in the current window.
        refill_remaining_msat: u64,
        /// When the oldest refill in the window drops out, if any.
        refill_resets_at: Option<u64>,
        /// Why no refill was made.
        reason: OutOfFundsReason,
    },
}
