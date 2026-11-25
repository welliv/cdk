# cdk-nwc-policy-cli

`nwc-policy-cli` creates, lists, revokes and serves NWC (NIP-47) links backed
by a CDK Cashu wallet, or by an upstream NWC wallet with
`--upstream-nwc-uri-file`.

Commands: `create`, `list`, `revoke`, `serve`, `fund`, `topup`, `sweep`,
`pockets`. `create --pocket` gives the link its own Cashu pocket
(`cdk-nwc-pocket`); without it the link shares the main balance under a
ledger budget.

See [NWC.md](../../NWC.md) for setup and the
[operator guide](../../NWC_POLICY_MANAGER_CLI.md) for every flag.

License: MIT or Apache-2.0, at your option.
