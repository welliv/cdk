# Final audit record

Independent audit of commit `0dc2cb4` found concrete defects in the manager,
CLI and ledger lock. All load-bearing findings were fixed and re-verified.

## Resolved

- **Ledger lock defeated by atomic rename.**  
  The policy ledger now locks a dedicated sidecar `.policy.lock` file. Every
  operation re-reads the ledger while holding the lock. Atomic rename no
  longer replaces the locked inode.

- **Two stores could double-authorize.**  
  A regression test now creates two stores on one path and verifies the second
  sees the first's replay record.

- **`list` exposed secrets.**  
  `list` now returns a redacted view without client or service secrets.

- **Secrets were world-readable.**  
  Manager and policy files are created with mode `0600` on Unix and fsynced
  before atomic rename.

- **`create_connection` was not atomic.**  
  Manager record is persisted first; the policy is added last. If policy
  registration fails, the manager record is rolled back.

- **`start()` ignored revocation and expiry.**  
  It now checks the live policy and refuses unauthorized connections.

- **Manager file had no lock.**  
  A dedicated `.manager.lock` sidecar protects manager writes.

- **`list`/`revoke` failed while `serve` ran.**  
  Locks are held only for short operations, and CLI `list`/`revoke` use
  metadata-only paths with no wallet construction. Verified live: both
  commands worked while `serve` was running.

- **`list`/`revoke` unnecessarily required wallet config.**  
  They now need only the manager and policy paths.

- **Seed on command line.**  
  `--seed-hex` is optional; `CDK_NWC_SEED_HEX` is supported and used in the
  verified run.

- **Unvalidated CLI inputs.**  
  Methods must be in CDK's NWC method set. Budget and caps must be positive,
  and the single cap and fee reserve cannot exceed the budget.

- **Default methods omitted `get_info`.**  
  Default is now `get_info,get_balance,pay_invoice`.

- **`serve` exit paths.**  
  `stop_all()` runs on both Ctrl+C and error paths. `serve` reports the number
  of services actually started.

- **`create` did not print the connection ID.**  
  It now prints `connection_id` alongside the NWC URI.

- **Manager and policy paths could collide.**  
  The CLI rejects identical paths.

## Accepted residual limitations

- Revocation is policy-level denial plus stop of idle services. An in-flight
  payment already dispatched to the wallet cannot be recalled.
- Secret files are mode `0600` on Unix; Windows ACL hardening is not done.
- Replay records are still an unbounded list.
- Clock hardening and transactional databases remain deferred.
- Inherited `cdk-nostr` issues remain: service-side client-secret retention,
  encryption-tag enforcement, and the `list_transactions` extension tag.

These are documented, out of the current scope, or planned follow-ups.
