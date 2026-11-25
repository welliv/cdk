//! Regression tests for defects found while battle-testing the NWC policy
//! layer live on signet (Mutinynet).

#![allow(missing_docs)]

use std::sync::Arc;

use async_trait::async_trait;
use cdk_nostr::nwc::handler::RequestContext;
use cdk_nostr::nwc::nip47::{
    ErrorCode, GetBalanceResponse, GetInfoResponse, ListTransactionsRequest, LookupInvoiceRequest,
    LookupInvoiceResponse, MakeInvoiceRequest, MakeInvoiceResponse, NIP47Error, PayInvoiceRequest,
    PayInvoiceResponse,
};
use cdk_nostr::nwc::NwcRequestHandler;
use cdk_nwc_policy::{BudgetPeriod, ConnectionPolicy, PolicyNwcHandler, PolicyStore};

const DAY: u64 = 86_400;
const INVOICE: &str = "lnbc100n1pnvpufspp5djn8hrq49r8cghwye9kqw752qjncwyfnrprhprpqk43mwcy4yfsqdq5g9kxy7fqd9h8vmmfvdjscqzzsxqyz5vqsp5uhpjt36rj75pl7jq2sshaukzfkt7uulj456s4mh7uy7l6vx7lvxs9qxpqysgqedwz08acmqwtk8g4vkwm2w78suwt2qyzz6jkkwcgrjm3r3hs6fskyhvud4fan3keru7emjm8ygqpcrwtlmhfjfmer3afs5hhwamgr4cqtactdq";
const INVOICE_MSAT: u64 = 10_000;

fn policy() -> ConnectionPolicy {
    ConnectionPolicy {
        id: "app-1".to_owned(),
        name: "Test app".to_owned(),
        client_pubkey: "client-pubkey".to_owned(),
        allowed_methods: vec!["get_balance".to_owned(), "pay_invoice".to_owned()],
        budget_msat: Some(10_000),
        budget_period: BudgetPeriod::Daily,
        max_single_payment_msat: Some(20_000),
        max_fee_reserve_msat: 100,
        fee_overage_msat: 0,
        expires_at: None,
        revoked: false,
        used_msat: 0,
        period_start_secs: 1_000,
    }
}

/// A payment authorized in period N that settles after the budget rolled into
/// period N+1 must be charged somewhere. Before the fix the rollover zeroed
/// `used_msat` and the later commit was skipped because the reservation's
/// period no longer matched, so the payment was never charged at all.
#[test]
fn in_flight_payment_across_rollover_is_charged() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");
    store.add_connection(policy()).expect("add connection");

    // Period 1: reserve 8_000 for a payment that is still in flight.
    let in_flight = store
        .authorize("client-pubkey", "pay_invoice", "event-1", 8_000, 1_001)
        .expect("reserve in period 1");

    // Period 2 begins (any request triggers the rollover).
    store
        .authorize("client-pubkey", "get_balance", "event-2", 0, DAY + 1_002)
        .expect("rollover request");

    // The in-flight payment settles in period 2.
    store.commit(&in_flight.id, 8_000).expect("commit");
    assert_eq!(
        store.used_msat("app-1").expect("used"),
        Some(8_000),
        "a payment that settles after rollover must be charged"
    );

    // Only 2_000 of the 10_000 period budget remains.
    let err = store
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-3",
            3_000,
            DAY + 1_003,
        )
        .expect_err("budget must account for the straddling payment");
    assert!(matches!(err, cdk_nwc_policy::PolicyError::QuotaExceeded));
    store
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-4",
            2_000,
            DAY + 1_004,
        )
        .expect("remaining budget is still usable");
}

