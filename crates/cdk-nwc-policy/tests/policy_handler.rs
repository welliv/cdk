#![allow(missing_docs)]

use std::sync::Arc;

use async_trait::async_trait;
use cdk_nostr::nwc::handler::RequestContext;
use cdk_nostr::nwc::nip47::{
    ErrorCode, GetBalanceResponse, GetInfoResponse, ListTransactionsRequest, LookupInvoiceRequest,
    LookupInvoiceResponse, MakeInvoiceRequest, MakeInvoiceResponse, Method, NIP47Error,
    PayInvoiceRequest, PayInvoiceResponse,
};
use cdk_nostr::nwc::NwcRequestHandler;
use cdk_nwc_policy::{BudgetPeriod, ConnectionPolicy, PolicyNwcHandler};

const INVOICE: &str = "lnbc100n1pnvpufspp5djn8hrq49r8cghwye9kqw752qjncwyfnrprhprpqk43mwcy4yfsqdq5g9kxy7fqd9h8vmmfvdjscqzzsxqyz5vqsp5uhpjt36rj75pl7jq2sshaukzfkt7uulj456s4mh7uy7l6vx7lvxs9qxpqysgqedwz08acmqwtk8g4vkwm2w78suwt2qyzz6jkkwcgrjm3r3hs6fskyhvud4fan3keru7emjm8ygqpcrwtlmhfjfmer3afs5hhwamgr4cqtactdq";
const FAIL_INVOICE: &str = "lnbc100n1p5z3a63pp56854ytysg7e5z9fl3w5mgvrlqjfcytnjv8ff5hm5qt6gl6alxesqdqqcqzzsxqyz5vqsp5p0x0dlhn27s63j4emxnk26p7f94u0lyarnfp5yqmac9gzy4ngdss9qxpqysgqne3v0hnzt2lp0hc69xpzckk0cdcar7glvjhq60lsrfe8gejdm8c564prrnsft6ctxxyrewp4jtezrq3gxxqnfjj0f9tw2qs9y0lslmqpfu7et9";
const INVOICE_MSAT: u64 = 10_000;

struct MockWallet;

#[async_trait]
impl NwcRequestHandler for MockWallet {
    async fn get_info(&self) -> Result<GetInfoResponse, NIP47Error> {
        Ok(GetInfoResponse {
            alias: None,
            color: None,
            pubkey: None,
            network: None,
            block_height: None,
            block_hash: None,
            methods: vec![
                Method::PayInvoice,
                Method::MakeInvoice,
                Method::LookupInvoice,
                Method::ListTransactions,
                Method::GetBalance,
                Method::GetInfo,
            ],
            notifications: vec![],
        })
    }

    async fn get_balance(&self) -> Result<GetBalanceResponse, NIP47Error> {
        Ok(GetBalanceResponse { balance: 100_000 })
    }

    async fn make_invoice(
        &self,
        _request: MakeInvoiceRequest,
    ) -> Result<MakeInvoiceResponse, NIP47Error> {
        Err(nip47_error(ErrorCode::Internal, "no invoice"))
    }

    async fn pay_invoice(
        &self,
        request: PayInvoiceRequest,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        if request.invoice == FAIL_INVOICE {
            return Err(nip47_error(ErrorCode::PaymentFailed, "wallet failed"));
        }
        Ok(PayInvoiceResponse {
            preimage: "00".repeat(32),
            fees_paid: Some(250),
        })
    }

    async fn lookup_invoice(
        &self,
        _request: LookupInvoiceRequest,
    ) -> Result<LookupInvoiceResponse, NIP47Error> {
        Err(nip47_error(ErrorCode::Internal, "no lookup"))
    }

    async fn list_transactions(
        &self,
        _request: ListTransactionsRequest,
    ) -> Result<Vec<LookupInvoiceResponse>, NIP47Error> {
        Err(nip47_error(ErrorCode::Internal, "no history"))
    }
}

fn nip47_error(code: ErrorCode, message: &str) -> NIP47Error {
    NIP47Error {
        code,
        message: message.to_owned(),
    }
}

fn context(event: &str) -> RequestContext {
    RequestContext {
        event_id: event.to_owned(),
        client_pubkey: "client-key".to_owned(),
        created_at: 1_001,
        received_at: 1_001,
    }
}

