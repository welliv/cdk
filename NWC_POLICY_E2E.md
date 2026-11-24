# NWC policy end-to-end harness

Test: `crates/cdk-integration-tests/tests/nwc_policy_e2e.rs`

Command:

```bash
cargo test -p cdk-integration-tests --test nwc_policy_e2e
```

What it starts:

- an in-process Nostr relay using axum WebSockets;
- a pure in-memory Cashu mint with a fake lightning backend;
- a real CDK `WalletNwcHandler`;
- the `PolicyNwcHandler` wrapper;
- a real `NwcService`;
- a standard `nwc` client.

It also has a manager e2e test that creates a connection, starts all managed
services, pays through a standard NWC client, and revokes the connection.

What it verifies:

- `get_info` advertises only the three allowed methods;
- `get_balance` returns the funded wallet balance;
- `make_invoice` is rejected as restricted;
- `pay_invoice` succeeds through the policy layer and charges the budget;
- the service can be stopped and restarted from the same ledger;
- replaying the captured payment event after restart does not spend again;
- revoked connection can no longer call `get_balance`;
- manager-driven `start_all` and `revoke` work against the same stack.

Result on this host: `2 passed; 0 failed` in 32.67s.

No external relay binary, mainnet funds, or hosted service is used.
