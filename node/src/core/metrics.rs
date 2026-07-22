use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use protocol::metrics::PqcMetricEvent;

/// Runtime lifecycle counters (unchanged dashboard contract).
#[derive(Default)]
pub struct RuntimeMetrics {
    pub start_count: u64,
    pub exit_count: u64,
}

impl RuntimeMetrics {
    pub fn on_start(&mut self) {
        self.start_count += 1;
    }

    pub fn on_exit(&mut self) {
        self.exit_count += 1;
    }
}

/// Post-quantum / Type 4 observability counters.
#[derive(Default, Clone)]
pub struct PqcMetrics {
    pub pqc_tx_total: u64,
    pub hybrid_tx_total: u64,
    pub type4_mempool_rejected: u64,
    pub type4_sign_verified: u64,
    /// Cumulative ML-DSA verify time in microseconds.
    pub mldsa_verify_us_total: u64,
    pub mldsa_verify_count: u64,
}

impl PqcMetrics {
    pub fn on_event(&mut self, event: PqcMetricEvent) {
        match event {
            PqcMetricEvent::MldsaVerifyUs(us) => {
                self.mldsa_verify_us_total = self.mldsa_verify_us_total.saturating_add(us);
                self.mldsa_verify_count = self.mldsa_verify_count.saturating_add(1);
            }
            PqcMetricEvent::Type4MempoolAccepted { hybrid } => {
                self.pqc_tx_total = self.pqc_tx_total.saturating_add(1);
                if hybrid {
                    self.hybrid_tx_total = self.hybrid_tx_total.saturating_add(1);
                }
            }
            PqcMetricEvent::Type4MempoolRejected => {
                self.type4_mempool_rejected = self.type4_mempool_rejected.saturating_add(1);
            }
            PqcMetricEvent::Type4SignVerified { hybrid } => {
                self.type4_sign_verified = self.type4_sign_verified.saturating_add(1);
                if hybrid {
                    self.hybrid_tx_total = self.hybrid_tx_total.saturating_add(1);
                }
            }
        }
    }

    pub fn mldsa_verify_ms(&self) -> f64 {
        self.mldsa_verify_us_total as f64 / 1000.0
    }

    pub fn mldsa_verify_ms_avg(&self) -> f64 {
        if self.mldsa_verify_count == 0 {
            return 0.0;
        }
        self.mldsa_verify_ms() / self.mldsa_verify_count as f64
    }

    pub fn prometheus_lines(&self) -> Vec<String> {
        vec![
            format!("hacash_pqc_tx_total {}", self.pqc_tx_total),
            format!("hacash_hybrid_tx_total {}", self.hybrid_tx_total),
            format!(
                "hacash_type4_mempool_rejected_total {}",
                self.type4_mempool_rejected
            ),
            format!(
                "hacash_type4_sign_verified_total {}",
                self.type4_sign_verified
            ),
            format!("hacash_mldsa_verify_ms {}", self.mldsa_verify_ms()),
            format!("hacash_mldsa_verify_count {}", self.mldsa_verify_count),
            format!("hacash_mldsa_verify_ms_avg {}", self.mldsa_verify_ms_avg()),
        ]
    }

    pub fn to_json_map(&self) -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        m.insert("pqc_tx_total".to_owned(), self.pqc_tx_total.to_string());
        m.insert(
            "hybrid_tx_total".to_owned(),
            self.hybrid_tx_total.to_string(),
        );
        m.insert(
            "type4_mempool_rejected".to_owned(),
            self.type4_mempool_rejected.to_string(),
        );
        m.insert(
            "type4_sign_verified".to_owned(),
            self.type4_sign_verified.to_string(),
        );
        m.insert(
            "mldsa_verify_ms".to_owned(),
            format!("{:.3}", self.mldsa_verify_ms()),
        );
        m.insert(
            "mldsa_verify_ms_avg".to_owned(),
            format!("{:.3}", self.mldsa_verify_ms_avg()),
        );
        m.insert(
            "mldsa_verify_count".to_owned(),
            self.mldsa_verify_count.to_string(),
        );
        m
    }
}

static GLOBAL_PQC_METRICS: OnceLock<Arc<StdMutex<PqcMetrics>>> = OnceLock::new();

/// Register the node-owned metrics Arc for the protocol hook and RPC snapshot.
pub fn install_global_pqc_metrics(metrics: Arc<StdMutex<PqcMetrics>>) {
    let _ = GLOBAL_PQC_METRICS.set(metrics);
}

pub fn global_pqc_metrics() -> Option<Arc<StdMutex<PqcMetrics>>> {
    GLOBAL_PQC_METRICS.get().cloned()
}

pub fn pqc_metrics_hook_fn(event: PqcMetricEvent) {
    if let Some(metrics) = global_pqc_metrics() {
        if let Ok(mut m) = metrics.lock() {
            m.on_event(event);
        }
    }
}

pub fn install_pqc_metrics_hook() {
    protocol::metrics::install_hook(pqc_metrics_hook_fn);
    sys::set_mldsa_verify_observer(|us| {
        pqc_metrics_hook_fn(PqcMetricEvent::MldsaVerifyUs(us));
    });
}
