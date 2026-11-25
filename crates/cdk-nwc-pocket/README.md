# cdk-nwc-pocket

Per-connection Cashu "pockets" for Nostr Wallet Connect (NIP-47) links.

An NWC connection string is a bearer credential: whoever holds it can ask the
wallet to pay. Counting spend in a ledger in front of the main wallet only
helps while the ledger is right. With a pocket, the link is served by its
**own** Cashu wallet (own seed, own database), so it physically cannot spend
more than the ecash it holds.

## How it works

| Piece | What it does |
|---|---|
| `derive_pocket_seed` | `HMAC-SHA512(main_seed, "cdk-nwc-pocket-v1" \|\| index)`; pockets are recoverable from the main seed with NUT-13 restore. Indexes are never reused. |
| `transfer_to_pocket` | Funds a pocket with a same-mint send/receive (no Lightning). |
| `PocketNwcHandler` | `NwcRequestHandler` that can reach only the pocket. `get_balance` is the pocket balance; `fees_paid` is the measured drop in pocket balance minus the invoice. |
| `PocketRegistry` | File-backed (`0600`, file-locked) pocket configs and refill grants. |
| `Refiller` | Optional auto-refill, capped by a **rolling-window** refill cap enforced on the main-wallet side. Emits one `PocketEvent::OutOfFunds` event when the pocket is dry or the cap is reached. |
| `sweep_pocket` | On revoke/expiry, swaps every unspent pocket proof into the main wallet. Copies of the old proofs are then worthless. Pending proofs are left alone (pending-as-spent) and reported. |
| `sweep_excess` | Funds received above the pocket ceiling go to the main wallet. |

Replay protection, method allowlists, expiry and revoke stay in
`cdk-nwc-policy`; wrap a `PocketNwcHandler` in `PolicyNwcHandler` to get both.
Shared-balance mode (a policy ledger in front of the main wallet) remains
available as an explicit opt-in.

## Limits

- Same mint only; the pocket trusts the same mint as the main wallet.
- A pocket in the same process as the main wallet is isolated at the wallet
  level, not the OS level. Run the NWC service in its own process with only
  the pocket database if you need process isolation.
- Only the `sat` unit is supported.

## License

MIT or Apache-2.0, at your option, same as the rest of CDK.
