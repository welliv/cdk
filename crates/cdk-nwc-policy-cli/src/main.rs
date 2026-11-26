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
use cdk_nwc_policy_cli::key::{
    keys_path, load_keys, new_api_key, parse_list, relay_first, save_key, write_private, KeyMeta,
};
use cdk_nwc_policy_cli::pocket::{
    maintain_pockets, open_pocket, open_wallet, registry_path, DEFAULT_MAINTENANCE_INTERVAL,
};
use cdk_nwc_policy_cli::proxy::{self, ProxyConfig};
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
    /// API keys: each key is an NWC link with its own pocket, budget,
    /// per-payment cap, expiry and model list.
    #[command(subcommand)]
    Key(KeyCommand),
    /// Call an NWC link as a client (testing): info, balance or pay.
    Call(CallArgs),
}

#[derive(Debug, Subcommand)]
enum KeyCommand {
    /// Create a key: a pocket, its NWC link and its rules. Prints JSON.
    Create(Box<KeyCreateArgs>),
    /// List keys with status, balances and rules as JSON.
    List(PocketsArgs),
    /// Revoke a key and sweep its pocket home.
    Revoke(RevokeArgs),
    /// OpenAI-compatible proxy: each request is paid per request with ecash
    /// from the calling key's pocket (X-Cashu), change goes back to it.
    Proxy(ProxyArgs),
}

#[derive(Debug, Args)]
struct ProxyArgs {
    #[command(flatten)]
    wallet: WalletArgs,

    /// Routstr provider base URL (routstr-core).
    #[arg(long)]
    provider: String,

    #[arg(long, default_value = "127.0.0.1:8090")]
    listen: String,

    /// Seconds between sweeps of revoked/expired pockets.
    #[arg(long, default_value_t = 10)]
    sweep_interval_secs: u64,
}

#[derive(Debug, Args)]
struct KeyCreateArgs {
    #[command(flatten)]
    wallet: WalletArgs,

    #[arg(long)]
    label: String,

    #[arg(long, required = true)]
    relay: Vec<String>,

    /// Sats moved into the key's pocket now (the most it can spend before a
    /// refill).
    #[arg(long)]
    budget: u64,

    /// Most the main wallet refills this key per --window, in sats
    /// (0 = never refill).
    #[arg(long, default_value_t = 0)]
    cap: u64,

    /// Rolling refill window, e.g. `1d`, `12h`.
    #[arg(long, default_value = "1d")]
    window: String,

    /// Auto-refill at or below this many sats (default: half the budget
    /// when --cap > 0).
    #[arg(long)]
    refill_below: Option<u64>,

    /// Auto-refill up to this many sats (default: the budget).
    #[arg(long)]
    refill_to: Option<u64>,

    /// Largest invoice the pocket pays for this key, in sats.
    #[arg(long)]
    max_per_payment: Option<u64>,

    /// Relative expiry, e.g. `10m`, `12h`, `7d`. Afterwards the key is
    /// refused and `serve` sweeps its pocket home.
    #[arg(long)]
    expires: Option<String>,

    /// Comma-separated models the key may use (stored; enforced by the
    /// gateway, since a bolt11 does not carry the model).
    #[arg(long)]
    models: Option<String>,

    #[arg(long, default_value = "get_info,get_balance,pay_invoice")]
    methods: String,

    /// Write the NWC URI here (0600) instead of printing it in the JSON.
    #[arg(long)]
    uri_out: Option<PathBuf>,

