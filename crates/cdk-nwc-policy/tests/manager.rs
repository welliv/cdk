#![allow(missing_docs)]

use std::sync::Arc;

use async_trait::async_trait;
use cdk_nostr::nwc::nip47::{
    ErrorCode, GetBalanceResponse, GetInfoResponse, ListTransactionsRequest, LookupInvoiceRequest,
    LookupInvoiceResponse, MakeInvoiceRequest, MakeInvoiceResponse, NIP47Error, PayInvoiceRequest,
    PayInvoiceResponse,
};
use cdk_nostr::nwc::NwcRequestHandler;
use cdk_nwc_policy::manager::ConnectionManager;
use cdk_nwc_policy::{BudgetPeriod, ConnectionPolicy};

struct MockWallet;

#[async_trait]
impl NwcRequestHandler for MockWallet {
    async fn get_info(&self) -> Result<GetInfoResponse, NIP47Error> {
        Err(error())
    }
    async fn get_balance(&self) -> Result<GetBalanceResponse, NIP47Error> {
        Ok(GetBalanceResponse { balance: 0 })
    }
    async fn make_invoice(
        &self,
        _request: MakeInvoiceRequest,
    ) -> Result<MakeInvoiceResponse, NIP47Error> {
        Err(error())
    }
    async fn pay_invoice(
        &self,
        _request: PayInvoiceRequest,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        Err(error())
    }
    async fn lookup_invoice(
        &self,
        _request: LookupInvoiceRequest,
    ) -> Result<LookupInvoiceResponse, NIP47Error> {
        Err(error())
    }
    async fn list_transactions(
        &self,
        _request: ListTransactionsRequest,
    ) -> Result<Vec<LookupInvoiceResponse>, NIP47Error> {
        Err(error())
    }
}

fn error() -> NIP47Error {
    NIP47Error {
        code: ErrorCode::Internal,
        message: "unused".to_owned(),
    }
}

fn policy() -> ConnectionPolicy {
    ConnectionPolicy {
        id: "ignored".to_owned(),
        name: "ignored".to_owned(),
        client_pubkey: "ignored".to_owned(),
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
fn create_connection_registers_policy_and_returns_uri() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut manager = ConnectionManager::new(
        dir.path().join("manager.json"),
        dir.path().join("policy.json"),
        Arc::new(MockWallet),
    )
    .expect("manager");

    let uri = manager
        .create_connection(
            "Test app".to_owned(),
            vec!["ws://127.0.0.1:1234".to_owned()],
            policy(),
        )
        .expect("create connection");

    assert!(uri.starts_with("nostr+walletconnect://"));
    assert!(uri.contains("secret="));
    let records = manager.list_connections();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, "Test app");
    assert_eq!(records[0].relays, vec!["ws://127.0.0.1:1234".to_owned()]);

    let policies = manager.policy_handler().connections().expect("policies");
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0].client_pubkey, records[0].client_pubkey);
    assert_eq!(policies[0].name, "Test app");
}

#[test]
fn manager_state_survives_restart() {
    let dir = tempfile::tempdir().expect("temp dir");
    let manager_path = dir.path().join("manager.json");
    let policy_path = dir.path().join("policy.json");

    {
        let mut manager = ConnectionManager::new(&manager_path, &policy_path, Arc::new(MockWallet))
            .expect("manager");
        manager
            .create_connection(
                "Persistent app".to_owned(),
                vec!["ws://127.0.0.1:1234".to_owned()],
                policy(),
            )
            .expect("create connection");
    }

    let manager = ConnectionManager::new(&manager_path, &policy_path, Arc::new(MockWallet))
        .expect("reopened manager");
    let records = manager.list_connections();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, "Persistent app");
    assert_eq!(
        manager
            .policy_handler()
            .connections()
            .expect("policies")
            .len(),
        1
    );
}

#[tokio::test]
async fn revoke_marks_policy_revoked() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut manager = ConnectionManager::new(
        dir.path().join("manager.json"),
        dir.path().join("policy.json"),
        Arc::new(MockWallet),
    )
    .expect("manager");
    manager
        .create_connection(
            "Revoked app".to_owned(),
            vec!["ws://127.0.0.1:1234".to_owned()],
            policy(),
        )
        .expect("create connection");
    let id = manager.list_connections()[0].id.clone();

    manager.revoke(&id).await.expect("revoke");

    assert!(manager.policy_handler().connections().expect("policies")[0].revoked);
}

#[tokio::test]
async fn start_all_starts_revoked_and_expired_connections() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut manager = ConnectionManager::new(
        dir.path().join("manager.json"),
        dir.path().join("policy.json"),
        Arc::new(MockWallet),
    )
    .expect("manager");

    manager
        .create_connection(
            "Active app".to_owned(),
            vec!["ws://127.0.0.1:1234".to_owned()],
            policy(),
        )
        .expect("create active");
    manager
        .create_connection(
            "Revoked app".to_owned(),
            vec!["ws://127.0.0.1:1235".to_owned()],
            policy(),
        )
        .expect("create revoked");
    let mut expired_policy = policy();
    expired_policy.expires_at = Some(1);
    manager
        .create_connection(
            "Expired app".to_owned(),
            vec!["ws://127.0.0.1:1236".to_owned()],
            expired_policy,
        )
        .expect("create expired");

    let revoked_id = manager
        .list_connections()
        .iter()
        .find(|r| r.name == "Revoked app")
        .expect("revoked")
        .id
        .clone();
    manager.revoke(&revoked_id).await.expect("revoke");

    let started = manager.start_all().await.expect("start_all");
    assert_eq!(
        started, 3,
        "revoked and expired connections must be started so they can reply UNAUTHORIZED"
    );
    assert_eq!(manager.active_count(), 3);
}

#[tokio::test]
async fn start_and_revoke_keep_service_running_to_reject_requests() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut manager = ConnectionManager::new(
        dir.path().join("manager.json"),
        dir.path().join("policy.json"),
        Arc::new(MockWallet),
    )
    .expect("manager");
    manager
        .create_connection(
            "Soon revoked".to_owned(),
            vec!["ws://127.0.0.1:1234".to_owned()],
            policy(),
        )
        .expect("create");
    let id = manager.list_connections()[0].id.clone();

    manager.start(&id).await.expect("start");
    assert_eq!(manager.active_count(), 1);

    manager.revoke(&id).await.expect("revoke");
    assert!(manager.policy_handler().connections().expect("policies")[0].revoked);
    assert_eq!(
        manager.active_count(),
        1,
        "revoking must leave the service running so clients get UNAUTHORIZED, not a timeout"
    );

    // Starting a connection that is already revoked must also work (e.g. after restart).
    manager.stop(&id).await.expect("stop");
    manager
        .start(&id)
        .await
        .expect("revoked connection must still start so it can reply UNAUTHORIZED");
    assert_eq!(manager.active_count(), 1);
}
