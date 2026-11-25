//! Rogue-agent demo: an NWC link drains its pocket, is cut off, and the main
//! wallet is never touched.
//!
//! ```text
//! cargo run -p cdk-nwc-pocket --example rogue_agent -- \
//!     --mint-url https://cashu.mutinynet.com --work-dir /tmp/rogue-demo
//! ```
//!
//! What it does:
//!
//! 1. Opens (or creates) a main wallet in `--work-dir`. If it holds less than
//!    the pocket needs, it prints a mint invoice to pay from a signet faucet
//!    and waits for it.
//! 2. Derives pocket #N from the main seed and funds it with `--pocket-sats`
//!    via a same-mint swap.
//! 3. Serves the pocket as an NWC link over Nostr relays (or in-process with
//!    `--no-relay`) and hands the link to a "rogue agent".
//! 4. The agent pays invoices owned by an attacker wallet on the same mint
//!    until the pocket is empty, then tries one invoice as large as the whole
//!    main wallet.
//! 5. Revokes the link: stops the service and sweeps the pocket home.
//! 6. Hands the attacker's loot back to main so the demo costs only fees.
//!
//! The seed is kept in `<work-dir>/main-seed.hex` (mode 0600) or taken from
//! `CDK_DEMO_SEED_HEX`; it is never printed.

#![allow(missing_docs)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cdk::nuts::{CurrencyUnit, PaymentMethod};
use cdk::wallet::{ReceiveOptions, Wallet};
use cdk::Amount;
use cdk_nostr::nwc::nip47::{
    ErrorCode, GetBalanceResponse, NIP47Error, PayInvoiceRequest, PayInvoiceResponse,
};
use cdk_nostr::nwc::{NwcRequestHandler, NwcService, NwcServiceConfig};
use cdk_nwc_forward::ForwardingNwcHandler;
use cdk_nwc_pocket::{
    derive_pocket_seed, sweep_pocket, transfer_to_pocket, PocketConfig, PocketNwcHandler,
    PocketRegistry, RefillPolicy, MSAT_PER_SAT,
};
use nostr::prelude::{Keys, RelayUrl};
use tokio_util::sync::CancellationToken;

const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.primal.net",
    "wss://nostr.mom",
    "wss://relay.damus.io",
];

/// The rogue agent: it only knows the NWC link.
enum Agent {
    /// Calls the pocket handler directly (`--no-relay`).
    Local(Arc<PocketNwcHandler>),
    /// Talks NIP-47 over relays with a fresh client per request, so one bad
    /// relay connection does not stall the demo.
    Remote(String),
}

impl Agent {
    fn client(uri: &str) -> Result<ForwardingNwcHandler, NIP47Error> {
        ForwardingNwcHandler::from_uri(uri, Duration::from_secs(25)).map_err(|e| NIP47Error {
            code: ErrorCode::Other,
            message: e.to_string(),
        })
    }

    async fn get_balance(&self) -> Result<GetBalanceResponse, NIP47Error> {
        match self {
            Self::Local(handler) => handler.get_balance().await,
            Self::Remote(uri) => Self::client(uri)?.get_balance().await,
        }
    }

    async fn pay_invoice(
        &self,
        request: PayInvoiceRequest,
    ) -> Result<PayInvoiceResponse, NIP47Error> {
        match self {
            Self::Local(handler) => handler.pay_invoice(request).await,
            Self::Remote(uri) => Self::client(uri)?.pay_invoice(request).await,
        }
    }
}