    /// Write the API key for `key proxy` here (0600) instead of printing it.
    #[arg(long)]
    api_key_out: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct CallArgs {
    /// File holding the NWC URI.
    #[arg(long)]
    uri_file: PathBuf,

    /// `info`, `balance` or `pay`.
    method: String,

    /// Invoice for `pay`.
    #[arg(long)]
    invoice: Option<String>,

    #[arg(long, default_value_t = 60)]
    timeout_secs: u64,
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

#[derive(Debug, Clone, Args)]
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
    /// ignored (use --max-per-payment-sats). Needs --mint-url, --wallet-db
    /// and a seed.
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

    /// Largest invoice a pocket link may pay, in sats.
    #[arg(long, requires = "pocket")]
    max_per_payment_sats: Option<u64>,
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
        // The per-payment cap is checked against invoice + fee reserve, so
        // add the reserve to keep the cap in invoice terms.
        if args.max_per_payment_sats == Some(0) {
            bail!("--max-per-payment must be greater than zero");
        }
        let max_single_payment_msat = args
            .max_per_payment_sats
            .map(|sats| sats * MSAT_PER_SAT + args.fee_reserve_msat);
        return Ok(ConnectionPolicy {
            id: String::new(),
            name: args.name.clone(),
            client_pubkey: String::new(),
            allowed_methods,
            budget_msat: None,
            budget_period: BudgetPeriod::Never,
            max_single_payment_msat,
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

/// A newly created pocket link.
struct CreatedPocket {
    id: String,
    uri: String,
    index: u32,
    funded_sat: u64,
    cost_sat: u64,
}

async fn create_pocket(
    cli: &Cli,
    args: &CreateArgs,
    manager: &mut ConnectionManager<Backend>,
    main: &Wallet,
) -> Result<CreatedPocket> {
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
    let (mut funded_sat, mut cost_sat) = (0, 0);
    if args.pocket_sats > 0 {
        let (cost, received) =
            transfer_to_pocket(main, &pocket, Amount::from(args.pocket_sats), None).await?;
        eprintln!("pocket #{index} funded with {received} sat (cost {cost} sat)");
        funded_sat = u64::from(received);
        cost_sat = u64::from(cost);
    }
    Ok(CreatedPocket {
        id,
        uri,
        index,
        funded_sat,
        cost_sat,
    })
}

fn key_to_create_args(args: &KeyCreateArgs) -> Result<CreateArgs> {
    let (below, to) = match (args.refill_below, args.refill_to) {
        (Some(below), Some(to)) => (Some(below), Some(to)),
        (None, None) if args.cap > 0 => (Some(args.budget / 2), Some(args.budget)),
        (None, None) => (None, None),
        (below, to) => (
            Some(below.unwrap_or(args.budget / 2)),
            Some(to.unwrap_or(args.budget)),
        ),
    };
    if args.budget == 0 {
        bail!("--budget must be greater than zero");
    }
    Ok(CreateArgs {
        wallet: args.wallet.clone(),
        name: args.label.clone(),
        relay: args.relay.clone(),
        budget_msat: 0,
        methods: args.methods.clone(),
        single_cap_msat: 0,
        fee_reserve_msat: 0,
        budget_period: "never".to_owned(),
        expires_at: None,
        expires_in: args.expires.clone(),
        pocket: true,
        pocket_sats: args.budget,
        ceiling_sats: None,
        refill_cap_sats: args.cap,
        refill_window: args.window.clone(),
        auto_refill_below_sats: below,
        auto_refill_to_sats: to,
        max_per_payment_sats: args.max_per_payment,
    })
}

async fn key_create(cli: &Cli, args: &KeyCreateArgs) -> Result<()> {
    let create = key_to_create_args(args)?;
    let (mut manager, main) = build_manager(cli, &create.wallet, None).await?;
    let main = main.context("keys need a local wallet")?;
    let created = create_pocket(cli, &create, &mut manager, &main).await?;
    let models = parse_list(args.models.as_deref());
    let (api_key, api_key_sha256) = new_api_key();
    save_key(
        &keys_path(&cli.policy),
        KeyMeta {
            key_id: created.id.clone(),
            label: args.label.clone(),
            allowed_models: models.clone(),
            max_per_payment_sats: args.max_per_payment,
            created_at: now_secs(),
            api_key_sha256: Some(api_key_sha256),
        },
    )?;
    let uri = relay_first(&created.uri);
    let mut out = serde_json::json!({
        "key_id": created.id,
        "label": args.label,
        "pocket_index": created.index,
        "budget_sat": created.funded_sat,
        "funding_cost_sat": created.cost_sat,
        "refill_cap_sat": args.cap,
        "refill_window": args.window,
        "auto_refill": create.auto_refill_below_sats.map(|below| serde_json::json!({
            "below_sat": below,
            "to_sat": create.auto_refill_to_sats,
        })),
        "max_per_payment_sat": args.max_per_payment,
        "expires_at": resolve_expiry(None, args.expires.as_deref(), now_secs())?,
        "allowed_models": models,
        "methods": parse_list(Some(&args.methods)),
    });
    match &args.uri_out {
        Some(path) => {
            write_private(path, uri.as_bytes())?;
            out["nwc_uri_file"] = serde_json::json!(path);
        }
        None => out["nwc_uri"] = serde_json::json!(uri),
    }
    match &args.api_key_out {
        Some(path) => {
            write_private(path, api_key.as_bytes())?;
            out["api_key_file"] = serde_json::json!(path);
        }
        None => out["api_key"] = serde_json::json!(api_key),
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

async fn key_list(cli: &Cli, args: &PocketsArgs) -> Result<()> {
    let registry = PocketRegistry::new(pockets_path(cli))?;
    let policies = PolicyStore::new(&cli.policy)?.connections()?;
    let metas = load_keys(&keys_path(&cli.policy))?;
    let local = local_wallet(&args.wallet)?;
    let now = now_secs();
    let mut rows = Vec::new();
    for meta in metas {
        let Ok(config) = registry.get(&meta.key_id) else {
            continue;
        };
        let policy = policies.iter().find(|p| p.id == meta.key_id);
        let status = match policy {
            _ if config.swept_at.is_some() && policy.is_some_and(|p| p.revoked) => "revoked",
            _ if config.swept_at.is_some() => "closed",
            None => "missing",
            Some(p) if p.revoked => "revoked",
            Some(p) if p.expires_at.is_some_and(|at| now >= at) => "expired",
            Some(_) => "active",
        };
        let balance_sat = match &local {
            Some(local) => Some(u64::from(
                open_pocket_for(local, config.index)
                    .await?
                    .total_balance()
                    .await?,
            )),
            None => None,
        };
        rows.push(serde_json::json!({
            "key_id": meta.key_id,
            "label": meta.label,
            "status": status,
            "balance_sat": balance_sat,
            "ceiling_sat": config.ceiling_msat / MSAT_PER_SAT,
            "refill_cap_sat": config.refill.cap_msat / MSAT_PER_SAT,
            "refill_window_secs": config.refill.window_secs,
            "refill_remaining_sat": registry.remaining_msat(&meta.key_id, now)? / MSAT_PER_SAT,
            "max_per_payment_sat": meta.max_per_payment_sats,
            "expires_at": policy.and_then(|p| p.expires_at),
            "allowed_models": meta.allowed_models,
            "swept_at": config.swept_at,
        }));
    }
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

async fn revoke(cli: &Cli, args: &RevokeArgs) -> Result<()> {
    ConnectionManager::<Backend>::revoke_policy(&cli.policy, &args.id)?;
    println!("revoked {}", args.id);
    let registry = PocketRegistry::new(pockets_path(cli))?;
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
    Ok(())
}

async fn call(args: &CallArgs) -> Result<()> {
    let uri = fs::read_to_string(&args.uri_file).context("read NWC URI file")?;
    let client = ForwardingNwcHandler::from_uri(uri.trim(), Duration::from_secs(args.timeout_secs))
        .context("parse NWC URI")?;
    let result = match args.method.as_str() {
        "info" => client
            .get_info()
            .await
            .map(|info| serde_json::to_value(info).unwrap_or_default()),
        "balance" => client
            .get_balance()
            .await
            .map(|balance| serde_json::json!({ "balance_msat": balance.balance })),
        "pay" => {
            let invoice = args.invoice.clone().context("pay needs --invoice")?;
            client
                .pay_invoice(PayInvoiceRequest::new(invoice))
                .await
                .map(|paid| serde_json::json!({ "paid": true, "fees_paid_msat": paid.fees_paid }))
        }
        other => bail!("unknown method {other:?}; use info, balance or pay"),
    };
    let out = match result {
        Ok(value) => value,
        Err(error) => serde_json::json!({
            "error": format!("{:?}", error.code),
            "message": error.message,
        }),
    };
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

type PocketMap = Arc<std::sync::Mutex<HashMap<String, Arc<PocketNwcHandler>>>>;

/// Start every connection with a policy that is not running yet. Returns how
/// many were started.
async fn start_new_connections(
    cli: &Cli,
    args: &ServeArgs,
    manager: &mut ConnectionManager<Backend>,
    refiller: &Option<Arc<Refiller>>,
    pockets: &PocketMap,
) -> Result<usize> {
    let registry = PocketRegistry::new(pockets_path(cli))?;
    let pocket_configs = registry.list()?;
    let local = local_wallet(&args.wallet)?;
    let policies = PolicyStore::new(&cli.policy)?.connections()?;
    let with_policy: HashSet<String> = policies.iter().map(|p| p.id.clone()).collect();
    let mut started = 0;
    for record in manager.list_connections() {
        if !with_policy.contains(&record.id) || manager.is_running(&record.id) {
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
            pockets
                .lock()
                .map_err(|_| anyhow::anyhow!("pocket map poisoned"))?
                .insert(record.id.clone(), Arc::clone(&handler));
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
    Ok(started)
}

async fn serve(cli: &Cli, args: &ServeArgs) -> Result<()> {
    let (mut manager, wallet) = build_manager(cli, &args.wallet, args.network.clone()).await?;
    let registry = PocketRegistry::new(pockets_path(cli))?;

    let refiller = wallet
        .as_ref()
        .map(|main| Arc::new(Refiller::new(Arc::clone(main), registry.clone())));
    let pockets: PocketMap = Arc::default();
    let started = start_new_connections(cli, args, &mut manager, &refiller, &pockets).await?;
    println!(
        "serving {started} connection(s), {} with a pocket",
        pockets.lock().map(|p| p.len()).unwrap_or(0)
    );

    let interval = if args.claim_interval_secs > 0 {
        Duration::from_secs(args.claim_interval_secs)
    } else {
        DEFAULT_MAINTENANCE_INTERVAL
    };
    let mut background = Vec::new();
    if let Some(main) = wallet.clone() {
        if args.claim_interval_secs > 0 {
            background.push(spawn_quote_claimer(
                Arc::clone(&main),
                Duration::from_secs(args.claim_interval_secs),
            ));
        }
    }
    if let (Some(main), Some(refiller)) = (wallet, refiller.clone()) {
        let mut events = refiller.subscribe();
        background.push(tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                match serde_json::to_string(&event) {
                    Ok(json) => println!("pocket event: {json}"),
                    Err(error) => eprintln!("pocket event: {error}"),
                }
            }
        }));
        let policy_path = cli.policy.clone();
        let pockets = Arc::clone(&pockets);
        background.push(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let snapshot = match pockets.lock() {
                    Ok(map) => map.clone(),
                    Err(_) => continue,
                };
                if snapshot.is_empty() {
                    continue;
                }
                let policies =
                    match PolicyStore::new(&policy_path).and_then(|store| store.connections()) {
                        Ok(policies) => policies,
                        Err(error) => {
                            eprintln!("pocket maintenance: read policies: {error}");
                            continue;
                        }
                    };
                if let Err(error) =
                    maintain_pockets(&main, &registry, &policies, &snapshot, now_secs()).await
                {
                    eprintln!("pocket maintenance failed: {error:#}");
                }
            }
        }));
    }

    // Pick up connections created after start-up (no restart needed).
    let mut rescan = tokio::time::interval(interval);
    rescan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    rescan.tick().await;
    let signal = loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => break signal,
            _ = rescan.tick() => {
                let result = async {
                    manager.reload()?;
                    start_new_connections(cli, args, &mut manager, &refiller, &pockets).await
                }
                .await;
                match result {
                    Ok(0) => {}
                    Ok(started) => println!("started {started} new connection(s)"),
                    Err(error) => eprintln!("connection rescan failed: {error:#}"),
                }
            }
        }
    };
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
    // reqwest (key proxy) pulls in aws-lc-rs next to the ring provider the
    // Nostr relay client uses; pick one so rustls does not panic.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    if cli.manager == cli.policy {
        bail!("--manager and --policy must be different paths");
    }

    match &cli.command {
        Command::Create(args) => {
            let (mut manager, main) = build_manager(&cli, &args.wallet, None).await?;
            if args.pocket {
                let main = main.context("pockets need a local wallet")?;
                let created = create_pocket(&cli, args, &mut manager, &main).await?;
                println!("connection_id={}", created.id);
                println!("{}", created.uri);
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
        Command::Revoke(args) => revoke(&cli, args).await?,
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
        Command::Key(KeyCommand::Create(args)) => key_create(&cli, args).await?,
        Command::Key(KeyCommand::List(args)) => key_list(&cli, args).await?,
        Command::Key(KeyCommand::Revoke(args)) => revoke(&cli, args).await?,
        Command::Key(KeyCommand::Proxy(args)) => {
            let local = require_local_wallet(&cli, &args.wallet)?;
            let main = open_main(&local).await?;
            let config = ProxyConfig {
                provider: args.provider.clone(),
                policy_path: cli.policy.clone(),
                pockets_path: pockets_path(&cli),
                keys_path: keys_path(&cli.policy),
                mint_url: local.mint_url.to_owned(),
                wallet_db: local.wallet_db.to_path_buf(),
                seed: local.seed,
                sweep_interval: Duration::from_secs(args.sweep_interval_secs.max(1)),
            };
            proxy::run(config, main, &args.listen).await?;
        }
        Command::Call(args) => call(args).await?,
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
