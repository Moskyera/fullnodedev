HAC Miner Panel · By Mosky
===

Hacash fullnode + **OpenCL miners** (AMD / NVIDIA / Intel) + **GUI panel** for easy setup.

### Download (no GitHub knowledge needed)

## ⛏️ **[https://moskyera.github.io](https://moskyera.github.io)** two buttons: Full or Miner only

| Package | Use when |
|---------|----------|
| **Full** | Clean PC `hacash.exe` + workers + panel + `SETUP.bat` |
| **Miner only** | You already run the fullnode |

After extract on Windows: **`SETUP.bat`** or **`SETUP-MINER.bat`** → **`miner-panel.exe`**

On Linux x86_64: extract the `.tar.gz` package → **`./SETUP-LINUX.sh`** → **`./START-MINER-PANEL.sh`**

### Check the download before running it

These binaries can hold mining rewards and wallet keys. Every release is signed
by GitHub with **build provenance attestation** (OIDC, no maintainer key in the
repo). Verifying it is the **only** check that detects tampering:

```
gh attestation verify <file> --repo <owner>/<repo>
```

`gh` is the GitHub CLI (Windows and Linux). Use the repo you downloaded from, for
example `--repo Moskyera/fullnodedev`. If verification fails, **delete the file
and do not run it.**

The `.sha256` files are **not a signature**. They only catch a truncated or
corrupted download, and they are published from the same place as the archives,
so anyone able to replace an archive can replace its checksum too. A matching
checksum is **not** proof the file is genuine; the attestation is.

| Check | Windows | Linux |
|---|---|---|
| Genuine (tamper) | `gh attestation verify <file>.zip --repo <owner>/<repo>` | `gh attestation verify <file>.tar.gz --repo <owner>/<repo>` |
| Corruption only | `Get-FileHash <file>.zip -Algorithm SHA256` | `sha256sum -c <file>.tar.gz.sha256` |

### HAC OpenCL / CUDA + HACD CPU mining

| Coin | Worker | Backend |
|------|--------|---------|
| **HAC** | `poworker` | OpenCL (AMD/NVIDIA/Intel) and/or **CUDA** (NVIDIA) |
| **HACD** | `diaworker` | CPU only (no OpenCL/CUDA) |

- **HACD nonce rate jumps, and the "best so far" string gets weaker. Both are expected.**
  `diaworker` now runs the sha3-only half of the difficulty check *before* the 17+
  rounds of x16rs. A nonce that fails it can never mint a diamond for any x16rs
  hash, so its rounds are skipped: measured **8.3x** at diamond 133,700, **26x** at
  210,000, **60x** at 300,000, and the gain keeps rising with the diamond number.
  Below 42,000 the check passes everything and costs nothing. The consequence you
  see is that the two diamond strings in the status line (`... | AAAA -> BBBB.`)
  are now drawn only from the ~11% of nonces that got past the check, so they show
  fewer leading zeros than they used to. Those strings are **display only**; the
  set of diamonds actually found and submitted is unchanged, which is what
  `a_failing_gate_forbids_every_possible_x16rs_hash` (app/src/hash_util.rs) and
  `the_prefilter_never_skips_a_nonce_that_could_have_minted` (app/src/diaworker.rs)
  assert on every build.
- OpenCL: **[docs/MINING-AMD.md](docs/MINING-AMD.md)**, **[docs/MINING-LINUX.md](docs/MINING-LINUX.md)**
- CUDA: **[docs/MINING-NVIDIA-CUDA.md](docs/MINING-NVIDIA-CUDA.md)** (T4 Colab validated)
- Public free-IP pool: **`hac-pool`** - **[docs/PUBLIC-POOL.md](docs/PUBLIC-POOL.md)**
- Running a payout pool (wallet passphrase, payout procedure, 16-block payout lag): **[docs/POOL-OPERATOR.md](docs/POOL-OPERATOR.md)** (read this before mining for other people)
- Community requirements map: **[docs/COMMUNITY-REQUIREMENTS.md](docs/COMMUNITY-REQUIREMENTS.md)**
- Official rebuild notes: **[docs/JOJOIN-REBUILD.md](docs/JOJOIN-REBUILD.md)**

**Maintainers:** tag `vX.Y.Z` runs `.github/workflows/release.yml` (OpenCL workers + panel). CUDA and `hac-pool` builds: see JoJoin rebuild doc.

### Module Architecture

```
- x16rs    ->  x16rs-sys
- sys      ->  -
- field    ->  sys
- basis    ->  field
- protocol ->  basis
- chain    ->  protocol
- scaner   ->  protocol
- mint     ->  protocol
- node     ->  protocol, tokio
- server   ->  mint, tokio
- app      ->  mint
```

