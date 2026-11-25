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
    upstream_error_from_message(&error.to_string())
}

/// Recover the upstream NIP-47 error code from a client error message.
///
/// The `nwc` client flattens a wallet error response into the string
/// `"<message> [<ErrorCode:?>]"`. Preserving the code matters for the policy
/// layer: `INSUFFICIENT_BALANCE` proves nothing was dispatched, whereas every
/// other failure is treated as possibly spent. Transport failures and
/// timeouts carry no code and stay `OTHER`.
pub fn upstream_error_from_message(text: &str) -> NIP47Error {
    let parsed = text
        .strip_suffix(']')
        .and_then(|rest| rest.rsplit_once(" ["))
        .and_then(|(message, code)| {
            let code = match code {
                "RateLimited" => ErrorCode::RateLimited,
                "NotImplemented" => ErrorCode::NotImplemented,
                "InsufficientBalance" => ErrorCode::InsufficientBalance,
                "PaymentFailed" => ErrorCode::PaymentFailed,
                "NotFound" => ErrorCode::NotFound,
                "QuotaExceeded" => ErrorCode::QuotaExceeded,
                "Restricted" => ErrorCode::Restricted,
                "Unauthorized" => ErrorCode::Unauthorized,
                "Internal" => ErrorCode::Internal,
                "Other" => ErrorCode::Other,
                _ => return None,
            };
            Some((code, message))
        });
    match parsed {
        Some((code, message)) => NIP47Error {
            code,
            message: message.to_owned(),
        },
        None => NIP47Error {
            code: ErrorCode::Other,
            message: text.to_owned(),
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_upstream_error_codes() {
        let error = upstream_error_from_message("not enough funds [InsufficientBalance]");
        assert_eq!(error.code, ErrorCode::InsufficientBalance);
        assert_eq!(error.message, "not enough funds");

        // Observed live from an Alby Hub Cashu wallet on signet.
        let error = upstream_error_from_message("outputs are pending [Internal]");
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(error.message, "outputs are pending");
    }

    #[test]
    fn only_the_trailing_code_counts() {
        let error = upstream_error_from_message("fake [InsufficientBalance] [PaymentFailed]");
        assert_eq!(error.code, ErrorCode::PaymentFailed);
        assert_eq!(error.message, "fake [InsufficientBalance]");
    }

    #[test]
    fn transport_errors_stay_other() {
        for text in ["response not received", "timeout [Bogus]", "", "[Internal]"] {
            let error = upstream_error_from_message(text);
            assert_eq!(error.code, ErrorCode::Other, "{text:?}");
            assert_eq!(error.message, text);
        }
    }
}
