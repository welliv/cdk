# Per-key Cashu pockets for Routstr

Every API key gets its own Cashu pocket: a budget, an allowed-model list, a per-request cap and an expiry. A local **key proxy** (`nwc-policy-cli key proxy`) pays routstr-core **per request** (X-Cashu) from that key's pocket, and the provider's change goes back into the same pocket. The mint keeps pockets apart, so a leaked key can spend only its own pocket; revoking it sweeps the rest home.

**Status:** signet prototype (Mutinynet mint, test sats, a stub LLM behind a real routstr-core). Not reviewed, not for real funds yet. Built Nov 26–27, 2026 for Nut November.

| Repo | Branch | Commits |
|---|---|---|
| [welliv/cdk](https://github.com/welliv/cdk/tree/per-key-pockets) | `per-key-pockets` | `21c0f6b2` → `904b746d` → `5881bcc8` → `9fcf56b6` → `12e1cfb1` (+ this doc) |
| [welliv/routstrd](https://github.com/welliv/routstrd/tree/per-key-pockets) | `per-key-pockets` | `74cc2fc` (xcashu mode + pocket WalletAdapter) |

Quick start:

```sh
W="--mint-url <mint> --wallet-db ./wallet.sqlite"        # seed from CDK_NWC_SEED_HEX
nwc-policy-cli key create $W --label agent --relay <relay> --budget 300 \
  --max-per-payment 60 --models gpt-4o-mini --expires 7d --api-key-out agent.key
nwc-policy-cli key proxy  $W --provider http://<routstr-node>   # OpenAI-compatible on 127.0.0.1:8090/v1
nwc-policy-cli key revoke $W --id <id>                          # key refused, pocket swept home
```

## How a request is paid

1. **Key checks** run first; no mint call is made and no ecash leaves the pocket:
   - revoked or expired → 401
   - model not on the key's list → 403 (the message names the allowed models)
   - model's `max_cost` above the per-request cap → 402
   - unknown model → 404
2. **Pick ecash with no swap.**
   - First choice is an exact **bundle**: for price 45 that is `[32,8,4,2]` = 45 + the 1 sat input fee.
   - Otherwise the proxy overpays with the smallest set of loose proofs worth at most the cap. The provider returns the extra as change.
   - Only if neither works does it make one re-bundle swap, which cuts up to 6 bundles.
3. **Forward** the request with `X-Cashu`. Streaming (SSE) passes straight through.
4. **Change** goes back into the same pocket in one swap. That swap is shaped so the change plus the loose proofs make up the next bundle.
   - If the provider refuses and hands our own token back, the proofs are reused as they are: no swap, cost 0.
5. **Refill**: when the pocket can't cover a request, it refills from main, bounded by the refill cap. The refill swap is also cut into bundles.
6. **Mint 429, 5xx or network error**:
   - A shared backoff starts: 2 → 4 → … → 32 s.
   - While it lasts, only offline sends run. Change is parked on disk and the sweeper retries it later; there are no retry loops that burn fees.
   - If nothing can be sent offline, the client gets 503 `mint_busy` with `Retry-After`, and no ecash is spent.
7. **Locks**: `serve`, `key proxy`, `sweep`, `refill` and `topup` share cross-process spend locks (pocket → main). The locks are held only while choosing and receiving proofs, never during the LLM call.
8. **Revoke** sweeps the pocket home. It also swaps away tokens already handed out but not yet redeemed (for example from `/v1/pocket/send`).

Every response carries `x-pocket-{price,sent,change,cost,balance}-sat` and `x-pocket-change: received|parked|returned|none`. Every refusal ends with "No ecash was spent".

**routstrd per request (new, `mode: "xcashu"` + `pocket: {url, apiKeyFile}`)**
- The SDK's existing xcashu flow is used, with a WalletAdapter that sends `sendToken` and `receiveToken` to the proxy (`/v1/pocket/send|receive`, carrying the model).
- `GET /v1/pocket?model=` lets routstrd show the key's own refusal before paying.
- routstrd then holds **no float**, so revoking the key stops it immediately.

## Live tests on signet (Mutinynet mint, real routstr-core, Hermes Agent via `hermes model` → custom endpoint)

### (a) Hermes → key proxy → routstr-core
Key `hermes-direct`: pocket 300, cap 60, refill up to 200 per day, models gpt-4o-mini and gpt-4o.

| Check | Result |
|---|---|
| Chat (Hermes streams) | `echo: …`. Each request cost 3 sat (provider 2 + mint fee 1). **1 send swap in 87 requests** |
| gpt-4o (739 sat > cap 60) | Hermes shows `HTTP 402: pocket key 'hermes-direct': … above this key's 60 sat per-request cap. No ecash was spent`. Balance unchanged |
| Disallowed model (`hermes-routstrd` key) | `HTTP 403: … model 'gpt-4o' is not allowed for this key (allowed: gpt-4o-mini)` |
| Real Mutinynet 429s and provider 503s | Change parked; backoff 2 → 32 s; 503 `mint_busy` with nothing spent; **all 4 parked tokens landed later, one of them after a proxy restart** |
| Refill | Triggered below one bundle: +199 sat. Main went 2777 → 2577; the refill cap then reads 0 |
| Revoke | Hermes shows `HTTP 401: … this key was revoked; its pocket was swept home`. 256 sat swept, main 2577 → 2833 |
| Books | First 56 requests: 300 − (2583 sent − 2242 change − 174 parked back) = **133 = pocket balance exactly** |

### (b) Hermes → routstrd (apikeys mode, funded over NWC earlier; see SIGNET-TEST.md)
- The chat works. It is paid from **routstrd's own float**: 4117 sat in its wallet plus 70 sat on the provider key. The pocket isn't touched per request.
- **Finding:** revoking the NWC link stops refills but **not** spending of the float that's already there. This is why (c) exists.

### (c) Hermes → routstrd (xcashu) → key proxy pocket, per request
Key `hermes-routstrd`: pocket 200, cap 60, gpt-4o-mini only.

| Check | Result |
|---|---|
| Chat | Works. Pocket 200 → 188 after 4 chats (3 sat each). **routstrd's own wallet stays at 4117** |
| gpt-4o | Hermes shows `HTTP 403: pocket key 'hermes-routstrd': model 'gpt-4o' is not allowed …`. Before this fix, routstrd's generic "insufficient balance" came through instead |
| Revoke | Hermes shows `HTTP 401: … revoked`. 185 swept, main 2592 → 2777 |
| Books | 200 = 4×2 provider + 4 fees + 188; 188 − 3 sweep fee = 185 into main |

## Tests
- **cdk unit tests (18):**
  - key rules and their mapping to OpenAI-style errors
  - raw-token rules
  - bundles
  - change completing the next bundle
  - choosing the overpay set within the cap
- **`key_proxy.rs` integration tests (3):** in-memory mint at 100 ppk plus a fake routstr-core. They cover:
  - 401 for no key or a forged key
  - model filtering, and `/v1/pocket?model=` refusals
  - pay and change; swap-free requests; streaming
  - provider handing the token back costs 0
  - errors 402 / 403 / 404 / 401
  - `/v1/pocket/send|receive` rules
  - revoke makes outstanding tokens worthless
  - budget and refill caps; isolation between keys
  - provider revenue is exact
  - **books: end = issued − redeemed**, with no PendingSpent left
  - the spend lock is exclusive across handles
- **Other suites:** nwc_pocket 9, policy_claim 2 and policy_e2e 4 all pass. Clippy is clean.
- **routstrd:** 3 adapter tests; the 732 daemon tests pass; tsc is clean.

## Critique from first principles
- **The proxy holds the main seed.** It is a hot wallet. Run it on the user's machine, not on shared infrastructure.
- **Change depends on trusting the provider.** Worst case per request equals the cap. Overpay never exceeds the cap, and a returned token is only reused with the same provider, so this adds no new exposure.
- **`/v1/pocket/send` hands out raw ecash.** For that path the model allow-list relies on the model the client declares (routstrd). Each token is still capped, the total is bounded at the mint, and revoke reclaims anything not yet redeemed.
- **Fragmentation:** overpay change can leave dust. A pocket can end up refused while it holds about one bundle in tiny proofs, because the fee makes it worth less than the price. That is correct but slightly wasteful; the sweep recovers it.
- **Mutinynet rate limits hard** (about 1 request every 2–4 s from one IP). Each request needs two mint operations (the provider's redeem and our change swap), and that is the real throughput limit. The design degrades safely.
- **Gaps:**
  - No simulated-429 test in CI (the `MintConnector` trait is ~22 methods); the 429 behaviour was proven live only.
  - Main-wallet saga recovery logged `failed: 5` when it ran during a 429 storm; it retries on the next start.
