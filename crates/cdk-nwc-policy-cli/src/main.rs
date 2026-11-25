#![allow(missing_docs)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use cdk::wallet::{Wallet, WalletNwcHandler};
use cdk::Amount;
use cdk_nostr::nwc::nip47::{
    GetBalanceResponse, GetInfoResponse, ListTransactionsRequest, LookupInvoiceRequest,
    LookupInvoiceResponse, MakeInvoiceRequest, MakeInvoiceResponse, NIP47Error, PayInvoiceRequest,
    PayInvoiceResponse,
};
use cdk_nostr::nwc::{NwcRequestHandler, SUPPORTED_METHODS};
use cdk_nwc_forward::ForwardingNwcHandler;
use cdk_nwc_pocket::{
    sweep_pocket, transfer_to_pocket, AutoRefill, PocketConfig, PocketNwcHandler, PocketRegistry,
    RefillPolicy, Refiller, MSAT_PER_SAT,
};
use cdk_nwc_policy::manager::ConnectionManager;
use cdk_nwc_policy::{BudgetPeriod, ConnectionPolicy, PolicyStore};
use cdk_nwc_policy_cli::pocket::{
    maintain_pockets, open_pocket, open_wallet, registry_path, DEFAULT_MAINTENANCE_INTERVAL,
};
use cdk_nwc_policy_cli::{
    parse_budget_period, parse_duration_secs, resolve_expiry, spawn_quote_claimer,
};
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

    /// Pocket registry (defaults to `<policy>.pockets.json`).
    #[arg(long)]
    pockets: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Create(Box<CreateArgs>),
    List,
    Revoke(RevokeArgs),
    Serve(ServeArgs),
    /// Add sats to a connection's pocket by hand (not counted against the
    /// refill cap; the ceiling still applies).
    Topup(TopupArgs),
    /// Move everything in a pocket back to the main wallet.
    Sweep(SweepArgs),
    /// List pockets, with balances when wallet arguments are given.
    Pockets(PocketsArgs),
    /// Print a mint invoice for the main wallet and wait until it is paid.
    Fund(FundArgs),
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

    /// Give the link its own Cashu pocket. The link can then spend at most
    /// what the pocket holds; --budget-msat and --single-cap-msat are
    /// ignored. Needs --mint-url, --wallet-db and a seed.
    #[arg(long)]
    pocket: bool,

    /// Sats moved from the main wallet into the new pocket.
    #[arg(long, default_value_t = 0, requires = "pocket")]
    pocket_sats: u64,

    /// Most the pocket may hold; received funds above it go to main.
    /// Defaults to the larger of --pocket-sats and --auto-refill-to-sats.
    #[arg(long, requires = "pocket")]
    ceiling_sats: Option<u64>,

    /// Most the main wallet refills this pocket per --refill-window.
    #[arg(long, default_value_t = 0, requires = "pocket")]
    refill_cap_sats: u64,

    /// Rolling refill window, e.g. `1d`, `12h`, `7d`.
    #[arg(long, default_value = "1d", requires = "pocket")]
    refill_window: String,

    /// Auto-refill when the pocket drops to or below this many sats.
    #[arg(long, requires_all = ["pocket", "auto_refill_to_sats"])]
    auto_refill_below_sats: Option<u64>,

    /// Auto-refill up to this many sats.
    #[arg(long, requires_all = ["pocket", "auto_refill_below_sats"])]
    auto_refill_to_sats: Option<u64>,
}

#[derive(Debug, Args)]
struct RevokeArgs {
    #[command(flatten)]
    wallet: WalletArgs,

    #[arg(long)]
    id: String,
}

#[derive(Debug, Args)]
struct TopupArgs {
    #[command(flatten)]
    wallet: WalletArgs,

    #[arg(long)]
    id: String,

    #[arg(long)]
    sats: u64,
}

#[derive(Debug, Args)]
struct SweepArgs {
    #[command(flatten)]
    wallet: WalletArgs,

    #[arg(long)]
    id: String,
}

#[derive(Debug, Args)]
struct FundArgs {
    #[command(flatten)]
    wallet: WalletArgs,

