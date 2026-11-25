# What was built in November

Nut November submission notes. All times are UTC, from `git log`.

## Before 1 November (upstream CDK, not ours)

The work builds on upstream [cashubtc/cdk](https://github.com/cashubtc/cdk):

- `crates/cdk/src/wallet/nwc.rs` — `WalletNwcHandler`, mapping NIP-47
  methods to Cashu wallet operations. Added upstream in `10cab2ab`
  (2026-06-24, "feat(nwc): add NIP-47 wallet service support").
- `crates/cdk-nostr` — NIP-47 wallet service (`NwcService`) and the
  `NwcRequestHandler` trait. Added upstream in `530d3a37` (2026-08-14).
- The CDK wallet, mint, quote, melt, swap and proof-storage code.

The last upstream `main` commit before 1 November is `8006cc30`
(2026-10-29 13:17). This branch was later synced with upstream `main` at
`c1105a09` (2026-11-24 20:34, merged in `c3e1fe38` on 2026-11-25), so the
diff against our base is `git diff c1105a09 HEAD`. Every non-merge commit in
`c1105a09..HEAD` is ours; upstream November commits by other CDK
contributors came in only through that merge and are not part of this
submission.

## Built in November (this submission)

Range: `73bfce13^..HEAD` on the submission branch, first commit
2026-11-24 17:35. Against `c1105a09` at `e6b33d28`: 55 files changed, 9,987 lines added,
50 removed (the docs commit adds a few more).

| Commit | Date (UTC) | Change |
|---|---|---|
| `73bfce13` | 2026-11-24 17:35 | feat(nwc-policy): add per-connection policy enforcement |
| `02c128af` | 2026-11-25 11:21 | docs(nwc-policy): add Nut November submission package |
| `91a22574` | 2026-11-25 11:21 | ci(nwc-policy): add focused checks |
| `bc90b932` | 2026-11-25 17:11 | docs(nwc-policy): record live signet validation |
| `c23d147b` | 2026-11-25 17:13 | docs(nwc-policy): add submission banner to README |
| `38708522` | 2026-11-25 17:22 | docs(nwc-policy): leave category choice to judges |
| `6e5e4160` | 2026-11-25 17:29 | docs(nwc-policy): make repo explain the idea clearly |
| `bdfbe3c2` | 2026-11-25 18:01 | docs(nwc-policy): record Mutinynet live validation |
| `6f6e548c` | 2026-11-25 18:13 | docs(nwc-policy): correct live interoperability claim |
| `3cf0f492` | 2026-11-25 18:22 | docs(nwc-policy): correct shared-balance interpretation |
| `3e626dab` | 2026-11-25 18:26 | docs: align repo with CDK contribution style |
| `ee7f6837` | 2026-11-25 19:45 | nwc-policy: fix rollover accounting, release on insufficient balance, claim paid invoices, CLI period/expiry flags, keep upstream error codes |
| `3a5600c1` | 2026-11-25 20:08 | nwc: fix input fees, preimage verification, per-connection info methods, bounded replay ledger, 0600 wallet DB, and reservation expiry |
| `d087a065` | 2026-11-25 20:49 | nwc: time out stalled mint HTTP so make_invoice cannot hang forever |
| `1a995996` | 2026-11-25 21:28 | nwc: start revoked/expired connections so they reply UNAUTHORIZED; distinguish payment cap from period budget |
| `fb9f5510` | 2026-11-25 22:13 | feat(nwc-pocket): add per-connection Cashu pockets for NWC links |
| `c0380c60` | 2026-11-25 22:13 | test(nwc-pocket): cover isolation, revoke sweep, refill window and fees |
| `84a6226f` | 2026-11-25 22:27 | feat(nwc-pocket): add rogue-agent demo example |
| `990664f4` | 2026-11-25 22:35 | feat(nwc-policy): run a connection against its own inner handler |
| `dcca8bc6` | 2026-11-25 22:35 | feat(nwc-policy-cli): add pocket mode |
| `e6b33d28` | 2026-11-25 22:35 | test(nwc-pocket): cover serve maintenance and policy over a pocket |

Docs commits after `e6b33d28` (this file, `NWC.md`, crate READMEs) are also
November work.

### By component

- **`cdk-nwc-pocket` (new crate, 25 Nov):** per-connection Cashu pockets
  with derived seeds, same-mint funding and sweep, a rolling-window refill
  cap with auto-refill, ceiling, pocket-aware balance, measured fees and an
  out-of-funds event; rogue-agent demo example. Commits `fb9f5510`,
  `c0380c60`, `84a6226f`.
- **`cdk-nwc-policy` (new crate, 24–25 Nov):** persistent policy ledger,
  per-connection method allowlists, budgets, single-payment caps, fee
  reserves, expiry, revoke, replay protection, reservations
  (pending-as-spent), connection manager; later `with_inner` /
  `start_with_inner` to serve a connection from its own pocket. Commits
  `73bfce13`, `ee7f6837`, `3a5600c1`, `1a995996`, `990664f4`.
- **`cdk-nwc-policy-cli` (new crate):** `create`, `list`, `revoke`, `serve`;
  pocket mode with `fund`, `topup`, `sweep`, `pockets`. Commits `73bfce13`,
  `ee7f6837`, `3a5600c1`, `d087a065`, `dcca8bc6`.
- **`cdk-nwc-forward` (new crate):** forward NWC to an upstream wallet
  (tested against Alby Hub on Mutinynet). Commits `73bfce13`, `ee7f6837`.
- **Changes to upstream crates:**
  - `cdk-nostr`: `RequestContext` and `*_with_context` handler methods,
    per-connection advertised methods (`73bfce13`, `3a5600c1`);
  - `cdk/src/wallet/nwc.rs`: `fees_paid` includes swap input fees, and
    preimages are checked against the melt quote status (`3a5600c1`,
    `d087a065`);
  - `cdk-http-client` and the mint HTTP connector: request timeouts so a
    stalled mint cannot hang `make_invoice` (`d087a065`).
- **Tests:** `cdk-nwc-policy/tests/*`, `cdk-integration-tests/tests/nwc_policy_e2e.rs`,
  `nwc_policy_claim.rs`, `nwc_policy_mutinynet_live.rs`, `nwc_pocket.rs`.
- **Docs and CI:** `NWC.md`, `NWC_POLICY_*.md`, `docs/nwc-policy/`,
  `.github/workflows/nwc-policy-ci.yml`.

## Verify

```bash
git log --no-merges --format='%h %ad %an %s' --date=iso c1105a09..HEAD
git diff --stat c1105a09 HEAD
git log -1 --format='%h %ad %s' --date=iso 8006cc30   # last upstream main commit before Nov 1
```
