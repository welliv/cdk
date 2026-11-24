# Cashu-native capability layer: first-principles design note

Question explored: is a Cashu-native capability layer in scope, overreach, or the right next architecture?

## Short answer

Explore it, but do not build it yet.

The right architecture is a **protocol-agnostic policy core** with two frontends:

```text
Cashu wallet engine
        ↓
Policy core: budgets, method rules, expiry, replay, revocation, audit
        ↓
Frontend A: NWC (proven, existing apps)
Frontend B: Cashu-native capability API (future, only if validated)
```

This keeps NWC compatibility while leaving room for ecash-specific capabilities.

## Evidence base

NIP-47 defines core NWC and explicitly leaves more granular authorization models and extensions to separate specs.[1] It also says connection keys can have arbitrary constraints such as budgets.[1] The public NWC site describes NWC as an open protocol connecting Lightning wallets to apps.[33] Alby’s user guide frames app connections as having dedicated budgets and custom permissions.[35]

Cashu is a Chaumian ecash protocol for Bitcoin built around private bearer tokens.[11] Cashu’s FAQ warns that ecash tokens are bearer assets and that losing wallet storage loses the money itself.[10]

The NWC repository contains optional extensions intentionally kept out of the simplified core spec.[23] NWC-08 supports client-generated secrets, `max_amount`, renewal periods, requested methods and isolated sub-wallet connections.[4] NWC-13 defines `get_budget` as a way to query an existing budget, not to create or change one.[30] NWC wallet best practices recommend budgets as a safety measure against overspending.[41]

NUT-18 defines receiver-initiated Cashu payment requests with amount, unit, mint list, transport and locking conditions.[26] The draft Cashu Wallet Connect specification extends NIP-47 with native Cashu token operations, multi-mint support and NUT-18 payment requests.[27]

## First principles

### 1. The primitive is authorization, not payment

NWC is a transport and command protocol. Cashu is a bearer-token system.

The thing we actually care about is neither. It is the rule layer that decides:

- who may spend;
- how much;
- on what;
- until when;
- with what replay and audit guarantees.

That layer can sit above different command protocols.

### 2. NWC compatibility has real value

NWC already has SDKs, wallets and a large app ecosystem.[33] Alby’s UX already teaches users budgets and permissions.[35] NWC extensions are the sanctioned place for granular behavior.[1][23]

Throwing that away to build a Cashu-only protocol would sacrifice adoption for theoretical purity.

### 3. Cashu-native capabilities could express more

NWC is Lightning-shaped: invoice, balance, payment lookup. Cashu has mint choice, token transfer, bearer privacy and receiver-initiated NUT-18 requests.[26]

A Cashu-native frontend could expose capabilities NWC does not naturally express:

- send or receive ecash tokens;
- constrain acceptable mints;
- pay NUT-18 requests;
- issue scoped allowances while preserving mint privacy;
- give agents receipts without exposing proofs.

That is the real opportunity.

## What NWC already covers well

The current NWC extension system already covers much of what we built:

- connection-scoped keys and budgets are part of the NWC model;[1]
- client-initiated connection creation can request amount limits and method sets;[4]
- `get_budget` can expose an existing budget independently of wallet balance;[30]
- budgets are explicitly recommended as an overspend safety measure;[41]
- optional extensions are the formal path for extra capabilities.[23]

This means we should not invent a new protocol merely to have budgets.

## What NWC does not cover well

NWC core does not have first-class Cashu tokens, mint selection, NUT-18 requests or multi-mint balance semantics. The CWC draft exists exactly because these are native Cashu needs.[27]

CWC is still draft status, though.[27] That is a strong signal to learn from it, not to adopt it wholesale.

Cashu’s bearer-token nature also changes the threat model. The token itself is money.[10] A capability layer that hands raw tokens to an agent is much more dangerous than one that lets the agent ask a wallet to act under rules.

## Options

### Option A: Stay NWC-only

Pros:

- immediate compatibility;
- smallest surface;
- already proven with Alby and Mutinynet;
- easiest upstream path.

Cons:

- cannot express token-level or mint-level capabilities;
- limited privacy model;
- depends on NWC extensions moving slowly.

Verdict: correct for now.

### Option B: Add NWC extensions

Pros:

- uses the sanctioned extension path;[1][23]
- keeps existing clients and relays;
- can expose budget state via `get_budget`;[30]
- can reuse connection-creation budget requests.[4]

Cons:

- extension work requires ecosystem acceptance;
- still Lightning-shaped;
- awkward for raw ecash transfer and mint choice.