    #[arg(long)]
    sats: u64,

    /// Seconds to wait for payment.
    #[arg(long, default_value_t = 600)]
    wait_secs: u64,
}

#[derive(Debug, Args)]
struct PocketsArgs {
    #[command(flatten)]
    wallet: WalletArgs,
}

#[derive(Debug)]
enum Backend {
    Wallet(WalletNwcHandler, Option<String>),
    Upstream(Box<ForwardingNwcHandler>, Option<String>),
    Pocket(Arc<PocketNwcHandler>, Option<String>),
}

#[async_trait]
impl NwcRequestHandler for Backend {
    async fn get_info(&self) -> Result<GetInfoResponse, NIP47Error> {
        let (mut info, network) = match self {
            Self::Wallet(handler, network) => (handler.get_info().await?, network),
            Self::Upstream(handler, network) => (handler.get_info().await?, network),
            Self::Pocket(handler, network) => (handler.get_info().await?, network),
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
            Self::Pocket(handler, _) => handler.get_balance().await,
        }
    }

    async fn make_invoice(
        &self,
        request: MakeInvoiceRequest,
    ) -> Result<MakeInvoiceResponse, NIP47Error> {
        match self {
            Self::Wallet(handler, ..) => handler.make_invoice(request).await,
            Self::Upstream(handler, _) => handler.make_invoice(request).await,
            Self::Pocket(handler, _) => handler.make_invoice(request).await,
        }
    }

    async fn pay_invoice(
        &self,
        request: PayInvoiceRequest,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        match self {
            Self::Wallet(handler, ..) => handler.pay_invoice(request).await,
            Self::Upstream(handler, _) => handler.pay_invoice(request).await,
            Self::Pocket(handler, _) => handler.pay_invoice(request).await,
        }
    }

    async fn lookup_invoice(
        &self,
        request: LookupInvoiceRequest,
    ) -> Result<LookupInvoiceResponse, NIP47Error> {
        match self {
            Self::Wallet(handler, ..) => handler.lookup_invoice(request).await,
            Self::Upstream(handler, _) => handler.lookup_invoice(request).await,
            Self::Pocket(handler, _) => handler.lookup_invoice(request).await,
        }
    }

    async fn list_transactions(
        &self,
        request: ListTransactionsRequest,
    ) -> Result<Vec<LookupInvoiceResponse>, NIP47Error> {
        match self {
            Self::Wallet(handler, ..) => handler.list_transactions(request).await,
            Self::Upstream(handler, _) => handler.list_transactions(request).await,
            Self::Pocket(handler, _) => handler.list_transactions(request).await,
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
        let wallet = open_wallet(mint_url, wallet_db, seed).await?;
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
    let mut allowed_methods = Vec::new();
    for method in args
        .methods
        .split(',')
        .map(str::trim)
        .filter(|method| !method.is_empty())
    {
        if !allowed_methods.iter().any(|seen| seen == method) {
            allowed_methods.push(method.to_owned());
        }
    }
    if allowed_methods.is_empty() {
        bail!("at least one method is required");
    }
    for method in &allowed_methods {
        if !SUPPORTED_METHODS.contains(&method.as_str()) {
            bail!("unsupported NWC method: {method}");
        }
    }
    let expires_at = resolve_expiry(args.expires_at, args.expires_in.as_deref(), now_secs())?;
    if args.pocket {
        // The pocket balance is the spending bound; no shared-balance ledger.
        return Ok(ConnectionPolicy {
            id: String::new(),
            name: args.name.clone(),
            client_pubkey: String::new(),
            allowed_methods,
            budget_msat: None,
            budget_period: BudgetPeriod::Never,
            max_single_payment_msat: None,
            max_fee_reserve_msat: args.fee_reserve_msat,
            fee_overage_msat: 0,
            expires_at,
            revoked: false,
            used_msat: 0,
            period_start_secs: now_secs(),
        });
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
        expires_at,
        revoked: false,
        used_msat: 0,
        period_start_secs: now_secs(),
    })
}

/// Pocket limits from `create` flags.
fn pocket_limits(args: &CreateArgs) -> Result<(u64, RefillPolicy)> {
    let auto_refill = match (args.auto_refill_below_sats, args.auto_refill_to_sats) {
        (Some(below), Some(to)) => {
            if to <= below {
                bail!("--auto-refill-to-sats must be above --auto-refill-below-sats");
            }
            if args.refill_cap_sats == 0 {
                bail!("auto-refill needs --refill-cap-sats > 0");
            }
            Some(AutoRefill {
                threshold_msat: below * MSAT_PER_SAT,
                target_msat: to * MSAT_PER_SAT,
            })
        }
        _ => None,
    };
    let ceiling_sats = args.ceiling_sats.unwrap_or_else(|| {
        args.pocket_sats
            .max(args.auto_refill_to_sats.unwrap_or(0))
            .max(1)
    });
    if args.pocket_sats > ceiling_sats {
        bail!("--pocket-sats cannot exceed --ceiling-sats");
    }
    if args.auto_refill_to_sats.is_some_and(|to| to > ceiling_sats) {
        bail!("--auto-refill-to-sats cannot exceed --ceiling-sats");
    }
    let refill = RefillPolicy {
        cap_msat: args.refill_cap_sats * MSAT_PER_SAT,
        window_secs: parse_duration_secs(&args.refill_window)?.max(1),
        auto_refill,
    };
    Ok((ceiling_sats * MSAT_PER_SAT, refill))
}

/// Local wallet arguments, all present.
struct LocalWallet<'a> {
    mint_url: &'a str,
    wallet_db: &'a Path,
    seed: [u8; 64],
}

