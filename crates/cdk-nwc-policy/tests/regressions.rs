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
use cdk_nwc_policy::{BudgetPeriod, ConnectionPolicy, PolicyError, PolicyNwcHandler, PolicyStore};

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
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-1",
            8_000,
            1_000 + 86_400 - 10,
        )
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
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-1",
            8_000,
            1_000 + 86_400 - 10,
        )
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
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-1",
            8_000,
            1_000 + 86_400 - 10,
        )
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
            created_at: 1_001,
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

#[test]
fn concurrent_create_connection_preserves_other_process_records() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mgr_path = dir.path().join("manager.json");
    let pol_path = dir.path().join("policy.json");

    let mut mgr_a = cdk_nwc_policy::manager::ConnectionManager::new(
        &mgr_path,
        &pol_path,
        Arc::new(BrokeWallet),
    )
    .expect("mgr_a");
    let mut mgr_b = cdk_nwc_policy::manager::ConnectionManager::new(
        &mgr_path,
        &pol_path,
        Arc::new(BrokeWallet),
    )
    .expect("mgr_b");

    mgr_a
        .create_connection(
            "app-a".to_owned(),
            vec!["wss://relay.example".to_owned()],
            policy(),
        )
        .expect("create a");
    mgr_b
        .create_connection(
            "app-b".to_owned(),
            vec!["wss://relay.example".to_owned()],
            policy(),
        )
        .expect("create b");

    let on_disk =
        cdk_nwc_policy::manager::ConnectionManager::<BrokeWallet>::list_records(&mgr_path)
            .expect("list");
    assert_eq!(
        on_disk.len(),
        2,
        "second manager must not overwrite first connection"
    );
}

/// Simulate a crash between `authorize` and `commit`/`release`: a reservation
/// older than the reservation TTL expires (settled fail-closed in its own
/// period) and must not stay in the ledger forever or brick future periods.
#[test]
fn stale_reservation_expires_and_does_not_brick_future_periods() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("policy.json");
    let mut store = PolicyStore::new(&path).expect("store");
    store.add_connection(policy()).expect("add connection");

    // Process reserves 8_000 at t=1_001 and crashes before commit/release.
    let crashed = store
        .authorize("client-pubkey", "pay_invoice", "event-crash", 8_000, 1_001)
        .expect("reserve before crash");
    drop(store);

    // Next day (well past reservation TTL and daily rollover), process restarts.
    let mut reopened = PolicyStore::new(&path).expect("reopen");
    // Period 2 must not carry the crashed reservation from yesterday.
    reopened
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-next-day",
            9_000,
            DAY + 1_002,
        )
        .expect("expired reservation must not brick next period's budget");

    // Late commit for the already-expired reservation must not subtract from period 2.
    let _ = reopened.commit(&crashed.id, 0);
    assert_eq!(reopened.used_msat("app-1").expect("used"), Some(9_000));
}

/// Replay ledger stays bounded by pruning event IDs past the acceptance window,
/// while replaying an old event whose ID was pruned is still rejected by age.
#[test]
fn replay_ledger_prunes_expired_events_and_rejects_stale_replay() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("policy.json");
    let mut store = PolicyStore::new(&path)
        .expect("store")
        .with_replay_window(60);
    store.add_connection(policy()).expect("add connection");

    // Record 50 events at t=1_000..1_050.
    for i in 0..50u64 {
        store
            .authorize_event(
                "client-pubkey",
                "get_balance",
                &format!("evt-{i}"),
                0,
                1_000 + i,
                1_000 + i,
            )
            .expect("authorize");
    }
    assert_eq!(store.seen_event_count().expect("count"), 50);

    // Advance past the 60s window for all 50 events; authorizing one new event
    // must prune every expired ID from the ledger.
    store
        .authorize_event("client-pubkey", "get_balance", "evt-fresh", 0, 1_200, 1_200)
        .expect("fresh event");
    assert_eq!(store.seen_event_count().expect("pruned count"), 1);

    // Replaying `evt-0` (now pruned from `seen_events`) with its original
    // `created_at` must STILL be rejected because it is outside the window.
    let err = store
        .authorize_event("client-pubkey", "get_balance", "evt-0", 0, 1_000, 1_201)
        .expect_err("pruned event must be rejected as stale");
    assert!(matches!(err, cdk_nwc_policy::PolicyError::StaleRequest));

    // Events created too far in the future are also rejected so an attacker
    // cannot pin entries in the replay ledger indefinitely.
    let err = store
        .authorize_event(
            "client-pubkey",
            "get_balance",
            "evt-future",
            0,
            5_000,
            1_201,
        )
        .expect_err("far-future event must be rejected");
    assert!(matches!(err, cdk_nwc_policy::PolicyError::StaleRequest));
}

