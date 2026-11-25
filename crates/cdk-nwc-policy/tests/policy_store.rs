#![allow(missing_docs)]

use cdk_nwc_policy::{BudgetPeriod, ConnectionPolicy, PolicyError, PolicyStore};

fn test_policy() -> ConnectionPolicy {
    ConnectionPolicy {
        id: "app-1".to_owned(),
        name: "Test app".to_owned(),
        client_pubkey: "client-pubkey".to_owned(),
        allowed_methods: vec!["get_balance".to_owned(), "pay_invoice".to_owned()],
        budget_msat: Some(10_000),
        budget_period: BudgetPeriod::Daily,
        max_single_payment_msat: Some(8_000),
        max_fee_reserve_msat: 100,
        fee_overage_msat: 0,
        expires_at: None,
        revoked: false,
        used_msat: 0,
        period_start_secs: 1_000,
    }
}

#[test]
fn rejects_unknown_client() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");

    let err = store
        .authorize("unknown", "get_balance", "event-1", 0, 1_001)
        .expect_err("unknown client must be rejected");

    assert!(matches!(err, PolicyError::Unauthorized));
}

#[test]
fn rejects_disallowed_method() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");
    store.add_connection(test_policy()).expect("add connection");

    let err = store
        .authorize("client-pubkey", "make_invoice", "event-1", 0, 1_001)
        .expect_err("disallowed method must be rejected");

    assert!(matches!(err, PolicyError::Restricted(_)));
}

#[test]
fn enforces_budget_and_commits_actual_spend() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");
    store.add_connection(test_policy()).expect("add connection");

    let auth = store
        .authorize("client-pubkey", "pay_invoice", "event-1", 6_000, 1_001)
        .expect("first payment within budget");
    assert_eq!(auth.amount_msat, 6_000);

    let err = store
        .authorize("client-pubkey", "pay_invoice", "event-2", 5_000, 1_002)
        .expect_err("second payment must exceed remaining budget");
    assert!(matches!(err, PolicyError::QuotaExceeded));

    store.commit(&auth.id, 4_000).expect("commit actual spend");

    let second = store
        .authorize("client-pubkey", "pay_invoice", "event-3", 6_000, 1_003)
        .expect("budget restored after actual spend commit");
    assert_eq!(second.amount_msat, 6_000);
}

#[test]
fn rejects_single_payment_above_cap() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");
    store.add_connection(test_policy()).expect("add connection");

    let err = store
        .authorize("client-pubkey", "pay_invoice", "event-1", 8_001, 1_001)
        .expect_err("payment above single cap must be rejected");

    assert!(matches!(err, PolicyError::PaymentCapExceeded));
    assert_eq!(err.to_string(), "payment exceeds per-payment cap");
}

#[test]
fn rejects_replayed_event_after_restart() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("policy.json");

    {
        let mut store = PolicyStore::new(path.clone()).expect("store");
        store.add_connection(test_policy()).expect("add connection");
        store
            .authorize("client-pubkey", "pay_invoice", "event-1", 1_000, 1_001)
            .expect("first use of event");
    }

    let mut reloaded = PolicyStore::new(path).expect("store");
    let err = reloaded
        .authorize("client-pubkey", "pay_invoice", "event-1", 1_000, 1_002)
        .expect_err("replayed event must be rejected after restart");

    assert!(matches!(err, PolicyError::Replayed));
}

#[test]
fn rejects_expired_and_revoked_connections() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");

    let mut expired = test_policy();
    expired.id = "expired".to_owned();
    expired.client_pubkey = "expired-key".to_owned();
    expired.expires_at = Some(1_000);
    store.add_connection(expired).expect("add expired");

    let mut revoked = test_policy();
    revoked.id = "revoked".to_owned();
    revoked.client_pubkey = "revoked-key".to_owned();
    revoked.revoked = true;
    store.add_connection(revoked).expect("add revoked");

    let expired_err = store
        .authorize("expired-key", "get_balance", "event-1", 0, 1_001)
        .expect_err("expired connection must be rejected");
    assert!(matches!(expired_err, PolicyError::Expired));

    let revoked_err = store
        .authorize("revoked-key", "get_balance", "event-2", 0, 1_001)
        .expect_err("revoked connection must be rejected");
    assert!(matches!(revoked_err, PolicyError::Unauthorized));
}

#[test]
fn corrupt_ledger_fails_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("policy.json");
    std::fs::write(&path, "not json").expect("write corrupt ledger");

    let result = PolicyStore::new(&path);

    assert!(matches!(result, Err(PolicyError::Serde(_))));
}

