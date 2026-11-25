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

## Not run here

- `cdk-postgres` tests: require a running PostgreSQL instance.
- Full `cdk-integration-tests`: require regtest/Nostr relay infrastructure.
- `just quick-check`: blocked locally because `nixpkgs-fmt` is not installed.
- Live Mutinynet/Alby test: ignored by default and intentionally not run in this environment.
