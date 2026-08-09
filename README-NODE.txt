HPAY-compatible Hacash Full Node
================================

This package is for people who want to run a Hacash full node for HPAY Fast Pay
or an L2 hub without installing the miner package.

It does not change the Hacash consensus protocol. It uses the HVM and contract
features already provided by the Istanbul runtime. HPAY checks the node's
/query/capabilities response and fails closed when a required API or runtime
feature is unavailable.

SECURITY FIRST
--------------
1. Verify the GitHub build attestation before running the archive:

   gh attestation verify <archive> --repo Moskyera/fullnodedev

2. The SHA-256 file detects accidental corruption only. It is not a signature.
3. Keep the HTTP API bound to 127.0.0.1 unless you have configured a firewall,
   TLS reverse proxy and authentication for a deliberate remote deployment.
4. Never add a reward address or wallet password unless you intentionally
   enable mining. Mining is disabled in the included example configuration.
5. Back up the node data directory before replacing an existing binary.

WINDOWS
-------
1. Extract the ZIP into a new folder.
2. Copy hacash.config.ini.example to hacash.config.ini.
3. Run hacash.exe.

LINUX
-----
1. Extract the archive into a new folder.
2. Copy hacash.config.ini.example to hacash.config.ini.
3. Make the binary executable if required: chmod +x hacash
4. Run ./hacash.

The included configuration joins Hacash mainnet, enables the local HTTP API on
127.0.0.1:8080 and keeps HAC and HACD mining disabled. Wait for synchronization
to complete before connecting an HPAY L2 hub.

PACKAGE BOUNDARY
----------------
This archive contains the full node only. It deliberately excludes poworker,
diaworker, miner-panel, mining kernels, pool software and wallet software.