struct Args {
    mint_url: String,
    work_dir: PathBuf,
    pocket_sats: u64,
    invoice_sats: u64,
    relays: Vec<String>,
    no_relay: bool,
    wait_secs: u64,
    main_sats: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        mint_url: "https://cashu.mutinynet.com".to_owned(),
        work_dir: PathBuf::from("rogue-agent-demo"),
        pocket_sats: 30,
        invoice_sats: 7,
        relays: Vec::new(),
        no_relay: false,
        wait_secs: 600,
        main_sats: 20,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--mint-url" => args.mint_url = value()?,
            "--work-dir" => args.work_dir = PathBuf::from(value()?),
            "--pocket-sats" => args.pocket_sats = value()?.parse().map_err(|_| "bad number")?,
            "--invoice-sats" => args.invoice_sats = value()?.parse().map_err(|_| "bad number")?,
            "--relay" => args.relays.push(value()?),
            "--main-sats" => args.main_sats = value()?.parse().map_err(|_| "bad number")?,
            "--wait-secs" => args.wait_secs = value()?.parse().map_err(|_| "bad number")?,
            "--no-relay" => args.no_relay = true,
            "-h" | "--help" => {
                return Err("usage: rogue_agent [--mint-url URL] [--work-dir DIR] \
                     [--pocket-sats N] [--main-sats N] [--invoice-sats N] [--relay URL]... \
                     [--no-relay] [--wait-secs N]"
                    .to_owned())
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if args.relays.is_empty() {
        args.relays = DEFAULT_RELAYS.iter().map(|r| (*r).to_owned()).collect();
    }
    if args.invoice_sats == 0 || args.pocket_sats == 0 {
        return Err("--pocket-sats and --invoice-sats must be > 0".to_owned());
    }
    Ok(args)
}

type DemoResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .init();
    }
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(64);
        }
    };
    match run(args).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("demo failed: {error}");
            ExitCode::from(2)
        }
    }
}

fn write_private(path: &Path, contents: &str) -> DemoResult<()> {
    fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn load_or_create_seed(path: &Path, env: Option<&str>) -> DemoResult<[u8; 64]> {
    let hex = match env.map(std::env::var) {
        Some(Ok(hex)) => hex,
        _ if path.exists() => fs::read_to_string(path)?,
        _ => {
            let seed = Keys::generate().secret_key().to_secret_hex()
                + &Keys::generate().secret_key().to_secret_hex();
            write_private(path, &seed)?;
            seed
        }
    };
    let hex = hex.trim();
    if hex.len() != 128 {
        return Err("seed must be 64 bytes of hex".into());
    }
    let mut seed = [0u8; 64];
    for (i, byte) in seed.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)?;
    }
    Ok(seed)
}

async fn open_wallet(mint_url: &str, db: &Path, seed: [u8; 64]) -> DemoResult<Arc<Wallet>> {
    if !db.exists() {
        write_private(db, "")?;
    }
    let store = cdk_sqlite::WalletSqliteDatabase::new(db.to_path_buf()).await?;
    Ok(Arc::new(Wallet::new(
        mint_url,
        CurrencyUnit::Sat,
        Arc::new(store),
        seed,
        None,
    )?))
}

async fn sats(wallet: &Wallet) -> DemoResult<u64> {
    Ok(u64::from(wallet.total_balance().await?))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Make sure `main` holds at least `needed` sats, asking for a faucet payment.
async fn ensure_funded(main: &Wallet, needed: u64, wait_secs: u64) -> DemoResult<bool> {
    main.mint_unissued_quotes().await.ok();
    let have = sats(main).await?;
    if have >= needed {
        return Ok(true);
    }
    let missing = needed - have;
    let quote = main
        .mint_quote(
            PaymentMethod::BOLT11,
            Some(Amount::from(missing)),
            None,
            None,
        )
        .await?;
    println!("main wallet holds {have} sat, needs {needed} sat.");
    println!("pay this {missing} sat signet invoice (e.g. https://faucet.mutinynet.com):");
    println!("\n{}\n", quote.request);
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(5)).await;
        main.mint_unissued_quotes().await.ok();
        if sats(main).await? >= needed {
            println!("funded: main wallet holds {} sat", sats(main).await?);
            return Ok(true);
        }
    }
    println!("not funded within {wait_secs}s; rerun once the invoice is paid.");
    Ok(false)
}

