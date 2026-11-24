#![allow(missing_docs)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use cdk::nuts::CurrencyUnit;
use cdk::wallet::{Wallet, WalletNwcHandler};
use cdk_nostr::nwc::nip47::{
    GetBalanceResponse, GetInfoResponse, ListTransactionsRequest, LookupInvoiceRequest,
    LookupInvoiceResponse, MakeInvoiceRequest, MakeInvoiceResponse, NIP47Error, PayInvoiceRequest,
    PayInvoiceResponse,
};
use cdk_nostr::nwc::{NwcRequestHandler, SUPPORTED_METHODS};
use cdk_nwc_forward::ForwardingNwcHandler;
use cdk_nwc_policy::manager::ConnectionManager;
use cdk_nwc_policy::{BudgetPeriod, ConnectionPolicy};
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "nwc-policy-cli")]
struct Cli {
    #[arg(long, default_value = "nwc-manager.json")]
    manager: PathBuf,

    #[arg(long, default_value = "nwc-policy.json")]
    policy: PathBuf,

    #[arg(long)]
    upstream_nwc_uri_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Create(CreateArgs),
    List,
    Revoke(RevokeArgs),
    Serve(WalletArgs),
}

#[derive(Debug, Args)]
struct WalletArgs {
    #[arg(long)]
    mint_url: Option<String>,

    #[arg(long)]
    wallet_db: Option<PathBuf>,

    #[arg(long)]
    seed_hex: Option<String>,
}

#[derive(Debug, Args)]
struct CreateArgs {
    #[command(flatten)]
    wallet: WalletArgs,

    #[arg(long)]
    name: String,

    #[arg(long)]
    relay: Vec<String>,

    #[arg(long, default_value_t = 10_000)]
    budget_msat: u64,

    #[arg(long, default_value = "get_info,get_balance,pay_invoice")]
    methods: String,

    #[arg(long, default_value_t = 8_000)]
    single_cap_msat: u64,

    #[arg(long, default_value_t = 100)]
    fee_reserve_msat: u64,
}

#[derive(Debug, Args)]
struct RevokeArgs {
    #[arg(long)]
    id: String,
}

#[derive(Debug)]
enum Backend {
    Wallet(WalletNwcHandler),
    Upstream(Box<ForwardingNwcHandler>),
}

#[async_trait]
impl NwcRequestHandler for Backend {
    async fn get_info(&self) -> Result<GetInfoResponse, NIP47Error> {
        match self {
            Self::Wallet(handler) => handler.get_info().await,
            Self::Upstream(handler) => handler.get_info().await,
        }
    }

    async fn get_balance(&self) -> Result<GetBalanceResponse, NIP47Error> {
        match self {
            Self::Wallet(handler) => handler.get_balance().await,
            Self::Upstream(handler) => handler.get_balance().await,
        }
    }

    async fn make_invoice(
        &self,
        request: MakeInvoiceRequest,
    ) -> Result<MakeInvoiceResponse, NIP47Error> {
        match self {
            Self::Wallet(handler) => handler.make_invoice(request).await,
            Self::Upstream(handler) => handler.make_invoice(request).await,
        }
    }

    async fn pay_invoice(
        &self,
        request: PayInvoiceRequest,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        match self {
            Self::Wallet(handler) => handler.pay_invoice(request).await,
            Self::Upstream(handler) => handler.pay_invoice(request).await,
        }
    }

    async fn lookup_invoice(
        &self,
        request: LookupInvoiceRequest,
    ) -> Result<LookupInvoiceResponse, NIP47Error> {
        match self {
            Self::Wallet(handler) => handler.lookup_invoice(request).await,
            Self::Upstream(handler) => handler.lookup_invoice(request).await,
        }
    }

