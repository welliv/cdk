//! Per-connection Cashu pockets for NWC links.
//!
//! These tests run against an in-process fake-wallet mint. They check the
//! properties a pocket must give a user who hands a link to an agent:
//!
//! - a link can only spend what is in its pocket, even with no policy ledger
//!   in front of it (the ledger is bypassed on purpose here);
//! - revoking sweeps the pocket back to the main wallet and invalidates any
//!   proofs that were copied out of the pocket;
//! - auto-refill is capped by a rolling window enforced on the main side;
//! - the fee reported to the client is the actual drop in pocket balance.

#![allow(missing_docs)]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use cdk::cdk_database::{self, WalletDatabase};
use cdk::nuts::nut00::ProofsMethods;
use cdk::nuts::CurrencyUnit;
use cdk::wallet::{ReceiveOptions, Wallet, WalletBuilder};
use cdk::{Amount, Mint};
use cdk_integration_tests::init_pure_tests::{
    create_and_start_in_memory_test_mint, create_and_start_in_memory_test_mint_with_fee,
    fund_wallet, setup_tracing, DirectMintConnection,
};
use cdk_nostr::nwc::nip47::{ErrorCode, PayInvoiceRequest};
use cdk_nostr::nwc::NwcRequestHandler;
use cdk_nwc_pocket::{
    derive_pocket_seed, sweep_excess, transfer_to_pocket, AutoRefill, OutOfFundsReason,
    PocketConfig, PocketEvent, PocketNwcHandler, PocketRegistry, RefillPolicy, Refiller,
};

const SAT: u64 = 1_000;

async fn wallet_on(mint: &Arc<Mint>, seed: [u8; 64]) -> Arc<Wallet> {
    let mint_url = mint
        .mint_info()
        .await
        .expect("mint info")
        .urls
        .and_then(|urls| urls.first().cloned())
        .expect("mint url");
    let localstore: Arc<dyn WalletDatabase<cdk_database::Error> + Send + Sync> = Arc::new(
        cdk_sqlite::wallet::memory::empty()
            .await
            .expect("wallet database"),
    );
    Arc::new(
        WalletBuilder::new()
            .mint_url(mint_url.parse().expect("mint url"))
            .unit(CurrencyUnit::Sat)
            .localstore(localstore)
            .seed(seed)
            .client(DirectMintConnection::new(Arc::clone(mint)))
            .build()
            .expect("wallet"),
    )
}

fn random_seed() -> [u8; 64] {
    bip39::Mnemonic::generate(12)
        .expect("mnemonic")
        .to_seed_normalized("")
}

struct Setup {
    mint: Arc<Mint>,
    main: Arc<Wallet>,
    pocket: Arc<Wallet>,
    _dir: tempfile::TempDir,
    registry: PocketRegistry,
}

