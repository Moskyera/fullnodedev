HAC Miner — WORKERS ONLY (Windows x64)
By Mosky
======================================

FOR: You already run hacash.exe fullnode + hacash.config.ini

QUICK START
-----------
1. Extract ZIP to a folder
2. Run SETUP-MINER.bat
3. Open miner-panel.exe and choose:
   - HAC: OpenCL GPU is detected automatically; Auto Tune is available.
   - HACD: CPU/full-node mining; choose CPU threads (OpenCL is not used).
4. Enter the reward wallet and press Start.

INCLUDED
--------
  miner-panel.exe  GUI
  poworker.exe     HAC miner
  diaworker.exe    HACD miner
  list_opencl.exe  GPU device list
  diagnose_opencl.exe  GPU diagnostics and automatic selection
  x16rs/opencl/    OpenCL kernels

MINER FLEET (MULTIPLE MINERS)
-----------------------------
1. On each remote panel: Settings → Miner Fleet → enable LAN sharing.
2. Copy its LAN address/port and access token.
3. On the main panel: Dashboard → Manage miners → Add miner.
The dashboard totals hashrate, online panels, power and daily cost. The LAN API is read-only and disabled by default.

NOT INCLUDED (you must have these)
----------------------------------
  hacash.exe       Fullnode
  hacash.config.ini  Wallet + [server] RPC port 8080

Default connect: 127.0.0.1:8080

CLEAN PC?
---------
Download hacash-miner-full-windows-x64*.zip instead.