/// While a reservation from the previous period is unresolved it must keep
/// counting against the budget, otherwise a fresh period could be spent in
/// full on top of an outstanding payment.
#[test]
fn in_flight_reservation_counts_in_new_period_until_released() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");
    store.add_connection(policy()).expect("add connection");

    let in_flight = store
        .authorize("client-pubkey", "pay_invoice", "event-1", 8_000, 1_001)
        .expect("reserve in period 1");

    let err = store
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-2",
            8_000,
            DAY + 1_002,
        )
        .expect_err("outstanding reservation is carried into the new period");
    assert!(matches!(err, cdk_nwc_policy::PolicyError::QuotaExceeded));

    store.release(&in_flight.id).expect("release");
    assert_eq!(store.used_msat("app-1").expect("used"), Some(0));
    store
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-3",
            8_000,
            DAY + 1_003,
        )
        .expect("budget available once the reservation is released");
}

/// Committing a carried reservation with less than reserved refunds the
/// difference in the new period, exactly like a same-period commit.
#[test]
fn carried_reservation_refunds_unused_fee_reserve() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");
    store.add_connection(policy()).expect("add connection");

    let in_flight = store
        .authorize("client-pubkey", "pay_invoice", "event-1", 8_000, 1_001)
        .expect("reserve");
    store
        .authorize("client-pubkey", "get_balance", "event-2", 0, DAY + 1_002)
        .expect("rollover");
    store.commit(&in_flight.id, 6_500).expect("commit");
    assert_eq!(store.used_msat("app-1").expect("used"), Some(6_500));
}

struct BrokeWallet;

#[async_trait]
impl NwcRequestHandler for BrokeWallet {
    async fn get_info(&self) -> Result<GetInfoResponse, NIP47Error> {
        Err(err(ErrorCode::Internal, "unused"))
    }
    async fn get_balance(&self) -> Result<GetBalanceResponse, NIP47Error> {
        Ok(GetBalanceResponse { balance: 0 })
    }
    async fn make_invoice(
        &self,
        _request: MakeInvoiceRequest,
    ) -> Result<MakeInvoiceResponse, NIP47Error> {
        Err(err(ErrorCode::Internal, "unused"))
    }
    async fn pay_invoice(
        &self,
        _request: PayInvoiceRequest,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        Err(err(
            ErrorCode::InsufficientBalance,
            "insufficient balance to pay invoice",
        ))
    }
    async fn lookup_invoice(
        &self,
        _request: LookupInvoiceRequest,
    ) -> Result<LookupInvoiceResponse, NIP47Error> {
        Err(err(ErrorCode::Internal, "unused"))
    }
    async fn list_transactions(
        &self,
        _request: ListTransactionsRequest,
    ) -> Result<Vec<LookupInvoiceResponse>, NIP47Error> {
        Err(err(ErrorCode::Internal, "unused"))
    }
}

fn err(code: ErrorCode, message: &str) -> NIP47Error {
    NIP47Error {
        code,
        message: message.to_owned(),
    }
}

/// Found live: with an empty wallet every `pay_invoice` failed with
/// INSUFFICIENT_BALANCE yet charged the full reserve, so six failed attempts
/// exhausted a 100 sat daily budget without a single sat leaving the wallet.
/// Insufficient balance is decided before any payment is dispatched, so the
/// reservation must be released.
#[tokio::test]
async fn insufficient_balance_releases_reservation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let handler =
        PolicyNwcHandler::new(dir.path().join("policy.json"), Arc::new(BrokeWallet)).expect("h");
    let mut policy = policy();
    policy.budget_msat = Some(INVOICE_MSAT * 3);
    handler.add_connection(policy).expect("add");

    for i in 0..10 {
        let ctx = RequestContext {
            event_id: format!("event-{i}"),
            client_pubkey: "client-pubkey".to_owned(),
            received_at: 1_001,
        };
        let error = handler
            .pay_invoice_with_context(PayInvoiceRequest::new(INVOICE), &ctx)
            .await
            .expect_err("wallet is empty");
        assert_eq!(
            error.code,
            ErrorCode::InsufficientBalance,
            "attempt {i} must reach the wallet, not be blocked by a drained budget"
        );
    }
    assert_eq!(handler.used_msat("app-1"), Some(0));
}
