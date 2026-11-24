//! Forward NWC requests to an upstream NWC wallet service.

#![allow(missing_docs)]

use std::time::Duration;

use async_trait::async_trait;
use cdk_nostr::nwc::nip47::{
    ErrorCode, GetBalanceResponse, GetInfoResponse, ListTransactionsRequest, LookupInvoiceRequest,
    LookupInvoiceResponse, MakeInvoiceRequest, MakeInvoiceResponse, NIP47Error, PayInvoiceRequest,
    PayInvoiceResponse,
};
use cdk_nostr::nwc::NwcRequestHandler;
use nwc::prelude::{Error as NwcError, NostrWalletConnect, NostrWalletConnectUri};

/// Forwards every request to an upstream NWC connection.
#[derive(Debug, Clone)]
pub struct ForwardingNwcHandler {
    client: NostrWalletConnect,
}

impl ForwardingNwcHandler {
    /// Create a forwarding handler from a parsed upstream URI.
    pub fn new(uri: NostrWalletConnectUri, timeout: Duration) -> Self {
        Self {
            client: NostrWalletConnect::builder(uri).timeout(timeout).build(),
        }
    }

    /// Create a forwarding handler from a URI string.
    pub fn from_uri(uri: &str, timeout: Duration) -> Result<Self, NwcError> {
        let uri = NostrWalletConnectUri::parse(uri)?;
        Ok(Self::new(uri, timeout))
    }
}

fn map_error(error: NwcError) -> NIP47Error {
    NIP47Error {
        code: ErrorCode::Other,
        message: error.to_string(),
    }
}

#[async_trait]
impl NwcRequestHandler for ForwardingNwcHandler {
    async fn get_info(&self) -> Result<GetInfoResponse, NIP47Error> {
        self.client.get_info().await.map_err(map_error)
    }

    async fn get_balance(&self) -> Result<GetBalanceResponse, NIP47Error> {
        self.client.get_balance().await.map_err(map_error)
    }

    async fn make_invoice(
        &self,
        request: MakeInvoiceRequest,
    ) -> Result<MakeInvoiceResponse, NIP47Error> {
        self.client.make_invoice(request).await.map_err(map_error)
    }

    async fn pay_invoice(
        &self,
        request: PayInvoiceRequest,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        self.client.pay_invoice(request).await.map_err(map_error)
    }

    async fn lookup_invoice(
        &self,
        request: LookupInvoiceRequest,
    ) -> Result<LookupInvoiceResponse, NIP47Error> {
        self.client.lookup_invoice(request).await.map_err(map_error)
    }

    async fn list_transactions(
        &self,
        request: ListTransactionsRequest,
    ) -> Result<Vec<LookupInvoiceResponse>, NIP47Error> {
        self.client
            .list_transactions(request)
            .await
            .map_err(map_error)
    }
}
