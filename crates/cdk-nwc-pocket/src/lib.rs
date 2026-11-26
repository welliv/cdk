//! Per-connection Cashu pockets for Nostr Wallet Connect.
//!
//! An NWC connection URI is a bearer credential: whoever holds it can sign
//! requests the wallet service will accept. This crate bounds what such a
//! holder can lose with money rather than bookkeeping:
//!
//! - every spend-capable connection gets its own **pocket**, a separate CDK
//!   [`Wallet`](cdk::Wallet) with its own proofs, database and a seed derived
//!   from the main seed ([`derive_pocket_seed`]);
//! - the NWC handler for that connection ([`PocketNwcHandler`]) only holds the
//!   pocket wallet, so the most a link can spend is what is in the pocket;
//! - revoking or expiring a connection sweeps the pocket back to the main
//!   wallet with a swap ([`sweep_pocket`]), which invalidates any copied proofs;
//! - the budget is a **refill cap** on a rolling window, enforced on the main
//!   wallet side by [`Refiller`] using the [`PocketRegistry`] ledger;
//! - when a payment is refused because the pocket is empty and no refill is
//!   available, a single [`PocketEvent::OutOfFunds`] is emitted.
//!
//! Spending is accounted as the drop in the pocket's unspent balance, so
//! routing fees, input fees and unreturned fee reserve are all included.

pub mod error;
pub mod event;
pub mod handler;
pub mod refill;
pub mod registry;
pub mod seed;
pub mod transfer;

pub use self::error::Error;
pub use self::event::{OutOfFundsReason, PocketEvent};
pub use self::handler::PocketNwcHandler;
pub use self::refill::Refiller;
pub use self::registry::{
    AutoRefill, PocketConfig, PocketRegistry, RefillPolicy, SpendLock, MAIN_SPEND_LOCK,
};
pub use self::seed::derive_pocket_seed;
pub use self::transfer::{
    is_transient, receive_with_retry, sweep_excess, sweep_pocket, transfer_to_pocket,
    transfer_to_pocket_split, SweepReport,
};

/// Millisatoshis per satoshi.
pub const MSAT_PER_SAT: u64 = 1_000;
