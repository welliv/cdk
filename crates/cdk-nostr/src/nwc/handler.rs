//! Handler trait implemented by wallet backends.
//!
//! A backend (e.g. a Cashu wallet) implements [`NwcRequestHandler`] to service
//! the NIP-47 commands. The handler is intentionally decoupled from any
//! relay/transport concerns: the [`crate::nwc::NwcService`] owns the Nostr relay
//! connection, decryption, authorization and response encoding, and only calls
//! into the handler with already-validated, decrypted requests.
//!
//! Every method returns either a typed NIP-47 response or a [`NIP47Error`],
//! which the service serializes into the encrypted response event. Handlers
//! must never panic: any internal failure should be surfaced as a
//! [`NIP47Error`] with an appropriate [`ErrorCode`](nostr::prelude::nip47::ErrorCode).

use async_trait::async_trait;
use nostr::prelude::nip47::{
    GetBalanceResponse, GetInfoResponse, ListTransactionsRequest, LookupInvoiceRequest,
    LookupInvoiceResponse, MakeInvoiceRequest, MakeInvoiceResponse, NIP47Error, PayInvoiceRequest,
    PayInvoiceResponse,
};

/// Context for a decoded NIP-47 request.
///
/// The context carries information that is available at the transport layer
/// but not in the request body. Policy handlers use it for replay protection,
/// per-connection authorization and auditing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContext {
    /// Hex-encoded Nostr request event ID.
    pub event_id: String,
    /// Hex-encoded authorized client public key.
    pub client_pubkey: String,
    /// Unix timestamp from the Nostr request event's `created_at` field.
    pub created_at: u64,
    /// Unix timestamp when the wallet service received the request.
    pub received_at: u64,
}

/// Backend that services NIP-47 Nostr Wallet Connect requests.
///
/// Implementations are expected to be cheap to clone/share (e.g. wrap an
/// `Arc`), since the service may dispatch requests concurrently.
#[async_trait]
pub trait NwcRequestHandler: Send + Sync {
    /// Handle a `get_info` request.
    async fn get_info(&self) -> Result<GetInfoResponse, NIP47Error>;

    /// Handle a `get_balance` request. The returned balance is in millisatoshis.
    async fn get_balance(&self) -> Result<GetBalanceResponse, NIP47Error>;

    /// Handle a `make_invoice` request.
    async fn make_invoice(
        &self,
        request: MakeInvoiceRequest,
    ) -> Result<MakeInvoiceResponse, NIP47Error>;

    /// Handle a `pay_invoice` request.
    async fn pay_invoice(
        &self,
        request: PayInvoiceRequest,
    ) -> Result<PayInvoiceResponse, NIP47Error>;

    /// Handle a `lookup_invoice` request.
    async fn lookup_invoice(
        &self,
        request: LookupInvoiceRequest,
    ) -> Result<LookupInvoiceResponse, NIP47Error>;

    /// Handle a `list_transactions` request.
    async fn list_transactions(
        &self,
        request: ListTransactionsRequest,
    ) -> Result<Vec<LookupInvoiceResponse>, NIP47Error>;

    /// Context-aware `get_info`. Defaults to [`Self::get_info`].
    async fn get_info_with_context(
        &self,
        _ctx: &RequestContext,
    ) -> Result<GetInfoResponse, NIP47Error> {
        self.get_info().await
    }

    /// Context-aware `get_balance`. Defaults to [`Self::get_balance`].
    async fn get_balance_with_context(
        &self,
        _ctx: &RequestContext,
    ) -> Result<GetBalanceResponse, NIP47Error> {
        self.get_balance().await
    }

    /// Context-aware `make_invoice`. Defaults to [`Self::make_invoice`].
    async fn make_invoice_with_context(
        &self,
        request: MakeInvoiceRequest,
        _ctx: &RequestContext,
    ) -> Result<MakeInvoiceResponse, NIP47Error> {
        self.make_invoice(request).await
    }

    /// Context-aware `pay_invoice`. Defaults to [`Self::pay_invoice`].
    async fn pay_invoice_with_context(
        &self,
        request: PayInvoiceRequest,
        _ctx: &RequestContext,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        self.pay_invoice(request).await
    }

    /// Context-aware `lookup_invoice`. Defaults to [`Self::lookup_invoice`].
    async fn lookup_invoice_with_context(
        &self,
        request: LookupInvoiceRequest,
        _ctx: &RequestContext,
    ) -> Result<LookupInvoiceResponse, NIP47Error> {
        self.lookup_invoice(request).await
    }

    /// Context-aware `list_transactions`. Defaults to [`Self::list_transactions`].
    async fn list_transactions_with_context(
        &self,
        request: ListTransactionsRequest,
        _ctx: &RequestContext,
    ) -> Result<Vec<LookupInvoiceResponse>, NIP47Error> {
        self.list_transactions(request).await
    }
}
