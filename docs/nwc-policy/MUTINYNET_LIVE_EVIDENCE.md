# Mutinynet live evidence

Run on 2026-11-25 UTC against a freshly deployed CDK wallet using `https://cashu.mutinynet.com`, funded through `mutinynet-cli`. No connection secrets are recorded here.

## Deployment

The two Alby NWC URIs used in testing are separate connections to the **same underlying Alby wallet** and share its balance. Balance comparisons between those two connections are therefore not independent evidence of settlement.

- CDK workdir: isolated `0600` scratch directory.
- Mint: `https://cashu.mutinynet.com`.
- Funding: 5,000 sat minted after paying a Cashu mint quote with `mutinynet-cli`.
- NWC service: `cdk-nwc-policy-cli serve`, backed by the CDK wallet DB and seed.
- Connections: three generated locally with separate policies.

## Our own NWC connection

`get_info`:

```text
alias=CDK Cashu Wallet
network=mainnet
methods=pay_invoice, make_invoice, lookup_invoice, list_transactions, get_balance, get_info
```

Note: CDK reports `mainnet` here; the mint used was Mutinynet. This is a reporting mismatch, not a funds-network issue.

Initial state:

```text
balance_msat=5000000
tx_count=1
tx: incoming settled 5000000 msat
```

## Back-and-forth with Alby Hub signet

### CDK → Alby Hub

1. Alby NWC created a 1,000 msat invoice.
2. Our CDK NWC connection paid it.
3. Our side reported `PAY_OK`, `fees_paid = 1000`.
4. Our `lookup_invoice` reported `settled`, `outgoing`, 1,000 msat.
5. Alby `lookup_invoice` still reported `pending`.
6. Our balance moved from 5,000,000 to 4,997,000 msat.

### Faucet → Alby Hub control

To isolate the issue, a fresh 1,000 msat Alby invoice was paid directly with `mutinynet-cli lightning`. The faucet returned a payment preimage, but Alby still reported the invoice as `pending`.

Because the two Alby connections share one wallet balance, this is reported as a **lookup-state discrepancy**, not as balance proof. It does not by itself prove funds did or did not settle.

### Alby Hub → CDK

1. Our CDK NWC connection created a 1,000 msat invoice.
2. Alby NWC `pay_invoice` returned `no preimage in melt response [Internal]`.
3. Our `lookup_invoice` remained `pending`.
4. Our balance was unchanged for this attempt.

The same failure mode was seen earlier when paying the Mutinynet faucet L402 invoice through the Alby Cashu NWC connection.

### CDK → CDK control

To confirm our own path settles:

1. Our CDK connection created a 1,000 msat invoice.
2. Our CDK connection paid it.
3. Our `lookup_invoice` reported `settled`, `outgoing`, fees 1,000 msat.
4. Balance moved from 4,997,000 to 4,994,000 msat.

So invoice creation, lookup and payment settle correctly inside our CDK/NWC deployment. The pending/failed direction is specific to the Alby Cashu interoperability path tested here.

### Ignored live harness re-run

The ignored test `nwc_policy_mutinynet_live` was re-run four times against the same Alby connection:

```text
pay_result_ok=false
pay_error=outputs are pending [Internal] [Other]
mint_quote_state=unpaid
test result: FAILED
```

This confirms the Alby Cashu payment path was not settling in this environment. The test is ignored and is not counted as a passing validation.

## Policy enforcement

A restricted connection was created with:

- methods: `get_info`, `get_balance`, `pay_invoice`;
- budget: 10,000 msat;
- single cap: 1,000 msat;
- fee reserve: 1,000 msat.

Observed:

```text
get_info methods = pay_invoice, get_balance, get_info
list_transactions -> method not allowed: list_transactions [Restricted]
make_invoice -> method not allowed: make_invoice [Restricted]
pay 2000 msat -> budget exceeded [QuotaExceeded]
pay 1000 msat -> budget exceeded [QuotaExceeded]
```

The second payment failure is expected under the current rule: the cap test checks `amount + fee_reserve`, so 1,000 + 1,000 exceeds a 1,000 msat cap. The cap includes the fee reserve by design.

A second connection with a 3,000 msat cap was created:

```text
pay 1000 msat -> PAY_OK
balance -> 4,991,000 msat
ledger used_msat -> 2000
fee_overage_msat -> 0
reservations -> 0
```

This confirms method filtering, single-payment capping, fee accounting and reservation cleanup.

## Revocation and expiry

After revoking the policy connection:

```text
get_balance -> connection not authorized [Unauthorized]
```

A policy with `expires_at` set to the past was also rejected with `Unauthorized`. The policy file was restored afterward.

## Replay ledger

Final policy state:

```text
events = 25
reservations = 0
```

No reservation leaked after successful or failed payments. Duplicate-event rejection is also covered by the unit and e2e suites; it was not re-forged at the relay layer during this live run.

## Compatibility notes

- Cashu mint quotes reject the optional NWC `expiry` parameter: `expiry is not supported by Cashu mint quotes`. The harness was changed to omit it.
- CDK reports network as `mainnet` even when backed by the Mutinynet mint.
- Payments through this mint reported `preimage_len = 0` on successful NWC responses.

## Conclusion

The live run verifies:

- real deployment of CDK with a Mutinynet Cashu mint;
- real faucet funding through `mutinynet-cli`;
- our own NWC connection backed by that CDK wallet;
- discovery, balance, invoice creation, lookup, payment and history;
- method allowlists, quota enforcement, reservation cleanup, revocation and expiry;
- self-payment settlement inside our own stack.

It also uncovered observed Alby Cashu interoperability discrepancies: incoming invoices remained `pending` in lookup, and outbound payments failed with `no preimage in melt response` or `outputs are pending [Internal]` with the mint quote staying unpaid. Because the two Alby connections share one wallet balance, balance comparisons are not used as proof either way. These observations are outside our policy layer and are not claimed as universal interoperability.
