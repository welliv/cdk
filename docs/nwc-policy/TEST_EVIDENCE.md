# Test evidence

## Commands

```bash
cargo fmt --all -- --check
cargo test -p cdk-nostr -p cdk-nwc-policy -p cdk-nwc-forward -p cdk-nwc-policy-cli --no-fail-fast
```

Results: format check passed; 45 tests passed, 0 failed.

## Live checks

- Signet Alby Cashu NWC: discovery, balance, history and a 1,000 msat payment were observed through the policy layer.
- Mutinynet CDK wallet: funded with 5,000 sat via `mutinynet-cli`, then exposed through our own NWC connection.
- Verified on our own stack: `get_info`, `get_balance`, `make_invoice`, `lookup_invoice`, `list_transactions`, self-payment settlement.
- Verified policy behavior: method filtering, quota rejection, reservation cleanup, revocation and expiry.
- The two Alby NWC connections used in testing are separate connections to the same wallet; balance comparisons between them are not independent proof.
- Alby Cashu interoperability was not demonstrated: incoming lookups stayed `pending`, and outbound attempts failed with `no preimage in melt response` or `outputs are pending [Internal]` with an unpaid mint quote. The ignored live harness failed on repeated runs. We do not claim universal Alby Cashu interoperability.

## Not run here

- PostgreSQL-dependent tests.
- Full regtest integration suite.
- `just quick-check` (`nixpkgs-fmt` unavailable locally).
- Live relay-layer replay forging; duplicate-event rejection is covered by unit/e2e tests.

No connection secrets are recorded here.