    async fn list_transactions(
        &self,
        request: ListTransactionsRequest,
    ) -> Result<Vec<LookupInvoiceResponse>, NIP47Error> {
        match self {
            Self::Wallet(handler) => handler.list_transactions(request).await,
            Self::Upstream(handler) => handler.list_transactions(request).await,
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn resolve_seed(wallet: &WalletArgs) -> Result<[u8; 64]> {
    let seed_hex = match &wallet.seed_hex {
        Some(seed) => seed.clone(),
        None => std::env::var("CDK_NWC_SEED_HEX")
            .context("--seed-hex or CDK_NWC_SEED_HEX is required")?,
    };
    let seed_bytes = hex::decode(seed_hex).context("seed must be valid hex")?;
    if seed_bytes.len() != 64 {
        bail!("seed must decode to 64 bytes");
    }
    let mut seed = [0u8; 64];
    seed.copy_from_slice(&seed_bytes);
    Ok(seed)
}

async fn build_manager(cli: &Cli, wallet: &WalletArgs) -> Result<ConnectionManager<Backend>> {
    let backend = if let Some(uri_path) = &cli.upstream_nwc_uri_file {
        let uri = fs::read_to_string(uri_path)
            .context("read upstream NWC URI file")?
            .trim()
            .to_owned();
        let forwarding = ForwardingNwcHandler::from_uri(&uri, Duration::from_secs(30))
            .context("parse upstream NWC URI")?;
        Backend::Upstream(Box::new(forwarding))
    } else {
        let mint_url = wallet
            .mint_url
            .as_ref()
            .context("--mint-url is required without --upstream-nwc-uri-file")?;
        let wallet_db = wallet
            .wallet_db
            .as_ref()
            .context("--wallet-db is required without --upstream-nwc-uri-file")?;
        let seed = resolve_seed(wallet)?;
        let database = cdk_sqlite::WalletSqliteDatabase::new(wallet_db)
            .await
            .context("open wallet database")?;
        let wallet = Wallet::new(mint_url, CurrencyUnit::Sat, Arc::new(database), seed, None)
            .context("build CDK wallet")?;
        Backend::Wallet(WalletNwcHandler::new(Arc::new(wallet), None))
    };

    ConnectionManager::new(&cli.manager, &cli.policy, Arc::new(backend))
        .context("open connection manager")
}

fn policy_from_args(args: &CreateArgs) -> Result<ConnectionPolicy> {
    let allowed_methods: Vec<String> = args
        .methods
        .split(',')
        .map(|method| method.trim().to_owned())
        .filter(|method| !method.is_empty())
        .collect();
    if allowed_methods.is_empty() {
        bail!("at least one method is required");
    }
    for method in &allowed_methods {
        if !SUPPORTED_METHODS.contains(&method.as_str()) {
            bail!("unsupported NWC method: {method}");
        }
    }
    if args.budget_msat == 0 || args.single_cap_msat == 0 {
        bail!("budget and single cap must be greater than zero");
    }
    if args.single_cap_msat > args.budget_msat {
        bail!("single cap cannot exceed budget");
    }
    if args.fee_reserve_msat > args.budget_msat {
        bail!("fee reserve cannot exceed budget");
    }
    Ok(ConnectionPolicy {
        id: String::new(),
        name: args.name.clone(),
        client_pubkey: String::new(),
        allowed_methods,
        budget_msat: Some(args.budget_msat),
        budget_period: BudgetPeriod::Daily,
        max_single_payment_msat: Some(args.single_cap_msat),
        max_fee_reserve_msat: args.fee_reserve_msat,
        fee_overage_msat: 0,
        expires_at: None,
        revoked: false,
        used_msat: 0,
        period_start_secs: now_secs(),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.manager == cli.policy {
        bail!("--manager and --policy must be different paths");
    }

    match &cli.command {
        Command::Create(args) => {
            let mut manager = build_manager(&cli, &args.wallet).await?;
            let policy = policy_from_args(args)?;
            let uri = manager.create_connection(args.name.clone(), args.relay.clone(), policy)?;
            let views = manager.list_connections_view();
            let id = views
                .last()
                .map(|view| view.id.clone())
                .context("created connection missing from manager")?;
            println!("connection_id={id}");
            println!("{uri}");
        }
        Command::List => {
            let records = ConnectionManager::<Backend>::list_records(&cli.manager)?;
            println!("{}", serde_json::to_string_pretty(&records)?);
        }
        Command::Revoke(args) => {
            ConnectionManager::<Backend>::revoke_policy(&cli.policy, &args.id)?;
            println!("revoked {}", args.id);
        }
        Command::Serve(wallet) => {
            let mut manager = build_manager(&cli, wallet).await?;
            let started = manager.start_all().await?;
            println!("serving {started} connection(s)");
            let signal = tokio::signal::ctrl_c().await;
            manager.stop_all().await;
            signal?;
            println!("stopped");
        }
    }

    Ok(())
}
