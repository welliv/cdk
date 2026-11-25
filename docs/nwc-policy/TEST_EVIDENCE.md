# Test evidence

Run on branch `feat/nwc-policy-submission` at commit `73bfce1`, after removing CWC and the direct `CHANGELOG.md` edit.

## Formatting

```bash
cargo fmt --all -- --check
```

Result: pass.

## Targeted tests

```bash
cargo test -p cdk-nostr -p cdk-nwc-policy -p cdk-nwc-forward -p cdk-nwc-policy-cli --no-fail-fast
```

Results:

- `cdk-nostr`: 18 passed, 0 failed.
- `cdk-nwc-policy/tests/manager.rs`: 4 passed, 0 failed.
- `cdk-nwc-policy/tests/policy_handler.rs`: 9 passed, 0 failed.
- `cdk-nwc-policy/tests/policy_store.rs`: 14 passed, 0 failed.
- Other targets: 0 tests, 0 failed.

Total targeted: **45 passed, 0 failed**.

## Earlier workspace check

Before removing CWC, the following also passed:

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --lib --workspace --exclude cdk-postgres --exclude cdk-integration-tests
cargo test --doc
```

Result: exit 0.

## Live signet validation

Run on 2026-11-25 UTC against two signet Alby Hub Cashu NWC connections. Secrets were stored in `0600` scratch files and are not recorded here.

Direct client checks:

- both URIs parsed with 2 relays;
- `get_info`: alias `NWC (Cashu)`, network `signet`;
- advertised methods: `pay_invoice`, `multi_pay_invoice`, `get_info`, `get_balance`, `make_invoice`, `lookup_invoice`, `list_transactions`, `get_budget`;
- `get_balance`: 500,002,000 msat on both;
- `list_transactions`: 17 records before payment, 21 records after both test payments;
- signet `pay_invoice`: 1,000 msat, succeeded, `fees_paid = 0`;
- post-payment history showed settled outgoing and incoming 1,000 msat records.

Policy-layer checks:

- a local `cdk-nwc-policy` connection was generated in front of one signet connection;
- policy: budget 10,000 msat, single cap 5,000 msat, fee reserve 1,000 msat;
- allowed methods: `get_info`, `get_balance`, `make_invoice`, `pay_invoice`;
- `get_info` returned only the four allowed methods;
- `get_balance` forwarded 500,002,000 msat;
- `list_transactions` was rejected with `Restricted`;
- `make_invoice` + `pay_invoice` succeeded for 1,000 msat;
- final ledger: `used_msat = 1000`, 5 recorded events, 0 open reservations.

This proves transport compatibility, real signet payment behavior, method filtering and budget accounting against a live Cashu-backed NWC service. It does not exercise CWC or our local wallet backend, which remain covered by their own unit/e2e suites.

## Mutinynet deployment and back-and-forth validation

A fresh CDK wallet was deployed against `https://cashu.mutinynet.com`, funded with 5,000 sat through `mutinynet-cli`, and fronted by our own `cdk-nwc-policy` connection. See [MUTINYNET_LIVE_EVIDENCE.md](MUTINYNET_LIVE_EVIDENCE.md) for full details.

Verified:

- real faucet funding through `mutinynet-cli`;
- our own NWC connection backed by the CDK wallet;
- discovery, balance, invoice creation, lookup, payment and history;
- method filtering, quota enforcement, reservation cleanup, revocation and expiry;
- self-payment settlement inside our own CDK stack.

The run also found two interoperability failures on the Alby Cashu path: incoming invoices remained `pending` after externally observed payment, and outbound payment failed with `no preimage in melt response`. These are documented, not hidden.

## Not run here

- `cdk-postgres` tests: require a running PostgreSQL instance.
- Full `cdk-integration-tests`: require regtest/Nostr relay infrastructure.
- `just quick-check`: blocked locally because `nixpkgs-fmt` is not installed.
- Relay-layer replay forging: duplicate-event rejection is covered by unit/e2e suites; a signed event with the same ID was not replayed during this live run.