fn local_wallet(args: &WalletArgs) -> Result<Option<LocalWallet<'_>>> {
    match (&args.mint_url, &args.wallet_db) {
        (Some(mint_url), Some(wallet_db)) => Ok(Some(LocalWallet {
            mint_url,
            wallet_db,
            seed: resolve_seed(args)?,
        })),
        _ => Ok(None),
    }
}

fn require_local_wallet<'a>(cli: &Cli, args: &'a WalletArgs) -> Result<LocalWallet<'a>> {
    if cli.upstream_nwc_uri_file.is_some() {
        bail!("pockets need a local wallet, not --upstream-nwc-uri-file");
    }
    local_wallet(args)?.context("pockets need --mint-url, --wallet-db and a seed")
}

fn pockets_path(cli: &Cli) -> PathBuf {
    cli.pockets
        .clone()
        .unwrap_or_else(|| registry_path(&cli.policy))
}

async fn open_main(local: &LocalWallet<'_>) -> Result<Arc<Wallet>> {
    open_wallet(local.mint_url, local.wallet_db, local.seed).await
}

async fn open_pocket_for(local: &LocalWallet<'_>, index: u32) -> Result<Arc<Wallet>> {
    open_pocket(local.mint_url, local.wallet_db, &local.seed, index).await
}

/// Sweep a pocket home; mark it swept when nothing is left pending.
async fn sweep(
    local: &LocalWallet<'_>,
    registry: &PocketRegistry,
    id: &str,
    close: bool,
) -> Result<()> {
    let config = registry.get(id)?;
    let main = open_main(local).await?;
    let pocket = open_pocket_for(local, config.index).await?;
    let report = sweep_pocket(&pocket, &main).await?;
    if close && report.is_complete() {
        registry.mark_swept(id, now_secs())?;
    }
    println!(
        "swept {} sat from pocket {id} to main ({} sat pending{})",
        report.swept_msat / MSAT_PER_SAT,
        report.pending_msat / MSAT_PER_SAT,
        if report.is_complete() {
            ""
        } else {
            "; rerun later to sweep the rest"
        }
    );
    Ok(())
}

