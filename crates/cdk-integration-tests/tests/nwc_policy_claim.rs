//! Regression: invoices created through NWC `make_invoice` must turn into
//! spendable ecash once paid.
//!
//! Found live on signet: a 1000 sat invoice from `make_invoice` was paid
//! (mint quote state PAID) but `get_balance` stayed at 0 and `lookup_invoice`
//! reported `pending` forever, because nothing minted paid quotes.

#![allow(missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use cdk::cdk_database::{self, WalletDatabase};
use cdk::nuts::{CurrencyUnit, MintQuoteState};
use cdk::wallet::{Wallet, WalletBuilder, WalletNwcHandler};
use cdk_integration_tests::init_pure_tests::{
    create_and_start_in_memory_test_mint, setup_tracing, DirectMintConnection,
};
use cdk_nostr::nwc::nip47::MakeInvoiceRequest;
use cdk_nostr::nwc::NwcRequestHandler;
use cdk_nwc_policy_cli::{claim_paid_quotes, spawn_quote_claimer};

async fn memory_wallet() -> Arc<Wallet> {
    let mint = create_and_start_in_memory_test_mint().await.expect("mint");
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
    let seed = bip39::Mnemonic::generate(12)
        .expect("mnemonic")
        .to_seed_normalized("");
    Arc::new(
        WalletBuilder::new()
            .mint_url(mint_url.parse().expect("mint url"))
            .unit(CurrencyUnit::Sat)
            .localstore(localstore)
            .seed(seed)
            .client(DirectMintConnection::new(mint))
            .build()
            .expect("wallet"),
    )
}

async fn wait_until_paid(wallet: &Wallet) {
    for _ in 0..50 {
        let quotes = wallet.check_all_mint_quotes().await.expect("check quotes");
        if quotes
            .iter()
            .all(|quote| quote.state == MintQuoteState::Paid)
            && !quotes.is_empty()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("fake backend never paid the mint quote");
}

#[tokio::test]
async fn paid_make_invoice_is_claimed_into_balance() {
    setup_tracing();
    let wallet = memory_wallet().await;
    let handler = WalletNwcHandler::new(wallet.clone(), None);

    handler
        .make_invoice(MakeInvoiceRequest {
            amount: 21_000,
            description: None,
            description_hash: None,
            expiry: None,
        })
        .await
        .expect("make_invoice");
    wait_until_paid(&wallet).await;

    // Paid, but without a claimer nothing is minted.
    let balance = handler.get_balance().await.expect("balance");
    assert_eq!(
        balance.balance, 0,
        "paid quote is not spendable until claimed"
    );

    let minted = claim_paid_quotes(&wallet).await.expect("claim");
    assert_eq!(minted, 21);
    let balance = handler.get_balance().await.expect("balance");
    assert_eq!(balance.balance, 21_000);

    // Claiming again is a no-op.
    assert_eq!(claim_paid_quotes(&wallet).await.expect("claim"), 0);
}

#[tokio::test]
async fn background_claimer_mints_paid_invoices() {
    setup_tracing();
    let wallet = memory_wallet().await;
    let handler = WalletNwcHandler::new(wallet.clone(), None);
    let claimer = spawn_quote_claimer(wallet.clone(), Duration::from_millis(100));

    handler
        .make_invoice(MakeInvoiceRequest {
            amount: 5_000,
            description: None,
            description_hash: None,
            expiry: None,
        })
        .await
        .expect("make_invoice");

    let mut balance = 0;
    for _ in 0..100 {
        balance = handler.get_balance().await.expect("balance").balance;
        if balance == 5_000 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    claimer.abort();
    assert_eq!(balance, 5_000, "claimer must mint the paid invoice");
}
