# November delta

## Baseline used

This work builds on upstream CDK:

- `cdk-nostr` NIP-47 wallet service and handler interface.
- `cdk/src/wallet/nwc.rs` mapping NWC methods to Cashu wallet operations.
- CDK wallet, mint quote, melt quote and proof storage primitives.

## November work

The clean submission commit is `73bfce1` (amended from the original local commit `690ed10`).

Added:

- `crates/cdk-nwc-policy/`
  - persistent policy ledger;
  - per-connection budgets, caps and fee reserves;
  - revocation, expiry and replay protection;
  - redacted connection views.
- `crates/cdk-nwc-forward/`
  - upstream NWC forwarding handler.
- `crates/cdk-nwc-policy-cli/`
  - `create`, `list`, `revoke`, `serve`;
  - least-privilege defaults;
  - fee-reserve validation.
- Context-aware methods in `cdk-nostr/src/nwc/handler.rs` and `service.rs`.
- Integration tests:
  - `nwc_policy_e2e.rs`;
  - `nwc_policy_mutinynet_live.rs`.
- Documentation:
  - `NWC_POLICY_MANAGER_CLI.md`;
  - `NWC_POLICY_E2E.md`;
  - `NWC_POLICY_MUTINYNET_LIVE.md`;
  - `FACT_CHECK.md`;
  - `FINAL_AUDIT.md`;
  - `CASHU_NATIVE_CAPABILITY_DESIGN.md`.

Removed from the submission:

- the direct `CHANGELOG.md` edit, because CDK requires changelog notes in PR descriptions;
- all CWC work, which is a separate project.
