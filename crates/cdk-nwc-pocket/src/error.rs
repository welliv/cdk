//! Errors for pocket operations.

use thiserror::Error;

/// Pocket errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Wallet operation failed.
    #[error(transparent)]
    Wallet(#[from] cdk::Error),
    /// Disk input/output error.
    #[error("io error: {0}")]
    Io(String),
    /// Registry serialization error.
    #[error("serialization error: {0}")]
    Serde(String),
    /// No pocket is registered for the connection.
    #[error("pocket not found for connection {connection_id}")]
    UnknownPocket {
        /// Connection ID.
        connection_id: String,
    },
    /// The pocket was swept on revoke or expiry and is closed.
    #[error("pocket for connection {connection_id} was swept and is closed")]
    PocketSwept {
        /// Connection ID.
        connection_id: String,
    },
    /// A pocket with this connection ID or derivation index already exists.
    #[error("duplicate pocket")]
    DuplicatePocket,
    /// A transfer would take the pocket above its ceiling.
    #[error("pocket ceiling exceeded: balance {balance_msat} msat + {amount_msat} msat > ceiling {ceiling_msat} msat")]
    CeilingExceeded {
        /// Current pocket balance.
        balance_msat: u64,
        /// Requested transfer.
        amount_msat: u64,
        /// Pocket ceiling.
        ceiling_msat: u64,
    },
    /// The refill cap for the current window is used up.
    #[error("refill cap reached")]
    RefillCapReached,
    /// The pocket wallet's unit is not supported.
    #[error("only sat-denominated pockets are supported")]
    UnsupportedUnit,
}