async fn run(args: Args) -> DemoResult<bool> {
    fs::create_dir_all(&args.work_dir)?;
    let dir = &args.work_dir;
    let main_seed = load_or_create_seed(&dir.join("main-seed.hex"), Some("CDK_DEMO_SEED_HEX"))?;
    let attacker_seed = load_or_create_seed(&dir.join("attacker-seed.hex"), None)?;
    let main = open_wallet(&args.mint_url, &dir.join("main.sqlite"), main_seed).await?;
    let attacker = open_wallet(&args.mint_url, &dir.join("attacker.sqlite"), attacker_seed).await?;

    // Leftovers from an interrupted run go back to main first.
    attacker.mint_unissued_quotes().await.ok();
    return_loot(&attacker, &main).await?;
    let registry = PocketRegistry::new(dir.join("pockets.json"))?;
    for old in registry.list()? {
        if old.swept_at.is_some() {
            continue;
        }
        let wallet = open_wallet(
            &args.mint_url,
            &dir.join(format!("pocket-{}.sqlite", old.index)),
            derive_pocket_seed(&main_seed, old.index),
        )
        .await?;
        let report = sweep_pocket(&wallet, &main).await?;
        if report.is_complete() {
            registry.mark_swept(&old.connection_id, now())?;
        }
        println!(
            "swept leftover pocket #{}: {} sat home",
            old.index,
            report.swept_msat / MSAT_PER_SAT
        );
    }

    // Main keeps `--main-sats` beside the pocket; +3 covers swap input fees.
    let needed = args.pocket_sats + args.main_sats + 3;
    println!("== rogue-agent demo on {} ==", args.mint_url);
    if !ensure_funded(&main, needed, args.wait_secs).await? {
        return Ok(false);
    }

    // 1. A fresh pocket for this link.
    let index = registry.next_index()?;
    let connection_id = format!("agent-{index}");
    let pocket = open_wallet(
        &args.mint_url,
        &dir.join(format!("pocket-{index}.sqlite")),
        derive_pocket_seed(&main_seed, index),
    )
    .await?;
    registry.add(PocketConfig {
        connection_id: connection_id.clone(),
        index,
        ceiling_msat: args.pocket_sats * MSAT_PER_SAT,
        refill: RefillPolicy::default(),
        swept_at: None,
    })?;
    let (cost, received) =
        transfer_to_pocket(&main, &pocket, Amount::from(args.pocket_sats), None).await?;
    let main_start = sats(&main).await?;
    let pocket_start = sats(&pocket).await?;
    println!(
        "pocket #{index} funded with {received} sat (cost {cost} sat). \
         main: {main_start} sat, pocket: {pocket_start} sat"
    );

    // 2. Serve the pocket as an NWC link and give it to the agent.
    let handler = Arc::new(PocketNwcHandler::new(
        connection_id.clone(),
        Arc::clone(&pocket),
        None,
    )?);
    let cancel = CancellationToken::new();
    let agent = if args.no_relay {
        println!("agent talks to the pocket handler in-process (--no-relay)");
        Agent::Local(handler.clone())
    } else {
        let relays = args
            .relays
            .iter()
            .map(|r| RelayUrl::parse(r))
            .collect::<Result<Vec<_>, _>>()?;
        let service = NwcService::new(NwcServiceConfig {
            service_keys: Keys::generate(),
            client_secret: Keys::generate().secret_key().clone(),
            relays,
            lud16: None,
        })?;
        let uri = service.connection_uri().to_string();
        let serve_handler = handler.clone();
        let serve_cancel = cancel.clone();
        tokio::spawn(async move {
            if let Err(error) = service.run(serve_handler, serve_cancel).await {
                eprintln!("nwc service stopped: {error}");
            }
        });
        println!(
            "NWC link served over {} (secret not shown)",
            args.relays.join(", ")
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        Agent::Remote(uri)
    };

    // The service may still be subscribing on the relays; a fresh request
    // per attempt is safe because get_balance has no side effects.
    let mut balance = None;
    for attempt in 1..=4 {
        match agent.get_balance().await {
            Ok(response) => {
                balance = Some(response);
                break;
            }
            Err(e) => println!(
                "get_balance attempt {attempt}: {} {}",
                code_name(e.code),
                e.message
            ),
        }
    }
    let balance = balance.ok_or("NWC link never answered get_balance")?;
    println!(
        "agent get_balance: {} sat (pocket only)",
        balance.balance / MSAT_PER_SAT
    );

    // 3. Drain. Only an out-of-funds answer (pocket empty, cap reached)
    // counts as a cut-off; relay hiccups and other failures get a new invoice.
    // (A FakeWallet mint auto-pays mint quotes, so there some invoices are
    // already paid by the time the agent tries them.) What was really taken is read from
    // the pocket itself, because a payment can succeed while its reply is lost.
    let max_rounds = args.pocket_sats / args.invoice_sats + 12;
    let mut cut_off = None;
    for round in 1..=max_rounds {
        let quote = attacker
            .mint_quote(
                PaymentMethod::BOLT11,
                Some(Amount::from(args.invoice_sats)),
                None,
                None,
            )
            .await?;
        let started = Instant::now();
        match agent
            .pay_invoice(PayInvoiceRequest {
                id: None,
                invoice: quote.request.clone(),
                amount: None,
            })
            .await
        {
            Ok(response) => println!(
                "round {round}: agent paid {} sat to attacker (fee {} msat, {} ms)",
                args.invoice_sats,
                response.fees_paid.unwrap_or(0),
                started.elapsed().as_millis()
            ),
            Err(error)
                if matches!(
                    error.code,
                    ErrorCode::InsufficientBalance | ErrorCode::QuotaExceeded
                ) =>
            {
                println!(
                    "round {round}: CUT OFF: {} \"{}\" ({} ms)",
                    code_name(error.code),
                    error.message,
                    started.elapsed().as_millis()
                );
                cut_off = Some(code_name(error.code));
                break;
            }
            Err(error) => println!(
                "round {round}: payment failed {} \"{}\"; trying a new invoice",
                code_name(error.code),
                error.message
            ),
        }
    }
    let stolen = handler.spent_msat() / MSAT_PER_SAT;

    // 4. Go for the whole main wallet.
    let big = attacker
        .mint_quote(
            PaymentMethod::BOLT11,
            Some(Amount::from(main_start.max(1))),
            None,
            None,
        )
        .await?;
    let big_result = agent
        .pay_invoice(PayInvoiceRequest {
            id: None,
            invoice: big.request,
            amount: None,
        })
        .await;
    match &big_result {
        Ok(_) => println!("!! {main_start} sat invoice was paid"),
        Err(error) => println!(
            "agent asks for {main_start} sat (= whole main wallet): REFUSED {}",
            code_name(error.code)
        ),
    }
    let main_after_attack = sats(&main).await?;

    // 5. Revoke: stop serving and sweep the pocket home.
    cancel.cancel();
    let report = handler.sweep_to(&main).await?;
    if report.is_complete() {
        registry.mark_swept(&connection_id, now())?;
    }
    println!(
        "revoked: swept {} sat home ({} sat pending){}",
        report.swept_msat / MSAT_PER_SAT,
        report.pending_msat / MSAT_PER_SAT,
        if report.is_complete() {
            ""
        } else {
            " — rerun sweep later"
        }
    );

    // 6. Clean up.
    attacker.mint_unissued_quotes().await.ok();
    let loot = sats(&attacker).await?;
    let returned = return_loot(&attacker, &main).await?;
    let main_end = sats(&main).await?;

    let untouched = main_after_attack == main_start;
    let bounded = stolen <= pocket_start;
    println!();
    println!("| check | result |");
    println!("|---|---|");
    println!("| pocket funded | {pocket_start} sat |");
    println!("| pocket spent before cut-off (incl. fees) | {stolen} sat |");
    println!("| attacker wallet held | {loot} sat (a FakeWallet mint also auto-pays) |");
    println!(
        "| cut off with | {} |",
        cut_off.as_deref().unwrap_or("never (pocket not drained)")
    );
    println!(
        "| whole-wallet invoice | {} |",
        if big_result.is_err() {
            "refused"
        } else {
            "PAID"
        }
    );
    println!(
        "| main during attack | {main_start} -> {main_after_attack} sat ({}) |",
        if untouched { "untouched" } else { "CHANGED" }
    );
    println!("| loot returned to main | {returned} sat |");
    println!("| main at end | {main_end} sat |");
    Ok(untouched && bounded && big_result.is_err() && cut_off.is_some())
}

fn code_name(code: ErrorCode) -> String {
    serde_json::to_string(&code)
        .map(|s| s.trim_matches('"').to_owned())
        .unwrap_or_else(|_| "UNKNOWN".to_owned())
}

/// Move everything the attacker holds back to `main`. Returns sats received.
async fn return_loot(attacker: &Wallet, main: &Wallet) -> DemoResult<u64> {
    let proofs = attacker.get_unspent_proofs().await?;
    if proofs.is_empty() {
        return Ok(0);
    }
    // `receive_proofs` swaps the proofs to main's keys and pays the input fee.
    let received = main
        .receive_proofs(proofs.clone(), ReceiveOptions::default(), None, None)
        .await?;
    attacker.check_proofs_spent(proofs).await?;
    Ok(u64::from(received))
}