async fn setup(mint: Arc<Mint>, main_sats: u64, pocket_sats: u64, refill: RefillPolicy) -> Setup {
    let main_seed = random_seed();
    let main = wallet_on(&mint, main_seed).await;
    fund_wallet(&main, main_sats, None)
        .await
        .expect("fund main");
    let pocket = wallet_on(&mint, derive_pocket_seed(&main_seed, 0)).await;
    if pocket_sats > 0 {
        transfer_to_pocket(&main, &pocket, Amount::from(pocket_sats), None)
            .await
            .expect("fund pocket");
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = PocketRegistry::new(dir.path().join("pockets.json")).expect("registry");
    registry
        .add(PocketConfig {
            connection_id: "agent".to_owned(),
            index: 0,
            ceiling_msat: 1_000 * SAT,
            refill,
            swept_at: None,
        })
        .expect("add pocket");
    Setup {
        mint,
        main,
        pocket,
        _dir: dir,
        registry,
    }
}

async fn sats(wallet: &Wallet) -> u64 {
    u64::from(wallet.total_balance().await.expect("balance"))
}

fn invoice(sats: u64) -> PayInvoiceRequest {
    PayInvoiceRequest {
        id: None,
        invoice: cdk_fake_wallet::create_fake_invoice(sats * SAT, "agent".to_owned()).to_string(),
        amount: None,
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

/// A rogue agent with the link but no policy ledger in front of it still
/// cannot reach a single sat of the main wallet.
#[tokio::test]
async fn pocket_link_cannot_spend_beyond_pocket_without_ledger() {
    setup_tracing();
    let mint = create_and_start_in_memory_test_mint().await.expect("mint");
    let s = setup(mint, 1_000, 100, RefillPolicy::default()).await;
    let main_after_funding = sats(&s.main).await;
    let pocket_start = sats(&s.pocket).await;
    assert!(pocket_start > 0 && pocket_start <= 100);

    let handler =
        PocketNwcHandler::new("agent".to_owned(), Arc::clone(&s.pocket), None).expect("handler");

    // An invoice the main wallet could pay, but the pocket cannot.
    let big = handler
        .pay_invoice(invoice(500))
        .await
        .expect_err("must fail");
    assert_eq!(big.code, ErrorCode::InsufficientBalance);

    // Drain the pocket with small payments until it is cut off.
    let mut paid = 0;
    let cut_off = loop {
        match handler.pay_invoice(invoice(20)).await {
            Ok(_) => paid += 20,
            Err(error) => break error,
        }
        assert!(paid <= 100, "paid more than the pocket held");
    };
    assert_eq!(cut_off.code, ErrorCode::InsufficientBalance);
    assert!(paid > 0);
    assert!(handler.spent_msat() <= pocket_start * SAT);

    let balance = handler.get_balance().await.expect("balance");
    assert_eq!(balance.balance, sats(&s.pocket).await * SAT);
    assert_eq!(sats(&s.main).await, main_after_funding, "main untouched");
}

/// Revoking sweeps the pocket home; proofs copied out before revoke are dead.
#[tokio::test]
async fn revoke_sweeps_pocket_and_kills_copied_proofs() {
    setup_tracing();
    let mint = create_and_start_in_memory_test_mint().await.expect("mint");
    let s = setup(mint, 1_000, 200, RefillPolicy::default()).await;
    let main_before = sats(&s.main).await;
    let pocket_before = sats(&s.pocket).await;

    // The attacker copies the pocket's proofs (e.g. from its database).
    let stolen = s.pocket.get_unspent_proofs().await.expect("proofs");
    assert_eq!(
        u64::from(stolen.total_amount().expect("sum")),
        pocket_before
    );

    let handler =
        PocketNwcHandler::new("agent".to_owned(), Arc::clone(&s.pocket), None).expect("handler");
    let report = handler.sweep_to(&s.main).await.expect("sweep");
    assert!(report.is_complete());
    s.registry.mark_swept("agent", now()).expect("mark swept");

    assert_eq!(sats(&s.pocket).await, 0);
    let main_after = sats(&s.main).await;
    assert_eq!(main_after - main_before, report.swept_msat / SAT);
    assert!(main_after - main_before <= pocket_before);
    assert!(
        main_after - main_before + 2 >= pocket_before,
        "sweep lost too much"
    );

    let thief = wallet_on(&s.mint, random_seed()).await;
    thief
        .receive_proofs(stolen, ReceiveOptions::default(), None, None)
        .await
        .expect_err("copied proofs must be spent after sweep");
    assert_eq!(sats(&thief).await, 0);

    // A swept pocket is never refilled again.
    let refiller = Refiller::new(Arc::clone(&s.main), s.registry.clone());
    assert!(refiller
        .refill("agent", &s.pocket, 50 * SAT, now())
        .await
        .is_err());
}

/// Auto-refill tops the pocket up, but never beyond the rolling-window cap.
#[tokio::test]
async fn auto_refill_is_capped_by_rolling_window() {
    setup_tracing();
    let mint = create_and_start_in_memory_test_mint().await.expect("mint");
    let policy = RefillPolicy {
        cap_msat: 100 * SAT,
        window_secs: 3_600,
        auto_refill: Some(AutoRefill {
            threshold_msat: 10 * SAT,
            target_msat: 40 * SAT,
        }),
    };
    let s = setup(mint, 1_000, 0, policy).await;
    let main_start = sats(&s.main).await;
    let refiller = Arc::new(Refiller::new(Arc::clone(&s.main), s.registry.clone()));
    let mut events = refiller.subscribe();
    let handler = PocketNwcHandler::new(
        "agent".to_owned(),
        Arc::clone(&s.pocket),
        Some(Arc::clone(&refiller)),
    )
    .expect("handler");

    let mut paid = 0;
    let cut_off = loop {
        match handler.pay_invoice(invoice(15)).await {
            Ok(_) => paid += 15,
            Err(error) => break error,
        }
        assert!(paid <= 100, "refills exceeded the cap");
    };
    assert_eq!(cut_off.code, ErrorCode::QuotaExceeded);
    assert!(paid >= 60, "refills should have funded several payments");

    let main_loss = main_start - sats(&s.main).await;
    assert!(
        main_loss <= 100,
        "main lost {main_loss} sats with a 100 sat cap"
    );
    let t = now();
    assert!(s.registry.remaining_msat("agent", t).expect("remaining") < 20 * SAT);

    let mut saw_event = false;
    while let Ok(event) = events.try_recv() {
        let PocketEvent::OutOfFunds { reason, .. } = event;
        if reason == OutOfFundsReason::RefillCapReached {
            saw_event = true;
        }
    }
    assert!(saw_event, "out-of-funds event must fire");

    // Once the window rolls past the grants, refills are allowed again.
    let later = t + 3_601;
    assert_eq!(
        s.registry
            .remaining_msat("agent", later)
            .expect("remaining"),
        100 * SAT
    );
    let granted = refiller
        .refill("agent", &s.pocket, 20 * SAT, later)
        .await
        .expect("refill after window");
    assert!(granted > 0);
}

/// A refill larger than the main balance sends what main holds instead of
/// failing, so a partial grant at the end of the cap is not wasted.
#[tokio::test]
async fn refill_sends_what_main_holds() {
    setup_tracing();
    let mint = create_and_start_in_memory_test_mint_with_fee(100)
        .await
        .expect("mint");
    let policy = RefillPolicy {
        cap_msat: 100 * SAT,
        window_secs: 3_600,
        auto_refill: None,
    };
    let s = setup(mint, 40, 0, policy).await;
    let main_start = sats(&s.main).await;
    let refiller = Refiller::new(Arc::clone(&s.main), s.registry.clone());

    let added = refiller
        .refill("agent", &s.pocket, 60 * SAT, now())
        .await
        .expect("partial refill from a short main wallet");
    let main_left = sats(&s.main).await;
    assert!(added > 0, "something must land");
    assert_eq!(sats(&s.pocket).await * SAT, added);
    assert!(
        main_left < 5,
        "main should be nearly empty, has {main_left}"
    );
    // The cap is charged the real cost only.
    let used = 100 * SAT
        - s.registry
            .remaining_msat("agent", now())
            .expect("remaining");
    assert_eq!(used, (main_start - main_left) * SAT);
}

/// `fees_paid` is the real drop in pocket balance minus the invoice amount.
#[tokio::test]
async fn fee_accounting_is_pocket_balance_drop() {
    setup_tracing();
    let mint = create_and_start_in_memory_test_mint_with_fee(1_000)
        .await
        .expect("mint");
    let s = setup(mint, 2_000, 300, RefillPolicy::default()).await;
    let handler =
        PocketNwcHandler::new("agent".to_owned(), Arc::clone(&s.pocket), None).expect("handler");

    let before = sats(&s.pocket).await;
    let response = handler.pay_invoice(invoice(50)).await.expect("pay");
    let after = sats(&s.pocket).await;
    let drop_msat = (before - after) * SAT;

    assert_eq!(handler.spent_msat(), drop_msat);
    assert_eq!(response.fees_paid, Some(drop_msat - 50 * SAT));
    assert!(
        response.fees_paid.unwrap_or(0) > 0,
        "a fee-charging mint must show a fee"
    );
}

/// Funds received above the pocket ceiling move to the main wallet.
#[tokio::test]
async fn excess_above_ceiling_moves_to_main() {
    setup_tracing();
    let mint = create_and_start_in_memory_test_mint().await.expect("mint");
    let s = setup(mint, 100, 0, RefillPolicy::default()).await;
    let main_before = sats(&s.main).await;

    // Someone pays a make_invoice on the link.
    fund_wallet(&s.pocket, 300, None).await.expect("receive");
    let moved = sweep_excess(&s.pocket, &s.main, 100 * SAT)
        .await
        .expect("sweep excess");

    assert!(moved >= 200 * SAT - 2 * SAT, "moved {moved}");
    assert!(sats(&s.pocket).await <= 100);
    assert!(sats(&s.main).await > main_before);
}

/// Manual top-ups do not consume the refill cap but respect the ceiling.
#[tokio::test]
async fn refill_cap_survives_manual_top_up() {
    setup_tracing();
    let mint = create_and_start_in_memory_test_mint().await.expect("mint");
    let policy = RefillPolicy {
        cap_msat: 50 * SAT,
        window_secs: 3_600,
        auto_refill: None,
    };
    let s = setup(mint, 1_000, 0, policy).await;
    let refiller = Refiller::new(Arc::clone(&s.main), s.registry.clone());
    let t = now();

    // Manual top-ups are the user's choice and do not use the cap.
    refiller
        .top_up("agent", &s.pocket, 80 * SAT)
        .await
        .expect("top up");
    assert_eq!(
        s.registry.remaining_msat("agent", t).expect("rem"),
        50 * SAT
    );

    // The ceiling still applies to manual top-ups.
    assert!(refiller
        .top_up("agent", &s.pocket, 2_000 * SAT)
        .await
        .is_err());
}

/// The `serve` maintenance pass sweeps a revoked or expired pocket home and
/// rebalances an open one above its ceiling.
#[tokio::test]
async fn maintenance_sweeps_closed_pockets_and_rebalances_open_ones() {
    use std::collections::HashMap;

    use cdk_nwc_policy::{BudgetPeriod, ConnectionPolicy};
    use cdk_nwc_policy_cli::pocket::maintain_pockets;

    setup_tracing();
    let mint = create_and_start_in_memory_test_mint().await.expect("mint");
    let s = setup(Arc::clone(&mint), 500, 100, RefillPolicy::default()).await;
    let open_pocket = wallet_on(&mint, random_seed()).await;
    s.registry
        .add(PocketConfig {
            connection_id: "open".to_owned(),
            index: 1,
            ceiling_msat: 50 * SAT,
            refill: RefillPolicy::default(),
            swept_at: None,
        })
        .expect("add");
    fund_wallet(&open_pocket, 120, None).await.expect("fund");

    let policy = |id: &str, expires_at: Option<u64>| ConnectionPolicy {
        id: id.to_owned(),
        name: id.to_owned(),
        client_pubkey: id.to_owned(),
        allowed_methods: vec!["pay_invoice".to_owned()],
        budget_msat: None,
        budget_period: BudgetPeriod::Never,
        max_single_payment_msat: None,
        max_fee_reserve_msat: 0,
        fee_overage_msat: 0,
        expires_at,
        revoked: false,
        used_msat: 0,
        period_start_secs: 0,
    };
    let t = now();
    let policies = vec![policy("agent", Some(t - 1)), policy("open", None)];
    let mut pockets = HashMap::new();
    pockets.insert(
        "agent".to_owned(),
        Arc::new(
            PocketNwcHandler::new("agent".to_owned(), Arc::clone(&s.pocket), None)
                .expect("handler"),
        ),
    );
    pockets.insert(
        "open".to_owned(),
        Arc::new(
            PocketNwcHandler::new("open".to_owned(), Arc::clone(&open_pocket), None)
                .expect("handler"),
        ),
    );

    let main_before = sats(&s.main).await;
    maintain_pockets(&s.main, &s.registry, &policies, &pockets, t)
        .await
        .expect("maintenance");

    assert_eq!(sats(&s.pocket).await, 0, "expired pocket swept");
    assert!(s.registry.get("agent").expect("get").swept_at.is_some());
    assert!(
        sats(&open_pocket).await <= 50,
        "open pocket trimmed to ceiling"
    );
    assert!(s.registry.get("open").expect("get").swept_at.is_none());
    assert!(sats(&s.main).await > main_before + 150);
}

/// The policy layer in front of a pocket still blocks replays and revoked
/// links (pocket + ledger, not pocket instead of ledger).
#[tokio::test]
async fn policy_over_pocket_keeps_replay_protection_and_revoke() {
    use cdk_nostr::nwc::handler::RequestContext;
    use cdk_nwc_policy::{BudgetPeriod, ConnectionPolicy, PolicyNwcHandler};

    setup_tracing();
    let mint = create_and_start_in_memory_test_mint().await.expect("mint");
    let s = setup(mint, 500, 100, RefillPolicy::default()).await;
    let pocket = Arc::new(
        PocketNwcHandler::new("agent".to_owned(), Arc::clone(&s.pocket), None).expect("handler"),
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let policy = PolicyNwcHandler::new(dir.path().join("policy.json"), pocket).expect("policy");
    let t = now();
    policy
        .add_connection(ConnectionPolicy {
            id: "agent".to_owned(),
            name: "agent".to_owned(),
            client_pubkey: "client".to_owned(),
            allowed_methods: vec!["pay_invoice".to_owned()],
            budget_msat: None,
            budget_period: BudgetPeriod::Never,
            max_single_payment_msat: None,
            max_fee_reserve_msat: 0,
            fee_overage_msat: 0,
            expires_at: None,
            revoked: false,
            used_msat: 0,
            period_start_secs: t,
        })
        .expect("add");
    let ctx = |event: &str| RequestContext {
        event_id: event.to_owned(),
        client_pubkey: "client".to_owned(),
        created_at: t,
        received_at: t,
    };

    let request = invoice(10);
    policy
        .pay_invoice_with_context(request.clone(), &ctx("e1"))
        .await
        .expect("first pay");
    let after_first = sats(&s.pocket).await;
    policy
        .pay_invoice_with_context(request, &ctx("e1"))
        .await
        .expect_err("replayed event must be rejected");
    assert_eq!(sats(&s.pocket).await, after_first);

    policy.revoke("agent").expect("revoke");
    let error = policy
        .pay_invoice_with_context(invoice(5), &ctx("e2"))
        .await
        .expect_err("revoked");
    assert_eq!(error.code, ErrorCode::Unauthorized);
    assert_eq!(sats(&s.pocket).await, after_first);
}