fn policy() -> ConnectionPolicy {
    ConnectionPolicy {
        id: "app-1".to_owned(),
        name: "Test app".to_owned(),
        client_pubkey: "client-key".to_owned(),
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

fn pay_policy(budget_msat: u64) -> ConnectionPolicy {
    let mut policy = policy();
    policy.allowed_methods = vec!["pay_invoice".to_owned()];
    policy.budget_msat = Some(budget_msat);
    policy.max_single_payment_msat = Some(20_000_000);
    policy
}

#[tokio::test]
async fn filters_get_info_methods_for_connection() {
    let dir = tempfile::tempdir().expect("temp dir");
    let handler = PolicyNwcHandler::new(dir.path().join("policy.json"), Arc::new(MockWallet))
        .expect("handler");
    let mut allowed = policy();
    allowed.allowed_methods = vec!["get_info".to_owned(), "get_balance".to_owned()];
    handler.add_connection(allowed).expect("add connection");

    let response = handler
        .get_info_with_context(&context("event-info"))
        .await
        .expect("get_info allowed");

    assert_eq!(response.methods, vec![Method::GetBalance, Method::GetInfo]);
}

#[tokio::test]
async fn caps_fee_overage_and_records_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let handler = PolicyNwcHandler::new(dir.path().join("policy.json"), Arc::new(MockWallet))
        .expect("handler");
    handler
        .add_connection(pay_policy(20_000_000))
        .expect("add connection");

    let response = handler
        .pay_invoice_with_context(PayInvoiceRequest::new(INVOICE), &context("event-fee"))
        .await
        .expect("payment succeeded despite fee overage");

    assert_eq!(response.fees_paid, Some(250));
    assert_eq!(handler.used_msat("app-1"), Some(INVOICE_MSAT + 100));
    assert_eq!(handler.fee_overage_msat("app-1"), Some(150));
}

#[tokio::test]
async fn accepts_amountless_invoice_using_invoice_amount() {
    let dir = tempfile::tempdir().expect("temp dir");
    let handler = PolicyNwcHandler::new(dir.path().join("policy.json"), Arc::new(MockWallet))
        .expect("handler");
    handler
        .add_connection(pay_policy(20_000_000))
        .expect("add connection");

    let request = PayInvoiceRequest::new(INVOICE);
    assert_eq!(request.amount, None);
    handler
        .pay_invoice_with_context(request, &context("event-amountless"))
        .await
        .expect("amountless invoice is allowed when the invoice carries an amount");

    assert_eq!(handler.used_msat("app-1"), Some(INVOICE_MSAT + 100));
}

#[tokio::test]
async fn rejects_amount_that_disagrees_with_invoice() {
    let dir = tempfile::tempdir().expect("temp dir");
    let handler = PolicyNwcHandler::new(dir.path().join("policy.json"), Arc::new(MockWallet))
        .expect("handler");
    handler
        .add_connection(pay_policy(20_000_000))
        .expect("add connection");

    let mut request = PayInvoiceRequest::new(INVOICE);
    request.amount = Some(1_000);

    let error = handler
        .pay_invoice_with_context(request, &context("event-mismatch"))
        .await
        .expect_err("client-supplied amount must match the invoice");

    assert_eq!(error.code, ErrorCode::Other);
    assert_eq!(handler.used_msat("app-1"), Some(0));
}

#[tokio::test]
async fn rejects_unknown_client() {
    let dir = tempfile::tempdir().expect("temp dir");
    let handler = PolicyNwcHandler::new(dir.path().join("policy.json"), Arc::new(MockWallet))
        .expect("handler");

    let error = handler
        .get_balance_with_context(&RequestContext {
            event_id: "event-1".to_owned(),
            client_pubkey: "unknown".to_owned(),
            created_at: 1_001,
            received_at: 1_001,
        })
        .await
        .expect_err("unknown client must be rejected");

    assert_eq!(error.code, ErrorCode::Unauthorized);
}

#[tokio::test]
async fn rejects_disallowed_method() {
    let dir = tempfile::tempdir().expect("temp dir");
    let handler = PolicyNwcHandler::new(dir.path().join("policy.json"), Arc::new(MockWallet))
        .expect("handler");
    handler.add_connection(policy()).expect("add connection");

    let error = handler
        .make_invoice_with_context(
            MakeInvoiceRequest {
                amount: 1_000,
                description: None,
                description_hash: None,
                expiry: None,
            },
            &context("event-1"),
        )
        .await
        .expect_err("disallowed method must be rejected");

    assert_eq!(error.code, ErrorCode::Restricted);
}

#[tokio::test]
async fn allows_known_client_and_commits_zero_amount() {
    let dir = tempfile::tempdir().expect("temp dir");
    let handler = PolicyNwcHandler::new(dir.path().join("policy.json"), Arc::new(MockWallet))
        .expect("handler");
    handler.add_connection(policy()).expect("add connection");

    let response = handler
        .get_balance_with_context(&context("event-1"))
        .await
        .expect("allowed method");

    assert_eq!(response.balance, 100_000);
    assert_eq!(handler.used_msat("app-1").expect("policy exists"), 0);
}

#[tokio::test]
async fn enforces_budget_on_pay_invoice() {
    let dir = tempfile::tempdir().expect("temp dir");
    let handler = PolicyNwcHandler::new(dir.path().join("policy.json"), Arc::new(MockWallet))
        .expect("handler");
    handler
        .add_connection(pay_policy(5_000))
        .expect("add connection");

    let request = PayInvoiceRequest::new(INVOICE);

    let error = handler
        .pay_invoice_with_context(request, &context("event-1"))
        .await
        .expect_err("invoice amount plus reserve exceeds budget");

    assert_eq!(error.code, ErrorCode::QuotaExceeded);
    assert_eq!(handler.used_msat("app-1").expect("policy exists"), 0);
}

#[tokio::test]
async fn charges_budget_when_payment_failure_is_ambiguous() {
    let dir = tempfile::tempdir().expect("temp dir");
    let handler = PolicyNwcHandler::new(dir.path().join("policy.json"), Arc::new(MockWallet))
        .expect("handler");
    handler
        .add_connection(pay_policy(20_000_000))
        .expect("add connection");

    let request = PayInvoiceRequest::new(FAIL_INVOICE);

    let error = handler
        .pay_invoice_with_context(request, &context("event-fail"))
        .await
        .expect_err("wallet failure must propagate");

    assert_eq!(error.code, ErrorCode::PaymentFailed);
    assert_eq!(
        handler.used_msat("app-1"),
        Some(INVOICE_MSAT + 100),
        "payment failure must not silently release a possibly spent budget"
    );
}