#[test]
fn rejects_duplicate_client_pubkey() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");

    let first = test_policy();
    let mut second = test_policy();
    second.id = "app-2".to_owned();
    store.add_connection(first).expect("add first");

    let error = store
        .add_connection(second)
        .expect_err("duplicate client key");

    assert!(matches!(error, PolicyError::DuplicateClient));
}

#[test]
fn commit_caps_overage_instead_of_releasing_a_successful_spend() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");
    store.add_connection(test_policy()).expect("add connection");

    let auth = store
        .authorize("client-pubkey", "pay_invoice", "event-1", 1_000, 1_001)
        .expect("authorize");
    store
        .commit(&auth.id, 1_500)
        .expect("overage must be charged to the reservation");

    assert_eq!(store.used_msat("app-1").expect("used"), Some(1_000));
}

#[test]
fn rejects_duplicate_client_on_load() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("policy.json");
    let first = test_policy();
    let mut second = test_policy();
    second.id = "app-2".to_owned();
    let json = serde_json::json!({
        "connections": [first, second],
        "events": [],
        "reservations": []
    });
    std::fs::write(&path, json.to_string()).expect("write ledger");

    let result = PolicyStore::new(&path);

    assert!(matches!(result, Err(PolicyError::DuplicateClient)));
}

#[test]
fn two_stores_cannot_double_authorize_the_same_event() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("policy.json");
    let mut first = PolicyStore::new(&path).expect("first store");
    first.add_connection(test_policy()).expect("add connection");
    let mut second = PolicyStore::new(&path).expect("second store");

    first
        .authorize("client-pubkey", "pay_invoice", "event-1", 1_000, 1_001)
        .expect("first authorization");

    let error = second
        .authorize("client-pubkey", "pay_invoice", "event-1", 1_000, 1_002)
        .expect_err("second store must re-read the ledger and reject replay");

    assert!(matches!(error, PolicyError::Replayed));
}

#[test]
fn stale_reservation_is_carried_and_release_frees_new_period() {
    // An unresolved reservation from the previous period is carried into the
    // new one (it may still spend funds), and releasing it frees exactly its
    // amount in the new period.
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");
    let mut policy = test_policy();
    policy.max_single_payment_msat = Some(20_000);
    store.add_connection(policy).expect("add connection");

    let first = store
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-1",
            8_000,
            1_000 + 86_400 - 10,
        )
        .expect("first period payment");
    let blocked = store
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-2",
            8_000,
            86_400 + 1_002,
        )
        .expect_err("carried reservation still counts in the new period");
    assert!(matches!(blocked, PolicyError::QuotaExceeded));
    let second = store
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-3",
            2_000,
            86_400 + 1_003,
        )
        .expect("remaining budget usable");
    assert_eq!(second.amount_msat, 2_000);

    store.release(&first.id).expect("settle stale reservation");

    assert_eq!(store.used_msat("app-1").expect("used"), Some(2_000));
    let third = store
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-4",
            8_000,
            86_400 + 1_004,
        )
        .expect("released budget available in new period");
    assert_eq!(third.amount_msat, 8_000);
}

#[test]
fn lock_failure_does_not_mutate_ledger() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("policy.json");
    let lock_path = path.with_extension("policy.lock");
    {
        let mut store = PolicyStore::new(&path).expect("store");
        store.add_connection(test_policy()).expect("add connection");
        std::fs::remove_file(&lock_path).expect("remove lock file");
        std::fs::create_dir(&lock_path).expect("block lock path");

        let error = store
            .authorize("client-pubkey", "pay_invoice", "event-1", 5_000, 1_001)
            .expect_err("lock failure");

        assert!(matches!(error, PolicyError::Io(_)));
        std::fs::remove_dir(&lock_path).expect("unblock lock path");
        assert_eq!(store.used_msat("app-1").expect("used"), Some(0));
    }
}

#[test]
fn rolls_budget_over_for_new_period() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut store = PolicyStore::new(dir.path().join("policy.json")).expect("store");
    let mut policy = test_policy();
    policy.max_single_payment_msat = Some(20_000);
    store.add_connection(policy).expect("add connection");

    let auth = store
        .authorize("client-pubkey", "pay_invoice", "event-1", 10_000, 1_001)
        .expect("spend full daily budget");
    store.commit(&auth.id, 10_000).expect("commit spend");

    let rolled = store
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-2",
            1_000,
            86_400 + 1_002,
        )
        .expect("budget resets next day");
    assert_eq!(rolled.amount_msat, 1_000);

    let fresh = store
        .authorize(
            "client-pubkey",
            "pay_invoice",
            "event-3",
            1_000,
            86_400 + 1_003,
        )
        .expect("budget still available next day");
    assert_eq!(fresh.amount_msat, 1_000);
}
