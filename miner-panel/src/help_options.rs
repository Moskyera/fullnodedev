use crate::i18n::Lang;

pub struct HelpSection {
    pub title: &'static str,
    pub lines: &'static [&'static str],
}

pub fn option_reference(lang: Lang) -> &'static [HelpSection] {
    match lang {
        Lang::El => EL,
        _ => EN,
    }
}

static EN: &[HelpSection] = &[
    HelpSection {
        title: "miner-panel.exe (Settings tab)",
        lines: &[
            "CPU preset — supervene threads (Ryzen / Intel / CPU-only).",
            "GPU preset — amd_* / nvidia_* profile + estimated board watts.",
            "Mode — eco (low power), profit (kH/J), max (raw hashrate).",
            "Power cost — €/kWh (or local currency) for daily cost estimate.",
            "HAC price — optional $/HAC for profit / pause-if-unprofitable.",
            "Max temp — GPU throttle above this °C (0 = off). AMD: set thermal_file if auto-detect fails.",
            "Pause if unprofitable — stop hashing when estimated power cost > revenue.",
            "Benchmark — runs poworker autotune (~90s), writes best profile + work_groups + unit_size to ini.",
            "Mining type — HAC (blocks) or HACD (diamonds + auto-bids).",
            "Connect — HAC: local fullnode or pool RPC. HACD: local or shared LAN fullnode.",
            "Wallet — HAC: [miner] reward in hacash.config.ini. HACD: PRIVAKEY (3x...) reward.",
            "OpenCL — platform_id + device_id from list_opencl.exe.",
            "HACD bids — bid_password, bid_min / bid_max / bid_step (mei format, e.g. 1:0 = 1 HAC).",
        ],
    },
    HelpSection {
        title: "poworker.config.ini / diaworker.config.ini — [default]",
        lines: &[
            "connect — fullnode or pool miner RPC (default 127.0.0.1:8081).",
            "supervene — configured CPU miner threads.",
            "nonce_max — max nonce per batch (poworker only, default 4294967295).",
            "notice_wait — seconds to wait for new-block notice (poworker, default 45).",
        ],
    },
    HelpSection {
        title: "[gpu] section (poworker / HAC only)",
        lines: &[
            "use_opencl — true = HAC GPU mining with OpenCL (AMD/NVIDIA/Intel; no CUDA). HACD remains false.",
            "cpu_assist — hybrid: GPU + extra CPU threads (Ryzen assist).",
            "gpu_profile — amd_eco|balanced|profit|performance|max or nvidia_* / intel_balanced.",
            "platform_id — OpenCL platform index (list_opencl.exe).",
            "device_ids — device index or comma-separated list (e.g. 0 or 0,1).",
            "opencl_dir — path to x16rs/opencl/ kernels (relative to exe folder).",
            "work_groups — global work size / autotune result (VRAM-clamped at runtime).",
            "local_size — must be 256 (kernel requirement).",
            "unit_size — hashes per work item (64–160; autotune may tune).",
            "debug — OpenCL debug level (0 = off).",
        ],
    },
    HelpSection {
        title: "[efficiency] section",
        lines: &[
            "mode — max | profit | eco (also amd_profit / amd_eco aliases).",
            "power_cost_kwh — electricity price for profit estimates.",
            "gpu_watts — override GPU power (0 = estimate from profile).",
            "cpu_watts_per_thread — watts per CPU assist thread (default 8).",
            "hac_price — HAC/USD for profit pause (0 = disable revenue side).",
            "dynamic_supervene — auto adjust CPU assist from GPU/CPU ratio.",
            "supervene_min / supervene_max — CPU thread bounds for dynamic assist.",
            "oom_fallback — halve work_groups on OpenCL OOM (default true).",
            "max_temp_c — thermal throttle above this temp (0 = off).",
            "throttle_work_groups — target work_groups when hot; must be below full load (panel writes half of WG).",
            "thermal_file — path to plain-text GPU temp °C (optional; overrides auto-detect).",
            "thermal_gpu_index — nvidia-smi / amd-smi GPU index (default 0).",
            "idle_start_hour / idle_end_hour — local-time mining window (255 = always on).",
            "pause_if_unprofitable — pause when power cost > mining revenue.",
            "benchmark_seconds — >0 runs autotune then exits (panel uses 90).",
            "benchmark_fine_sweep — tune work_groups + unit_size (default on if benchmark ≥ 60s).",
            "stats_file — JSON path for miner-panel dashboard (e.g. miner-stats.json).",
        ],
    },
    HelpSection {
        title: "hacash.config.ini — [miner] (HAC)",
        lines: &[
            "enable — true for block mining reward.",
            "reward — wallet address receiving block rewards.",
        ],
    },
    HelpSection {
        title: "hacash.config.ini — [diamondminer] (HACD)",
        lines: &[
            "enable — true for diamond auto-bidding.",
            "reward — PRIVAKEY address (3x...) for diamond rewards.",
            "bid_password — wallet password for automatic bids.",
            "bid_min / bid_max / bid_step — bid range in mei (1:0 = 1 HAC).",
            "Fullnode also needs diamond_form = true in node config.",
        ],
    },
    HelpSection {
        title: "Executables in release folder",
        lines: &[
            "miner-panel.exe — GUI: settings, dashboard, help.",
            "poworker.exe — HAC block miner (reads poworker.config.ini).",
            "diaworker.exe — HACD diamond miner (reads diaworker.config.ini).",
            "list_opencl.exe — list OpenCL platforms/devices and config hints.",
            "hacash.exe / fullnode.exe — Hacash full node (miner RPC + diamond bids).",
        ],
    },
];

