# cdk-nwc-forward

An `NwcRequestHandler` that forwards every NIP-47 request to an upstream NWC
wallet (for example Alby Hub). Put `cdk-nwc-policy` in front of it to give
apps restricted links to a wallet you do not run yourself.

```rust,ignore
let upstream = ForwardingNwcHandler::from_uri(&uri, Duration::from_secs(30))?;
```

Upstream error codes are preserved (`upstream_error_from_message`), so the
policy layer can tell `INSUFFICIENT_BALANCE` (nothing was paid) from
ambiguous failures.

License: MIT or Apache-2.0, at your option.