/// Legacy policy files with an `"events": [...]` array must load cleanly,
/// still block immediate replays, and migrate/prune over time.
#[test]
fn legacy_events_array_migrates_and_prunes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("policy.json");
    let mut pol = policy();
    pol.period_start_secs = 1_000;
    let legacy = serde_json::json!({
        "connections": [pol],
        "events": ["legacy-evt-1"],
        "reservations": []
    });
    std::fs::write(&path, serde_json::to_string(&legacy).unwrap()).unwrap();

    let mut store = PolicyStore::new(&path)
        .expect("load legacy")
        .with_replay_window(60);
    let err = store
        .authorize_event(
            "client-pubkey",
            "get_balance",
            "legacy-evt-1",
            0,
            1_010,
            1_010,
        )
        .expect_err("legacy event must still be rejected as replayed");
    assert!(matches!(err, cdk_nwc_policy::PolicyError::Replayed));

    // First write migrates legacy entries with one full retention window from `now`.
    store
        .authorize_event(
            "client-pubkey",
            "get_balance",
            "migrated-at-1010",
            0,
            1_010,
            1_010,
        )
        .expect("migrates legacy");
    // Past that retention window, the migrated legacy entries are pruned.
    store
        .authorize_event("client-pubkey", "get_balance", "new-evt", 0, 1_200, 1_200)
        .expect("fresh");
    assert_eq!(store.seen_event_count().expect("pruned"), 1);
}

/// A payment over the per-payment cap and a payment over the period budget
/// must be distinct errors (same NIP-47 code later, different messages).
#[test]
fn cap_and_budget_errors_use_distinct_messages() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");
    let mut cap_policy = policy();
    cap_policy.max_single_payment_msat = Some(5_000);
    cap_policy.budget_msat = Some(100_000);
    cap_policy.client_pubkey = "cap-key".to_owned();
    cap_policy.id = "cap-app".to_owned();
    store.add_connection(cap_policy).expect("add cap policy");

    let mut budget_policy = policy();
    budget_policy.max_single_payment_msat = Some(100_000);
    budget_policy.budget_msat = Some(5_000);
    budget_policy.client_pubkey = "budget-key".to_owned();
    budget_policy.id = "budget-app".to_owned();
    store
        .add_connection(budget_policy)
        .expect("add budget policy");

    let cap_err = store
        .authorize("cap-key", "pay_invoice", "event-cap", 5_001, 1_001)
        .expect_err("over cap");
    let budget_err = store
        .authorize("budget-key", "pay_invoice", "event-budget", 5_001, 1_001)
        .expect_err("over budget");

    assert_eq!(
        cap_err.to_string(),
        "payment exceeds per-payment cap",
        "cap rejection must use a distinct message"
    );
    assert_eq!(budget_err.to_string(), "budget exceeded");
    assert_ne!(
        cap_err, budget_err,
        "cap and budget must be distinct PolicyError variants"
    );
    assert!(matches!(cap_err, PolicyError::PaymentCapExceeded));
    assert!(matches!(budget_err, PolicyError::QuotaExceeded));
}

/// The policy handler must map both variants to NIP-47 QUOTA_EXCEEDED with
/// the distinct messages.
#[tokio::test]
async fn pay_invoice_cap_and_budget_share_code_but_not_message() {
    let dir = tempfile::tempdir().expect("temp dir");

    let cap_handler = PolicyNwcHandler::new(dir.path().join("cap.json"), Arc::new(BrokeWallet))
        .expect("cap handler");
    let mut cap_policy = policy();
    cap_policy.max_single_payment_msat = Some(5_000);
    cap_policy.budget_msat = Some(1_000_000);
    cap_handler.add_connection(cap_policy).expect("add cap");
    let cap_error = cap_handler
        .pay_invoice_with_context(PayInvoiceRequest::new(INVOICE), &context("event-cap"))
        .await
        .expect_err("invoice plus reserve exceeds cap");
    assert_eq!(cap_error.code, ErrorCode::QuotaExceeded);
    assert_eq!(cap_error.message, "payment exceeds per-payment cap");

    let budget_handler =
        PolicyNwcHandler::new(dir.path().join("budget.json"), Arc::new(BrokeWallet))
            .expect("budget handler");
    let mut budget_policy = policy();
    budget_policy.max_single_payment_msat = Some(1_000_000);
    budget_policy.budget_msat = Some(5_000);
    budget_handler
        .add_connection(budget_policy)
        .expect("add budget");
    let budget_error = budget_handler
        .pay_invoice_with_context(PayInvoiceRequest::new(INVOICE), &context("event-budget"))
        .await
        .expect_err("invoice plus reserve exceeds budget");
    assert_eq!(budget_error.code, ErrorCode::QuotaExceeded);
    assert_eq!(budget_error.message, "budget exceeded");
}

fn context(event: &str) -> RequestContext {
    RequestContext {
        event_id: event.to_owned(),
        client_pubkey: "client-pubkey".to_owned(),
        created_at: 1_001,
        received_at: 1_001,
    }
}
