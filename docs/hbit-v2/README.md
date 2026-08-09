# HBIT v2 documentation

## What is here

| document | describes |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | how a share becomes money today, including the defects |
| [MAINNET-SAFETY.md](MAINNET-SAFETY.md) | what the pool refuses to do, as implemented and tested |

## What is deliberately not here

The upgrade plan calls for a further eight documents. They are absent on
purpose, because the machinery they would describe has not been written, and a
document that describes an unimplemented mechanism is worse than no document: an
operator reads it, believes the protection exists, and runs a mainnet pool on
that belief.

| document | blocked on |
|---|---|
| ACCOUNTING.md | frozen block entitlements and persistent per-miner balances |
| MIGRATION.md | a ledger to migrate to |
| RECOVERY.md | a recovery-required state that the pool can actually enter |
| BACKUP-RESTORE.md | the backup and verify commands |
| PROTOCOL-HBIT1.md | the versioned job protocol, job tokens and pool identity |
| THREAT-MODEL.md | can be written now, but is worth writing once the protocol above exists, or it documents a threat surface about to change |
| OPERATOR-RUNBOOK.md | the deployment fixes; the current systemd path does not start |
| MAINNET-VERIFICATION.md | there is nothing yet verified on mainnet to report |

Each will be written when the thing it describes exists and has a test that
fails when it is removed.

## What has and has not been proven

Stated at the level of evidence, not confidence.

**Unit and fixture tested.** Everything in MAINNET-SAFETY.md. Each rule has at
least one test that calls the real function rather than re-implementing it, and
each was proven by reverting the fix and watching the test fail.

**Not tested against a live mainnet node.** No part of this has run against the
real chain. The stub nodes in the test suite answer over real HTTP and speak the
node's real response shapes, which is not the same thing.

**Never exercised on mainnet.** No block has been submitted, no payout signed
and no transaction broadcast as part of this work.
