# Security Policy — Hacash fullnodedev

## Supported branches

- `main` / release tags: production full node
- Feature branches (e.g. `hip-23-draft`): draft standards — run tests before integration

## HIP-23 integrator release gate

Before shipping wallet, gateway, or indexer support for HIP-23 patterns:

```bash
cargo test hip23_ -- --nocapture
```

This runs regression, adversarial, stress, production-path, audit-strict, chain, replay, and proptest suites.

Documentation:

- `doc/HIP23.md` — normative spec
- `doc/HIP23_wallet_checklist.md` — pre-sign validation
- `doc/HIP23_threat_model.md` — threat analysis
- `doc/HIP23_audit_findings.md` — known issues

## Reporting vulnerabilities

Report security issues privately to the repository maintainers. Do not open public issues for exploitable consensus or node vulnerabilities until coordinated disclosure.

Include:

- Affected component (node, protocol, HIP draft)
- Reproduction steps or proof-of-concept
- Impact assessment
- Suggested fix (optional)

## Test modes

| Mode | `fast_sync` | Use |
|------|-------------|-----|
| Pattern semantics | `true` | Guard/TEX/AST logic |
| Production-like | `false` | Signatures, duplicate-tx, fee rules |

Integrators MUST validate against production-like mode before mainnet use.

## Dependency audit

Periodically run:

```bash
cargo audit
```

(Requires `cargo-audit` crate installed.)

## Scope note

HIP-23 is an **application integration standard** — it does not alter consensus. Security focus is on correct composition, co-signing, and indexer classification to prevent integrator-level fund loss.