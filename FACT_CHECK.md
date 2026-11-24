# Fact-check record: cdk-nwc-policy

Scope checked: local worktree `/root/cdk-policy-build`, branch
`feat/nwc-policy-module`, final commit after the audit fixes.

## Verdict

The work is well tailored to the agreed recommendation: a small policy module
on top of CDK's NWC service, not a daemon, not a hosted service, not a new
wallet engine. It is a verified prototype, not a complete end-user product.

## Re-verified by execution

- `cargo test -p cdk-nwc-policy -p cdk-nostr --features cdk-nostr/nwc --no-fail-fast`:
  18 cdk-nostr tests, 9 handler tests, 14 store tests, 4 manager tests.
  45 passed, 0 failed.
- `cargo test -p cdk-integration-tests --test nwc_policy_e2e`: 3 passed,
  0 failed. Real `WalletNwcHandler`, in-memory mint, in-process Nostr relay,
  standard NWC client, manager `start_all`, manager-driven revocation, and
  upstream forwarding through a real NWC client.
- Live Mutinynet/Alby harness passed: real Alby balance, real Mutinynet mint
  quote, real payment, quote state `paid`, and balance decrease. Alby's mint
  backend did not return a preimage, so the test verifies payment through mint
  quote state rather than NWC preimage.
- `cargo clippy -p cdk-nwc-policy -p cdk-nostr --all-targets --features cdk-nostr/nwc -- -D warnings`:
  clean.
- Context propagation reaches all six NWC methods through the new
  `*_with_context` defaults (`crates/cdk-nostr/src/nwc/handler.rs`,
  `crates/cdk-nostr/src/nwc/service.rs`).
- Policy time uses `RequestContext.received_at`, sourced from the service
  clock, not the client event timestamp.
- `get_info` methods are filtered to the connection allowlist.

## Corrections applied after the independent audit

- Payment budgets now use the BOLT11 invoice amount. A client-supplied
  `amount` is accepted only when it matches the invoice. Amountless request
  fields now work when the invoice carries an amount, and amountless invoices
  are rejected before payment.
- The ledger holds an exclusive process-level file lock. A second store on the
  same path fails closed, preventing two in-memory snapshots from authorizing
  the same event.
- Connection IDs and client keys are validated on load as well as on insert.
- Ledger mutations are written to disk before in-memory state is swapped. A
  persist failure no longer burns the event or consumes the budget.
- Each authorization records its budget period. A reservation from an old
  period can no longer refund the new period after rollover.
- Fee overage is recorded in `fee_overage_msat` instead of being silently
  ignored. The budget charges only up to the reservation because the payment
  has already happened.
- NWC request freshness is now checked service-side, not only by the relay
  `since` filter.

## Why this shape fits

NIP-47 explicitly allows per-key constraints such as budgets, which is the
capability this module adds.[1] The crate does not perform mint or melt
operations, does not run relays, and does not introduce CWC, multi-mint,
mainnet funds or a hosted service (`crates/cdk-nwc-policy/Cargo.toml`,
`crates/cdk-nwc-policy/README.md`).

## Residual limits, stated honestly

- One `NwcService` instance still accepts one client public key. The manager
  starts one service per connection; it does not change that service-level
  constraint.
- The policy wrapper must be used through the context-aware dispatch. Its
  plain `NwcRequestHandler` methods fail closed.
- Failed wallet calls release the reservation but leave the event ID recorded.
  A retry must use a fresh Nostr event.
- The replay ledger is an unbounded list and is rewritten in full on each
  mutation. A bounded TTL store is still needed for long-lived deployments.
- Fee overage is recorded, not blocked, because fees are known only after the
  payment has been attempted.
- Inherited `cdk-nostr` issues remain outside this crate: the service retains
  the client secret for URI generation, request encryption tags are not
  strictly enforced, and the info event advertises `list_transactions`
  without an extensions tag. The manager ledger still persists client and
  service secrets needed to rebuild services; files are mode `0600` on Unix.
  Revocation denies new requests and stops idle services, but cannot recall an
  in-flight wallet operation.
- Host clock hardening is deferred. A trusted host clock is assumed.

## Sources

[1] https://raw.githubusercontent.com/nostr-protocol/nips/master/47.md — NIP-47 Nostr Wallet Connect
    > "Keys can be revoked and created at will and have arbitrary constraints (eg. budgets)."
