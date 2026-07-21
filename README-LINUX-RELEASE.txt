HAC Miner - FULL PACKAGE (Linux x86_64)
By Mosky
=======================================

FOR: A new Ubuntu/Debian PC. Fullnode, miners and panel are included.

QUICK START
-----------
1. Extract the .tar.gz archive.
2. Open a terminal in the extracted folder.
3. Run: ./SETUP-LINUX.sh
   If the file is not executable, run: bash SETUP-LINUX.sh
4. Choose your wallet and hardware in the panel, then press Start.

HAC uses an OpenCL GPU. Detection and Auto Tune happen in the panel.
HACD is CPU/fullnode mining and never uses OpenCL.

IMPORTANT GPU DRIVER NOTE
-------------------------
SETUP-LINUX.sh can install the standard Linux libraries, but the GPU's OpenCL
driver comes from AMD/Intel/NVIDIA. For AMD Radeon, follow the current ROCm
Linux instructions and install the OpenCL runtime. Verify it with: ./list_opencl

RX 9070 / RX 9070 XT users should use a current AMD-supported Ubuntu + ROCm
combination from the official compatibility matrix.

MINER FLEET
-----------
On each remote panel enable Settings -> Miner Fleet -> LAN sharing. On the main
panel open Dashboard -> Manage miners and add its address and token. The Fleet
API is read-only and disabled by default.

Detailed help: MINING-LINUX.md

