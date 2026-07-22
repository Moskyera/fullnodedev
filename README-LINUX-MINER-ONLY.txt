HAC Miner - WORKERS ONLY (Linux x86_64)
By Mosky
=======================================

FOR: You already run the Hacash fullnode with miner RPC on 127.0.0.1:8080.
This archive does not include the fullnode.

QUICK START
-----------
1. Extract the .tar.gz archive.
2. Open a terminal in the extracted folder.
3. Run: ./SETUP-LINUX.sh
   If the file is not executable, run: bash SETUP-LINUX.sh
4. Choose your wallet and hardware in the panel, then press Start.

HAC uses an OpenCL GPU. Detection and Auto Tune happen in the panel.
HACD is CPU/fullnode mining and never uses OpenCL.

The setup can install standard Ubuntu/Debian libraries. You still need the
OpenCL driver supplied for your GPU. Verify it with: ./list_opencl

MINER FLEET
-----------
On each remote panel enable Settings -> Miner Fleet -> LAN sharing. On the main
panel open Dashboard -> Manage miners and add its address and token.

Detailed help: MINING-LINUX.md
