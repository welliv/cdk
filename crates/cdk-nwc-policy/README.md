# cdk-nwc-policy

Per-connection policy enforcement for CDK NWC wallet services.

Status: prototype. It is intentionally small and has no wallet, relay or mint
dependencies. It enforces bounded, revocable authority around an existing
`NwcRequestHandler`.

## What it adds

- multiple app connections, each identified by a client public key;
- per-connection method allowlists;
- budgets with daily, weekly, monthly and yearly rollover;
- a single-payment cap;
- a fee reserve held in addition to the BOLT11 invoice amount;
- invoice-authoritative payment amounts, with mismatched request amounts rejected;
- expiry and revocation;
- persistent Nostr event-ID replay protection;
- reserve/commit/release accounting for every request;
- a context-aware handler wrapper for CDK's NWC service.

## What it does not do

- it does not own proofs, keys or a wallet database;
- it does not run relays;
- it does not perform mint or melt operations;
- it does not create NWC connection strings;
- it does not enforce host clock hardening yet;
- it does not use a transactional database yet;
- connection generation and management live in `cdk-nwc-policy-manager`
  code in this crate and the CLI crate;
- it does not remove replay records after a failed wallet call, so a failed
  request must be retried with a fresh event ID.

Those are deliberate boundaries. The wallet engine remains responsible for
money movement; this crate decides whether a request is allowed.

## How it fits

```rust,no_run
use std::sync::Arc;

use cdk_nostr::nwc::NwcService;
use cdk_nwc_policy::PolicyNwcHandler;

# async fn example<H: cdk_nostr::nwc::NwcRequestHandler>(
#     wallet_handler: H,
#     config: cdk_nostr::nwc::service::NwcServiceConfig,
# ) -> Result<(), Box<dyn std::error::Error>> {
let handler = PolicyNwcHandler::new("nwc-policy.json", Arc::new(wallet_handler))?;
// Add app connections with `handler.add_connection(...)`.
let service = NwcService::new(config)?;
let cancel = CancellationToken::new();
service.run(Arc::new(handler), cancel).await?;
# Ok(())
# }
```

`cdk-nostr` dispatches requests through `RequestContext`, which carries the
request event ID, client public key and service-received timestamp. The policy
handler uses that context for replay checks and per-client authorization.
Client-supplied event time is never used for budgets or expiry.

## Test

```bash
cargo test -p cdk-nwc-policy
```

Current coverage: 26 unit tests across the policy ledger, NWC handler and
connection manager, plus two end-to-end tests in
`cdk-integration-tests/tests/nwc_policy_e2e.rs`
that drives a real CDK wallet, an in-memory mint, an in-process Nostr relay
and a standard NWC client.

The ledger fails closed on corrupt JSON and duplicate keys found on load, holds
an exclusive file lock, writes state before swapping memory, and binds
settlements to their budget period. Payment amounts come from the BOLT11
invoice, not the client-supplied optional amount. Fee overage is charged only
up to the reservation and recorded separately because the payment has already
happened. `get_info` capabilities are filtered per connection.

Known inherited `cdk-nostr` limitations, outside this crate: the wallet service
still retains the client secret for URI generation, request encryption tags are
not strictly enforced, and the service-level info event advertises
`list_transactions` without an extensions tag.
