# NWC policy manager and CLI

Crate: `crates/cdk-nwc-policy-cli`
Binary: `nwc-policy-cli`

The manager creates, persists, starts and revokes per-app NWC connections. The
CLI exposes those operations.

## Seed handling

Use the environment variable rather than a command-line flag:

```bash
export CDK_NWC_SEED_HEX=<128 hex chars>
```

`--seed-hex` still works but exposes the seed in shell history and process
listings.

## Backends

The policy layer can use either:

- a local CDK wallet (`--mint-url`, `--wallet-db`, seed), or
- an upstream NWC wallet via `--upstream-nwc-uri-file`.

For Alby Hub, use the upstream mode. Write the NWC URI to a `0600` file and
pass that file path. The URI never appears in the command line.

```bash
nwc-policy-cli --upstream-nwc-uri-file /path/to/alby-nwc.uri ...
```

## Commands

### Create a connection

```bash
nwc-policy-cli \
  --manager nwc-manager.json \
  --policy nwc-policy.json \
  create \
  --mint-url https://mint.example.com \
  --wallet-db wallet.db \
  --name "My app" \
  --relay wss://relay.example.com \
  --budget-msat 10000 \
  --single-cap-msat 8000 \
  --methods get_info,get_balance,pay_invoice \
  --fee-reserve-msat 100
```

It prints the `connection_id` and the `nostr+walletconnect://` URI. Give the
URI only to the app. Keep the connection ID for revocation.

In upstream mode, omit `--mint-url` and `--wallet-db`:

```bash
nwc-policy-cli --upstream-nwc-uri-file /path/to/alby-nwc.uri \
  --manager nwc-manager.json --policy nwc-policy.json \
  create --name "Alby app" --relay wss://relay.example.com \
  --budget-msat 10000 --single-cap-msat 8000 \
  --methods get_info,get_balance,pay_invoice --fee-reserve-msat 100
```

### List connections

```bash
nwc-policy-cli --manager nwc-manager.json --policy nwc-policy.json list
```

The output is redacted. It never prints client or service secrets.

### Revoke a connection

```bash
nwc-policy-cli --manager nwc-manager.json --policy nwc-policy.json revoke \
  --id <connection-id>
```

This works while `serve` is running. It denies all new requests and stops an
idle service. An in-flight wallet operation cannot be recalled.

### Serve all active connections

Local CDK wallet:

```bash
nwc-policy-cli --manager nwc-manager.json --policy nwc-policy.json serve \
  --mint-url https://mint.example.com --wallet-db wallet.db
```

Alby upstream:

```bash
nwc-policy-cli --upstream-nwc-uri-file /path/to/alby-nwc.uri \
  --manager nwc-manager.json --policy nwc-policy.json serve
```

`serve` starts one `NwcService` per active connection, reports the number
actually started, and stops them on Ctrl+C or error.

## Storage

- `nwc-manager.json` stores connection metadata and keys needed to rebuild
  services.
- `nwc-policy.json` stores budgets, method rules, replay IDs and reservations.
- Both files are created with mode `0600` on Unix.

## Verified

- `create` prints the connection ID and a valid NWC URI.
- `list` returns redacted records without wallet arguments.
- `revoke` works while `serve` is live.
- Manager and policy files are `0600`.
- Manager e2e test verifies `start_all` and manager-driven revocation with a
  real wallet, mint, relay and NWC client.
- Forwarding e2e test verifies an upstream NWC wallet can back the policy layer.
- Ignored live test `nwc_policy_mutinynet_live` exercises the policy layer
  against an upstream Alby NWC connection and `https://cashu.mutinynet.com/`.