Verdict: good for budget discovery and metadata, not enough for full Cashu-native capability.

### Option C: Build CWC-style protocol now

Pros:

- native Cashu methods;
- multi-mint and NUT-18 support;
- aligned with the CWC draft.[27]

Cons:

- draft protocol;[27]
- no deployed user base;
- duplicate wallet implementations;
- high adoption risk;
- bearer-token exposure risk.[10]

Verdict: too early.

### Option D: Two-front architecture

Pros:

- preserves NWC adoption;
- keeps policy core reusable;
- allows one narrow Cashu-native experiment later;
- avoids committing to a new protocol before validation.

Cons:

- slightly more architecture now;
- requires discipline to keep frontend logic thin.

Verdict: recommended.

## Recommendation

Keep the NWC frontend as the production path.

Treat the policy core as frontend-independent. Do not put Cashu-specific behavior into NWC core. Do not build CWC now.

Write one ADR that defines the policy core boundary and lists possible Cashu-native frontends. Then wait for a concrete app need.

## Smallest worthwhile future prototype

If we later build a Cashu-native frontend, start with one method:

```text
pay_cashu_request
```

It would:

- accept a NUT-18 payment request;[26]
- resolve amount, unit, mint constraints and transport;
- ask the same policy core for authorization;
- let the wallet select proofs and mints;
- return a receipt, not raw proofs.

This avoids handing bearer tokens to the agent.[10]

## Stop conditions

Do not build a Cashu-native protocol unless:

- one real app or agent workflow needs it;
- NWC extensions cannot express that workflow;
- the method set stays at one or two methods;
- the policy core is reused unchanged;
- at least one Cashu wallet maintainer or app team wants to test it.

Without those, it is overreach.

## Final judgment

Exploration is in scope. Implementation is not yet.

The best approach is to preserve what NWC gives us, keep the policy layer protocol-agnostic, and only add a Cashu-native frontend when a concrete bearer-ecash capability is needed.

NWC is the adoption bridge. A Cashu-native capability layer is the possible destination. Our current build is the reusable policy core underneath both.

## Sources

[1] https://raw.githubusercontent.com/nostr-protocol/nips/master/47.md — NIP-47 Nostr Wallet Connect
    > "Keys can be revoked and created at will and have arbitrary constraints (eg. budgets)."
    > "More granular authorization models and extensions for advanced wallet functionality may be defined in separate specifications"
[4] https://github.com/nostr-wallet-connect/nwc/blob/main/08.md — nwc/08.md at main · nostr-wallet-connect/nwc
    > "Letting the client create its own secret improves security and enables authorization-page UX where the user simply approves or declines a connection request."
[10] https://docs.cashu.space/faq — Frequently Asked Questions (FAQs) - Cashu.space
    > "Ecash tokens are bearer asset tokens."
[11] https://docs.cashu.space — Documentation
    > "Cashu is a free and open-source Chaumian ecash protocol built for Bitcoin."
[23] https://raw.githubusercontent.com/nostr-wallet-connect/nwc/main/README.md — README.md
    > "This repository contains standalone Nostr Wallet Connect specifications for optional features that are intentionally kept out of the simplified NIP-47 core spec."
[26] https://raw.githubusercontent.com/cashubtc/nuts/main/18.md — 18.md
    > "This NUT introduces a standardised format for payment requests, that supply a sending wallet with all information necessary to complete the transaction."
[27] https://raw.githubusercontent.com/Origami74/wally/master/SPECIFICATION.md — SPECIFICATION.md
    > "Cashu Wallet Connect (CWC) extends NIP-47 (Nostr Wallet Connect) to support Cashu ecash wallets."
    > "**Status:** Draft  
**Authors:** Wally Contributors"
    > "multi-mint support, and NUT-18 payment requests"
[30] https://raw.githubusercontent.com/nostr-wallet-connect/nwc/main/13.md — 13.md
    > "This specification queries an existing budget; it does not define a method for creating or changing one."
    > "A separate get_budget permission lets the wallet expose the connection's budget independently of get_balance."
[33] https://nwc.dev — https://nwc.dev/
    > "to connect lightning wallets to apps"
[35] https://guides.getalby.com/user-guide/alby-account/connect-to-apps/app-connections-via-nwc — App Connections via NWC | Alby Account | Alby Guides
    > "Stay in control: Connect your wallet to apps with a dedicated budget and custom permissions."
[41] https://docs.nwc.dev/bitcoin-lightning-wallets/best-practices — Best Practices | Nostr Wallet Connect Docs
    > "Enabling your users to set budgets creates an efficient safety measure to not overspend."
