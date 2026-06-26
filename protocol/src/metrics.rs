use std::sync::OnceLock;

/// Observability events for post-quantum / Type 4 traffic.
#[derive(Clone, Copy, Debug)]
pub enum PqcMetricEvent {
    /// ML-DSA-65 detached verify duration in microseconds.
    MldsaVerifyUs(u64),
    /// Type 4 tx accepted into mempool (local submit or P2P).
    Type4MempoolAccepted { hybrid: bool },
    /// Type 4 tx rejected before mempool insert.
    Type4MempoolRejected,
    /// Type 4 hybrid signature verified during execution.
    Type4SignVerified { hybrid: bool },
}

pub type PqcMetricsHook = fn(PqcMetricEvent);

static PQC_METRICS_HOOK: OnceLock<PqcMetricsHook> = OnceLock::new();

/// Install a process-wide hook (typically from the node runtime at startup).
pub fn install_hook(hook: PqcMetricsHook) {
    let _ = PQC_METRICS_HOOK.set(hook);
}

#[inline]
pub fn emit(event: PqcMetricEvent) {
    if let Some(hook) = PQC_METRICS_HOOK.get() {
        hook(event);
    }
}