static EL: &[HelpSection] = &[
    HelpSection {
        title: "miner-panel.exe (καρτέλα Ρυθμίσεις)",
        lines: &[
            "CPU preset — νήματα supervene (Ryzen / Intel / μόνο CPU).",
            "GPU preset — προφίλ amd_* / nvidia_* + εκτιμώμενα watt πλακέτας.",
            "Mode — eco (χαμηλή κατανάλωση), profit (kH/J), max (μέγιστο hashrate).",
            "Κόστος ρεύματος — €/kWh για εκτίμηση ημερήσιου κόστους.",
            "Τιμή HAC — προαιρετική τιμή $/HAC για κέρδος / pause-if-unprofitable.",
            "Μέγ. θερμοκρ. — throttle GPU πάνω από αυτό το °C (0 = απενεργ.). Για AMD: βάλε thermal_file αν δεν ανιχνεύεται αυτόματα.",
            "Pause if unprofitable — σταματά mining όταν κόστος ρεύματος > έσοδα.",
            "Benchmark — τρέχει autotune στο poworker (~90s), γράφει καλύτερο profile + work_groups + unit_size στο ini.",
            "Τύπος mining — HAC (blocks) ή HACD (diamonds + αυτόματα bids).",
            "Connect — HAC: solo fullnode ή pool RPC. HACD: τοπικό ή κοινό LAN fullnode.",
            "Wallet — HAC: reward στο [miner] του hacash.config.ini. HACD: PRIVAKEY (3x...).",
            "OpenCL — platform_id + device_id από list_opencl.exe.",
            "HACD bids — bid_password, bid_min / bid_max / bid_step (μορφή mei, π.χ. 1:0 = 1 HAC).",
        ],
    },
    HelpSection {
        title: "poworker.config.ini / diaworker.config.ini — [default]",
        lines: &[
            "connect — RPC fullnode ή pool (default 127.0.0.1:8081).",
            "supervene — ρυθμισμένα CPU threads.",
            "nonce_max — μέγιστο nonce ανά batch (μόνο poworker, default 4294967295).",
            "notice_wait — αναμονή ειδοποίησης νέου block σε sec (poworker, default 45).",
        ],
    },
    HelpSection {
        title: "[gpu] (poworker / μόνο HAC)",
        lines: &[
            "use_opencl — true = HAC GPU mining με OpenCL (AMD/NVIDIA/Intel· όχι CUDA). Στο HACD μένει false.",
            "cpu_assist — hybrid: GPU + επιπλέον CPU threads.",
            "gpu_profile — amd_eco|balanced|profit|performance|max ή nvidia_* / intel_balanced.",
            "platform_id — δείκτης OpenCL platform (list_opencl.exe).",
            "device_ids — δείκτης συσκευής ή λίστα (π.χ. 0 ή 0,1).",
            "opencl_dir — διαδρομή kernels x16rs/opencl/ (σχετική με τον φάκελο exe).",
            "work_groups — global work size / αποτέλεσμα autotune (VRAM clamp στο runtime).",
            "local_size — πρέπει να είναι 256 (απαίτηση kernel).",
            "unit_size — hashes ανά work item (64–160).",
            "debug — επίπεδο debug OpenCL (0 = off).",
        ],
    },
    HelpSection {
        title: "[efficiency]",
        lines: &[
            "mode — max | profit | eco.",
            "power_cost_kwh — τιμή ρεύματος για εκτίμηση κέρδους.",
            "gpu_watts — override ισχύος GPU (0 = εκτίμηση από profile).",
            "cpu_watts_per_thread — watt ανά CPU thread (default 8).",
            "hac_price — HAC/USD για profit pause (0 = χωρίς έσοδα).",
            "dynamic_supervene — αυτόματη ρύθμιση CPU assist από αναλογία GPU/CPU.",
            "supervene_min / supervene_max — όρια CPU threads.",
            "oom_fallback — μειώνει work_groups στο OpenCL OOM (default true).",
            "max_temp_c — thermal throttle πάνω από αυτή τη θερμοκρ. (0 = off).",
            "throttle_work_groups — στόχος work_groups όταν ζεσταίνεται· κάτω από full load (panel = μισό WG).",
            "thermal_file — αρχείο κειμένου με θερμοκρ. GPU σε °C (προαιρετικό).",
            "thermal_gpu_index — δείκτης GPU για nvidia-smi / amd-smi (default 0).",
            "idle_start_hour / idle_end_hour — παράθυρο mining τοπικής ώρας (255 = πάντα on).",
            "pause_if_unprofitable — παύση όταν κόστος > έσοδα.",
            "benchmark_seconds — >0 τρέχει autotune και τερματίζει.",
            "benchmark_fine_sweep — ρύθμιση work_groups + unit_size (default αν benchmark ≥ 60s).",
            "stats_file — JSON για dashboard (π.χ. miner-stats.json).",
        ],
    },
    HelpSection {
        title: "hacash.config.ini — [miner] (HAC)",
        lines: &[
            "enable — true για block mining reward.",
            "reward — διεύθυνση wallet για block rewards.",
        ],
    },
    HelpSection {
        title: "hacash.config.ini — [diamondminer] (HACD)",
        lines: &[
            "enable — true για αυτόματα diamond bids.",
            "reward — PRIVAKEY (3x...) για diamond rewards.",
            "bid_password — κωδικός wallet για bids.",
            "bid_min / bid_max / bid_step — εύρος bid σε mei (1:0 = 1 HAC).",
            "Το fullnode χρειάζεται επίσης diamond_form = true.",
        ],
    },
    HelpSection {
        title: "Executables στον φάκελο release",
        lines: &[
            "miner-panel.exe — GUI: ρυθμίσεις, dashboard, help.",
            "poworker.exe — HAC block miner (διαβάζει poworker.config.ini).",
            "diaworker.exe — HACD diamond miner (διαβάζει diaworker.config.ini).",
            "list_opencl.exe — λίστα OpenCL platforms/devices.",
            "hacash.exe / fullnode.exe — Hacash full node (miner RPC + diamond bids).",
        ],
    },
];
