# Safe NWC links for Cashu wallets

This fork of CDK adds a way to hand an app or AI agent a
[Nostr Wallet Connect](https://github.com/nostr-protocol/nips/blob/master/47.md)
(NIP-47) link backed by a Cashu wallet, and to limit what that link can lose
if it leaks or the agent goes rogue.

| Crate | What it is |
|---|---|
| [`cdk-nwc-pocket`](crates/cdk-nwc-pocket) | **New:** per-connection Cashu pockets. Each link spends from its own wallet. |
| [`cdk-nwc-policy`](crates/cdk-nwc-policy) | Policy ledger: method allowlist, expiry, revoke, replay protection, optional shared-balance budgets. |
| [`cdk-nwc-policy-cli`](crates/cdk-nwc-policy-cli) | `nwc-policy-cli`: create, list, revoke and serve links, with or without pockets. |
| [`cdk-nwc-forward`](crates/cdk-nwc-forward) | Forward NWC requests to an upstream NWC wallet (e.g. Alby Hub). |
| [`cdk-nostr`](crates/cdk-nostr) (upstream, extended) | NIP-47 wallet service; now passes request context (event ID, client key) to handlers. |

## The problem

An NWC link is a bearer credential. Whoever holds the string can ask the
wallet to pay. The usual fix is a budget counted in a ledger in front of the
wallet, but then every link can still *reach* the whole balance: a ledger
bug, a race, a lost reply, or a wrong budget setting is enough to drain it.

## How it works

A **pocket** is a separate Cashu wallet for one link:

- **Isolation by construction.** The link's NWC service talks only to its
  pocket wallet (own seed, own database). It cannot spend more than the
  ecash in the pocket, whatever the ledger says.
- **Recoverable.** Pocket seeds are `HMAC-SHA512(main_seed, "cdk-nwc-pocket-v1" || index)`,
  so pockets can be restored from the main seed (NUT-13). Indexes are never reused.
- **Funding and sweeping are same-mint swaps**, no Lightning involved.
  Revoking or expiring a link swaps every pocket proof back to the main
  wallet, so copies of the pocket's old proofs become worthless. Pending
  proofs are treated as spent and swept later.
- **Refill cap on a rolling window**, enforced on the main-wallet side.
  Optional auto-refill tops the pocket up (e.g. below 5 sats, up to 20) but
  never beyond the cap per window. A rolling window, unlike a calendar
  window, cannot be doubled across a boundary.
- **Honest fees.** `fees_paid` is the measured drop in pocket balance minus
  the invoice amount, mint input fees included.
- **One event** when the link runs dry: `OutOfFunds { reason: RefillCapReached | NoAutoRefill | CeilingReached, refill_resets_at, … }`.
- **Pocket-aware `get_balance`**: the link only ever sees its pocket.
- **Ceiling**: funds received above it (via `make_invoice`) move to main.
- The policy layer still runs in front: method allowlist, expiry, revoke
  (`UNAUTHORIZED`), replay protection and pending-as-spent.
- Shared-balance mode (a ledger budget in front of the main wallet) is still
  available as an explicit choice: create a link without `--pocket`.

## Setup

```bash
cargo build -p cdk-nwc-policy-cli
alias nwc-policy-cli=./target/debug/nwc-policy-cli
export CDK_NWC_SEED_HEX=<64-byte hex seed>      # never commit it
W="--mint-url https://cashu.mutinynet.com --wallet-db ./wallet.sqlite"

# Fund the main wallet (prints a bolt11; pay it, e.g. from https://faucet.mutinynet.com)
nwc-policy-cli fund $W --sats 200

# A link with its own 30 sat pocket, auto-refilled to 20 sats when it drops
# to 5, at most 40 sats of refills per rolling day:
nwc-policy-cli create $W --name agent --relay wss://relay.primal.net \
  --pocket --pocket-sats 30 --refill-cap-sats 40 --refill-window 1d \
  --auto-refill-below-sats 5 --auto-refill-to-sats 20
# prints connection_id=… and the nostr+walletconnect:// URI (treat it as a secret)

nwc-policy-cli serve $W --network signet      # serves every link
nwc-policy-cli pockets $W                     # balances, refill allowance left
nwc-policy-cli topup $W --id <id> --sats 10   # manual, not counted against the cap
nwc-policy-cli revoke $W --id <id>            # revoke and sweep the pocket home
```

`serve` also sweeps revoked or expired pockets on its own, mints paid
`make_invoice` quotes, and prints `pocket event: {...}` lines.

## Rogue-agent demo

```bash
cargo run -p cdk-nwc-pocket --example rogue_agent -- \
  --mint-url https://cashu.mutinynet.com --work-dir ./rogue-demo
```

The demo funds a 30 sat pocket, serves it as an NWC link over public relays,
and gives the link to an "agent" that pays invoices owned by an attacker
wallet on the same mint. Expected result:

```text
round 1: agent paid 7 sat to attacker (fee 2000 msat, 69 ms)
round 2: agent paid 7 sat to attacker (fee 2000 msat, 2006 ms)
round 3: agent paid 7 sat to attacker (fee 2000 msat, 2000 ms)
round 4: CUT OFF: INSUFFICIENT_BALANCE "insufficient pocket balance" (15 ms)
agent asks for 147 sat (= whole main wallet): REFUSED INSUFFICIENT_BALANCE
revoked: swept 2 sat home (0 sat pending)
| main during attack | 147 -> 147 sat (untouched) |
```

It then returns the attacker's loot to main, so a signet run costs only
fees. If the main wallet is empty it prints a signet invoice to pay first.
`--no-relay` runs the agent in-process; any Cashu mint URL works (a local
FakeWallet mint needs no funding).

## Tests

```bash
cargo test -p cdk-nwc-pocket -p cdk-nwc-policy -p cdk-nwc-policy-cli -p cdk-nwc-forward -p cdk-nostr
cargo test -p cdk-integration-tests --test nwc_pocket   # in-process mint
```

`nwc_pocket.rs` checks, against a real in-process mint:

- a pocket link cannot spend beyond its pocket even with no policy ledger;
- revoke sweeps the pocket, and proofs copied out before revoke are spent;
- auto-refill stops at the rolling-window cap and allows refills again once
  the window has passed;
- `fees_paid` equals the pocket balance drop on a fee-charging mint;
- funds above the ceiling move to main;
- the `serve` maintenance pass sweeps expired pockets;
- replay protection and revoke still apply in front of a pocket.

## Limits

- **Same mint only.** A pocket trusts the same mint as the main wallet.
- **Wallet-level isolation, same process.** `serve` holds the main wallet
  to refill and sweep. For OS-level isolation, run the NWC service in its
  own process with only the pocket database, and refill from outside.
- **`sat` unit only.** Amountless invoices are not supported.
- **No `get_budget`.** The NIP-47 `get_budget` method is not exposed yet,
  because `nostr` 0.45 cannot parse it; the CLI's `pockets` command shows the
  same numbers.
- **Mint fees** (e.g. `input_fee_ppk = 100` on Mutinynet) make small
  transfers cost about 1 sat each way.
- Experimental code on top of an early-stage project. Use amounts you can
  afford to lose.

## License

Same as CDK: MIT or Apache-2.0, at your option ([LICENSE.md](LICENSE.md),
[LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)). The crates
inherit `license = "Apache-2.0 OR MIT"` from the workspace.

See [NOVEMBER.md](NOVEMBER.md) for what was built during Nut November and
what came from upstream CDK.
