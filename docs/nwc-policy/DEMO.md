# Demo and reproduction

This document shows how to verify the NWC policy layer without exposing credentials.

## Local test suite

```bash
cargo fmt --all -- --check

cargo test \
  -p cdk-nostr \
  -p cdk-nwc-policy \
  -p cdk-nwc-forward \
  -p cdk-nwc-policy-cli \
  --no-fail-fast
```

Expected: 45 tests pass.

## Create a local connection

Use your own seed and mint values. Do not commit them.

```bash
export CDK_NWC_SEED_HEX=<your-64-byte-hex-seed>

cargo run -p cdk-nwc-policy-cli -- create \
  --name demo-app \
  --relay wss://relay.example.com \
  --mint-url https://mint.example.com \
  --wallet-db ./wallet.db \
  --budget-msat 10000 \
  --single-cap-msat 5000 \
  --fee-reserve-msat 1000 \
  --methods get_info,get_balance,make_invoice,pay_invoice
```

The command prints:

- a connection ID;
- a `nostr+walletconnect://` URI.

Paste that URI into any NWC-compatible app.

## Inspect and revoke

```bash
cargo run -p cdk-nwc-policy-cli -- list

cargo run -p cdk-nwc-policy-cli -- revoke --id <connection-id>
```

`list` is redacted. It does not print client secrets.

## Serve connections

```bash
cargo run -p cdk-nwc-policy-cli -- serve
```

The service:

- listens for NWC requests on the configured relays;
- publishes the info event;
- checks every request against policy;
- forwards allowed requests to the CDK Cashu wallet.

## Upstream forwarding mode

The same policy layer can front another NWC service:

```bash
cargo run -p cdk-nwc-policy-cli -- \
  --upstream-nwc-uri-file ./upstream.uri \
  serve
```

This is how the live signet validation was performed: our policy layer sat in front of a real Alby Hub Cashu NWC connection.

## Observed live signet behavior

- Both Alby Hub signet connections advertised `NWC (Cashu)` on network `signet`.
- Balance was `500002000 msat`.
- A 1,000 msat invoice was created and paid successfully.
- Our policy layer filtered methods correctly.
- A non-allowlisted `list_transactions` call returned `Restricted`.
- The policy ledger recorded exactly 1,000 msat used.
- No reservations remained open.

Secrets were stored in `0600` files and are not reproduced here.

## What to look for as a judge

1. Does `get_info` show only allowed methods?
2. Does `get_balance` return a balance in msat?
3. Does a disallowed method return `Restricted`?
4. Does a payment charge the ledger?
5. Does revocation stop new requests?

These are the observable behaviors that matter.
