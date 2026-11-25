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
use cdk_nwc_policy::ConnectionPolicy;
use cdk_nwc_policy_cli::{parse_budget_period, resolve_expiry, spawn_quote_claimer};
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
    Serve(ServeArgs),
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[command(flatten)]
    wallet: WalletArgs,

    /// Seconds between checks that mint ecash for paid `make_invoice` quotes
    /// (0 disables). Ignored with --upstream-nwc-uri-file.
    #[arg(long, default_value_t = 10)]
    claim_interval_secs: u64,

    /// Override the `network` reported by `get_info` (e.g. `signet`).
    #[arg(long)]
    network: Option<String>,
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

    /// Budget renewal period: never, daily, weekly, monthly or yearly.
    #[arg(long, default_value = "daily")]
    budget_period: String,

    /// Absolute expiry as a unix timestamp in seconds.
    #[arg(long, conflicts_with = "expires_in")]
    expires_at: Option<u64>,

    /// Relative expiry, e.g. `3600`, `30m`, `12h`, `7d` or `2w`.
    #[arg(long)]
    expires_in: Option<String>,
}

#[derive(Debug, Args)]
struct RevokeArgs {
    #[arg(long)]
    id: String,
}

#[derive(Debug)]
enum Backend {
    Wallet(WalletNwcHandler, Option<String>),
    Upstream(Box<ForwardingNwcHandler>, Option<String>),
}

#[async_trait]
impl NwcRequestHandler for Backend {
    async fn get_info(&self) -> Result<GetInfoResponse, NIP47Error> {
        let (mut info, network) = match self {
            Self::Wallet(handler, network) => (handler.get_info().await?, network),
            Self::Upstream(handler, network) => (handler.get_info().await?, network),
        };
        if let Some(network) = network {
            info.network = Some(network.clone());
        }
        Ok(info)
    }

    async fn get_balance(&self) -> Result<GetBalanceResponse, NIP47Error> {
        match self {
            Self::Wallet(handler, ..) => handler.get_balance().await,
            Self::Upstream(handler, _) => handler.get_balance().await,
        }
    }

    async fn make_invoice(
        &self,
        request: MakeInvoiceRequest,
    ) -> Result<MakeInvoiceResponse, NIP47Error> {
        match self {
            Self::Wallet(handler, ..) => handler.make_invoice(request).await,
            Self::Upstream(handler, _) => handler.make_invoice(request).await,
        }
    }

    async fn pay_invoice(
        &self,
        request: PayInvoiceRequest,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        match self {
            Self::Wallet(handler, ..) => handler.pay_invoice(request).await,
            Self::Upstream(handler, _) => handler.pay_invoice(request).await,
        }
    }

    async fn lookup_invoice(
        &self,
        request: LookupInvoiceRequest,
    ) -> Result<LookupInvoiceResponse, NIP47Error> {
        match self {
            Self::Wallet(handler, ..) => handler.lookup_invoice(request).await,
            Self::Upstream(handler, _) => handler.lookup_invoice(request).await,
        }
    }

    async fn list_transactions(
        &self,
        request: ListTransactionsRequest,
    ) -> Result<Vec<LookupInvoiceResponse>, NIP47Error> {
        match self {
            Self::Wallet(handler, ..) => handler.list_transactions(request).await,
            Self::Upstream(handler, _) => handler.list_transactions(request).await,
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

async fn build_manager(
    cli: &Cli,
    wallet: &WalletArgs,
    network: Option<String>,
) -> Result<(ConnectionManager<Backend>, Option<Arc<Wallet>>)> {
    let backend: (Backend, Option<Arc<Wallet>>) = if let Some(uri_path) = &cli.upstream_nwc_uri_file
    {
        let uri = fs::read_to_string(uri_path)
            .context("read upstream NWC URI file")?
            .trim()
            .to_owned();
        let forwarding = ForwardingNwcHandler::from_uri(&uri, Duration::from_secs(30))
            .context("parse upstream NWC URI")?;
        (Backend::Upstream(Box::new(forwarding), network), None)
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
        let wallet = Arc::new(wallet);
        (
            Backend::Wallet(WalletNwcHandler::new(wallet.clone(), None), network),
            Some(wallet),
        )
    };
    let (backend, wallet) = backend;

    let manager = ConnectionManager::new(&cli.manager, &cli.policy, Arc::new(backend))
        .context("open connection manager")?;
    Ok((manager, wallet))
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
        budget_period: parse_budget_period(&args.budget_period)?,
        max_single_payment_msat: Some(args.single_cap_msat),
        max_fee_reserve_msat: args.fee_reserve_msat,
        fee_overage_msat: 0,
        expires_at: resolve_expiry(args.expires_at, args.expires_in.as_deref(), now_secs())?,
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
            let policy = policy_from_args(args)?;
            let (mut manager, _) = build_manager(&cli, &args.wallet, None).await?;
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
        Command::Serve(args) => {
            let (mut manager, wallet) =
                build_manager(&cli, &args.wallet, args.network.clone()).await?;
            let started = manager.start_all().await?;
            println!("serving {started} connection(s)");
            let claimer = match wallet {
                Some(wallet) if args.claim_interval_secs > 0 => Some(spawn_quote_claimer(
                    wallet,
                    Duration::from_secs(args.claim_interval_secs),
                )),
                _ => None,
            };
            let signal = tokio::signal::ctrl_c().await;
            if let Some(claimer) = claimer {
                claimer.abort();
            }
            manager.stop_all().await;
            signal?;
            println!("stopped");
        }
    }

    Ok(())
}
