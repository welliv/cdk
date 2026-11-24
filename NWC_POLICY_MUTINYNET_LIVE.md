# Mutinynet live test

Ignored test: `crates/cdk-integration-tests/tests/nwc_policy_mutinynet_live.rs`

Prerequisite: an Alby Hub NWC connection URI stored in a `0600` file. Create
the connection inside Alby Hub and write the URI directly to that file. Do not
put the URI in chat or in shell history.

Run:

```bash
ALBY_NWC_URI_FILE=/path/to/alby-nwc.uri \
  cargo test -p cdk-integration-tests \
  --test nwc_policy_mutinynet_live -- --ignored --nocapture
```

What it verifies:

- the policy service fronts the real Alby NWC connection;
- `get_info` is filtered to the three allowed methods;
- `get_balance` returns a positive balance from the Alby wallet;
- a fresh mint quote is created at `https://cashu.mutinynet.com/`;
- the invoice is paid through the policy layer using the Alby wallet;
- the wallet balance decreases;
- revocation denies a fresh request.

Observed Alby mint-backend behavior: the payment succeeds and the Mutinynet
mint quote becomes `paid`, but the NWC response has no preimage, so the client
reports an error. The live test therefore verifies payment by querying the
mint quote state and checking the wallet balance, not by relying on the
preimage.

The policy layer treats payment errors as ambiguous and keeps the reserved
budget charged instead of releasing it. Failing closed prevents a successful
payment from being undercounted.

This is a test-network flow. It uses Mutinynet funds, not mainnet funds.
