# How NWC for Cashu works in the wild

A plain-language walkthrough for users, builders, and judges.

## The 30-second version

A Cashu wallet can already pay and receive Lightning invoices. NWC is a standard way for apps to ask a wallet to do those things over Nostr.

This project puts a policy gate between the app and the Cashu wallet.

The app gets a normal NWC URI. The wallet gives it only the methods, budget, and lifetime the user chose. The app never receives ecash tokens.

## The cast

```text
You
  ├── Cashu wallet (CDK)
  ├── NWC wallet service
  └── policy ledger

App / agent
  └── NWC client

Nostr relay
  └── carries encrypted messages

Cashu mint / Lightning
  └── settles the money
```

The relay carries encrypted messages. It does not see the invoice, amount, or response content.

## 1. You create a connection

You create a connection for an app and choose:

- allowed methods;
- total budget;
- single-payment cap;
- fee reserve;
- expiry date.

The wallet gives you a `nostr+walletconnect://` URI.

That URI is like a scoped capability. It authorizes one client key. It is not the wallet seed and it is not a raw ecash token.

You paste the URI into the app.

## 2. The app asks what is available

The app reads the wallet's info event.

It sees something like:

```text
get_info get_balance make_invoice pay_invoice
```

If the app was not allowed to list transactions, that method is not advertised. This prevents capability confusion.

## 3. The app checks your balance

The app sends an encrypted request:

```json
{
  "method": "get_balance",
  "params": {}
}
```

The wallet service:

1. checks the client key;
2. checks the request is fresh;
3. checks `get_balance` is allowed;
4. reads the CDK wallet balance;
5. converts it to millisatoshis;
6. replies encrypted.

The app sees a balance. It does not see which mint holds the funds.

## 4. The app pays a Lightning invoice

The app sends:

```json
{
  "method": "pay_invoice",
  "params": {
    "invoice": "lnbc..."
  }
}
```

Inside the wallet service:

1. decrypt the request;
2. verify the authorized client;
3. parse the invoice and take the amount from the invoice itself;
4. check the single-payment cap;
5. check the remaining budget;
6. reserve amount plus fee reserve;
7. create a Cashu melt quote;
8. melt the required ecash proofs;
9. pay the Lightning invoice;
10. commit actual amount and fees to the ledger;
11. return the preimage to the app.

The app only sees a successful payment response. It never sees the proofs that were spent.

## 5. The app creates an invoice

The app sends:

```json
{
  "method": "make_invoice",
  "params": {
    "amount": 10000,
    "description": "Tip"
  }
}
```

CDK creates a Cashu mint quote. The app shows the Lightning invoice to the user. When it is paid, new ecash proofs are issued into the user's wallet.

Again, the app never receives those proofs.

## 6. The budget is real

Before a payment, the budget is reserved. If payment succeeds, the actual amount and fees are charged. If failure is ambiguous, the reservation stays charged, because the Lightning payment may have gone through.

That last rule prevents a common retry exploit: claiming failure, then asking again to get a second payout.

## 7. Replay is blocked

Every request event ID is recorded in a persistent ledger.

If a relay redelivers an old event, or an attacker replays a captured request after restart, the wallet rejects it.

A duplicate request cannot become a duplicate payment.

## 8. You can revoke the app

If an app is compromised or no longer trusted, you revoke its connection.

New requests fail immediately. The app cannot spend further.

## Example in real life

You use a Nostr client that supports NWC.

You create a connection with:

- budget: 50,000 msat;
- cap per payment: 10,000 msat;
- methods: `get_info`, `get_balance`, `make_invoice`, `pay_invoice`.

The client can zap people and check its allowance. It cannot read your full transaction history because that method was not allowed. It cannot receive raw ecash. It cannot exceed its budget. When you are done, you revoke it.

That is the practical change: **a useful integration without handing over the wallet**.

## What this does not do

This NWC build is a compatibility bridge, not a full Cashu-native protocol.

It does not expose:

- raw Cashu token transfer;
- NUT-18 payment requests;
- app-level mint selection;
- multi-mint balance details;
- offline ecash handoff.

Those belong to Cashu-native capabilities such as CWC.

## Why this is the right first step

Apps already speak NWC. Users already want Cashu wallets to work with those apps.

This project makes that possible without weakening Cashu's most important property: ecash is bearer money, and bearer money should not be casually handed to applications.

The result is boring, useful infrastructure: a normal wallet interface with real guardrails underneath.