async fn create_pocket(
    cli: &Cli,
    args: &CreateArgs,
    manager: &mut ConnectionManager<Backend>,
    main: &Wallet,
) -> Result<()> {
    let local = require_local_wallet(cli, &args.wallet)?;
    let (ceiling_msat, refill) = pocket_limits(args)?;
    let registry = PocketRegistry::new(pockets_path(cli))?;
    let index = registry.next_index()?;
    let policy = policy_from_args(args)?;
    let uri = manager.create_connection(args.name.clone(), args.relay.clone(), policy)?;
    let id = manager
        .list_connections_view()
        .last()
        .map(|view| view.id.clone())
        .context("created connection missing from manager")?;
    registry.add(PocketConfig {
        connection_id: id.clone(),
        index,
        ceiling_msat,
        refill,
        swept_at: None,
    })?;
    let pocket = open_pocket_for(&local, index).await?;
    if args.pocket_sats > 0 {
        let (cost, received) =
            transfer_to_pocket(main, &pocket, Amount::from(args.pocket_sats), None).await?;
        eprintln!("pocket #{index} funded with {received} sat (cost {cost} sat)");
    }
    println!("connection_id={id}");
    println!("{uri}");
    Ok(())
}

async fn serve(cli: &Cli, args: &ServeArgs) -> Result<()> {
    let (mut manager, wallet) = build_manager(cli, &args.wallet, args.network.clone()).await?;
    let registry = PocketRegistry::new(pockets_path(cli))?;
    let pocket_configs = registry.list()?;
    let local = local_wallet(&args.wallet)?;
    let policies = PolicyStore::new(&cli.policy)?.connections()?;
    let with_policy: HashSet<String> = policies.iter().map(|p| p.id.clone()).collect();

    let refiller = wallet
        .as_ref()
        .map(|main| Arc::new(Refiller::new(Arc::clone(main), registry.clone())));
    let mut pockets: HashMap<String, Arc<PocketNwcHandler>> = HashMap::new();
    let mut started = 0;
    for record in manager.list_connections() {
        if !with_policy.contains(&record.id) {
            continue;
        }
        if let Some(config) = pocket_configs
            .iter()
            .find(|config| config.connection_id == record.id)
        {
            // A pocket connection never falls back to the main wallet.
            let local = local
                .as_ref()
                .context("pocket connections need --mint-url, --wallet-db and a seed")?;
            let pocket = open_pocket_for(local, config.index).await?;
            let handler = Arc::new(PocketNwcHandler::new(
                record.id.clone(),
                pocket,
                refiller.clone(),
            )?);
            pockets.insert(record.id.clone(), Arc::clone(&handler));
            manager
                .start_with_inner(
                    &record.id,
                    Arc::new(Backend::Pocket(handler, args.network.clone())),
                )
                .await?;
        } else {
            manager.start(&record.id).await?;
        }
        started += 1;
    }
    println!(
        "serving {started} connection(s), {} with a pocket",
        pockets.len()
    );

    let mut background = Vec::new();
    if let Some(main) = wallet.clone() {
        if args.claim_interval_secs > 0 {
            background.push(spawn_quote_claimer(
                Arc::clone(&main),
                Duration::from_secs(args.claim_interval_secs),
            ));
        }
    }
    if let (Some(main), Some(refiller)) = (wallet, refiller) {
        let mut events = refiller.subscribe();
        background.push(tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                match serde_json::to_string(&event) {
                    Ok(json) => println!("pocket event: {json}"),
                    Err(error) => eprintln!("pocket event: {error}"),
                }
            }
        }));
        if !pockets.is_empty() {
            let interval = if args.claim_interval_secs > 0 {
                Duration::from_secs(args.claim_interval_secs)
            } else {
                DEFAULT_MAINTENANCE_INTERVAL
            };
            let policy_path = cli.policy.clone();
            background.push(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    let policies = match PolicyStore::new(&policy_path)
                        .and_then(|store| store.connections())
                    {
                        Ok(policies) => policies,
                        Err(error) => {
                            eprintln!("pocket maintenance: read policies: {error}");
                            continue;
                        }
                    };
                    if let Err(error) =
                        maintain_pockets(&main, &registry, &policies, &pockets, now_secs()).await
                    {
                        eprintln!("pocket maintenance failed: {error:#}");
                    }
                }
            }));
        }
    }

    let signal = tokio::signal::ctrl_c().await;
    for task in background {
        task.abort();
    }
    manager.stop_all().await;
    signal?;
    println!("stopped");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.manager == cli.policy {
        bail!("--manager and --policy must be different paths");
    }

    match &cli.command {
        Command::Create(args) => {
            let (mut manager, main) = build_manager(&cli, &args.wallet, None).await?;
            if args.pocket {
                let main = main.context("pockets need a local wallet")?;
                create_pocket(&cli, args, &mut manager, &main).await?;
            } else {
                let policy = policy_from_args(args)?;
                let uri =
                    manager.create_connection(args.name.clone(), args.relay.clone(), policy)?;
                let views = manager.list_connections_view();
                let id = views
                    .last()
                    .map(|view| view.id.clone())
                    .context("created connection missing from manager")?;
                println!("connection_id={id}");
                println!("{uri}");
            }
        }
        Command::List => {
            let records = ConnectionManager::<Backend>::list_records(&cli.manager)?;
            println!("{}", serde_json::to_string_pretty(&records)?);
        }
        Command::Revoke(args) => {
            ConnectionManager::<Backend>::revoke_policy(&cli.policy, &args.id)?;
            println!("revoked {}", args.id);
            let registry = PocketRegistry::new(pockets_path(&cli))?;
            if let Ok(config) = registry.get(&args.id) {
                if config.swept_at.is_some() {
                    println!("pocket already swept");
                } else if let Some(local) = local_wallet(&args.wallet)? {
                    sweep(&local, &registry, &args.id, true).await?;
                } else {
                    println!(
                        "pocket not swept: pass --mint-url, --wallet-db and a seed, \
                         or let `serve` sweep it"
                    );
                }
            }
        }
        Command::Serve(args) => serve(&cli, args).await?,
        Command::Topup(args) => {
            let local = require_local_wallet(&cli, &args.wallet)?;
            let registry = PocketRegistry::new(pockets_path(&cli))?;
            let config = registry.get(&args.id)?;
            let main = open_main(&local).await?;
            let pocket = open_pocket_for(&local, config.index).await?;
            let refiller = Refiller::new(main, registry);
            let added = refiller
                .top_up(&args.id, &pocket, args.sats * MSAT_PER_SAT)
                .await?;
            println!("added {} sat to pocket {}", added / MSAT_PER_SAT, args.id);
        }
        Command::Sweep(args) => {
            let local = require_local_wallet(&cli, &args.wallet)?;
            let registry = PocketRegistry::new(pockets_path(&cli))?;
            sweep(&local, &registry, &args.id, false).await?;
        }
        Command::Fund(args) => {
            let local = require_local_wallet(&cli, &args.wallet)?;
            let main = open_main(&local).await?;
            let quote = main
                .mint_quote(
                    cdk::nuts::PaymentMethod::BOLT11,
                    Some(Amount::from(args.sats)),
                    None,
                    None,
                )
                .await?;
            println!("{}", quote.request);
            let deadline = std::time::Instant::now() + Duration::from_secs(args.wait_secs);
            loop {
                let minted = main.mint_unissued_quotes().await.unwrap_or(Amount::ZERO);
                if minted > Amount::ZERO {
                    println!(
                        "minted {minted} sat; main balance {} sat",
                        main.total_balance().await?
                    );
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    bail!("invoice not paid within {}s", args.wait_secs);
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
        Command::Pockets(args) => {
            let registry = PocketRegistry::new(pockets_path(&cli))?;
            let local = local_wallet(&args.wallet)?;
            let now = now_secs();
            let mut rows = Vec::new();
            for config in registry.list()? {
                let balance_msat = match &local {
                    Some(local) => Some(
                        u64::from(
                            open_pocket_for(local, config.index)
                                .await?
                                .total_balance()
                                .await?,
                        ) * MSAT_PER_SAT,
                    ),
                    None => None,
                };
                rows.push(serde_json::json!({
                    "connection_id": config.connection_id,
                    "index": config.index,
                    "balance_msat": balance_msat,
                    "ceiling_msat": config.ceiling_msat,
                    "refill": config.refill,
                    "refill_remaining_msat": registry.remaining_msat(&config.connection_id, now)?,
                    "refill_resets_at": registry.resets_at(&config.connection_id, now)?,
                    "swept_at": config.swept_at,
                }));
            }
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
    }

    Ok(())
}
