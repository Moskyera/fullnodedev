
/// Live peer connectivity, counted over the same connected peer set that
/// `all_peer_prints` walks.
///
/// `inbound` is the load bearing field. It counts remote nodes that dialed
/// THIS node and completed the p2p handshake, so it is the only one of these
/// numbers that proves the p2p port can actually be reached from outside.
/// A bound listening socket does not prove that: a node can listen forever,
/// pull blocks over connections it opened itself, and still relay for nobody.
/// `inbound` at zero means no peer has reached us; `inbound` above zero means
/// at least one has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerConnectivity {
    /// Every connected peer, inbound plus outbound. A peer key is held in at
    /// most one table, so nothing is counted twice.
    pub total: usize,
    /// Peers that dialed us and completed the handshake (the peer `is_cntome`
    /// flag). Not "listening": reached.
    pub inbound: usize,
    /// Peers we dialed out to.
    pub outbound: usize,
    /// Peers we hold a public, dialable address for. Says nothing about us.
    pub public: usize,
    /// False when the counts were never taken, in which case the zeroes above
    /// mean "unknown" and must not be reported as measured zeroes.
    pub measured: bool,
}

impl PeerConnectivity {
    /// True only when a remote peer has actually reached this node.
    pub fn inbound_proven(&self) -> bool {
        self.measured && self.inbound > 0
    }
}

// Hacash node
pub trait HNoder: Send + Sync {

    fn start(&self, _: Worker) {}

    fn submit_transaction(&self, _: &TxPkg, _is_async: bool, _only_insert_txpool: bool) -> Rerr { never!() }
    fn submit_block(&self, _: &BlkPkg, _is_async: bool) -> Rerr { never!() }

    fn engine(&self) -> Arc<dyn Engine> { never!() }
    fn txpool(&self) -> Arc<dyn TxPool> { never!() }

    fn register_p2p_extension(&self, _: Vec<u16>, _: Arc<dyn NodeP2PExtension>) -> Rerr {
        errf!("p2p extension registration not supported")
    }

    fn broadcast_p2p_extension_message(&self, _: Hash, _: u16, _: Vec<u8>) -> Rerr {
        errf!("p2p extension broadcast not supported")
    }

    fn all_peer_prints(&self) -> Vec<String> { never!() }

    /// Live connected peer counts. See `PeerConnectivity` for why `inbound`
    /// is the number that decides whether this node is a participant or a leaf.
    /// The default is an UNMEASURED zero (`measured: false`), so a caller that
    /// reaches a node build without p2p accounting never mistakes "not known"
    /// for "nobody has reached us".
    fn peer_connectivity(&self) -> PeerConnectivity { PeerConnectivity::default() }

    /// Prometheus-style lines for post-quantum metrics (empty when unsupported).
    fn pqc_metrics_prometheus(&self) -> Vec<String> { Vec::new() }

    fn exit(&self) {}
    
}


#[cfg(test)]
mod peer_connectivity_tests {
    use super::PeerConnectivity;

    #[test]
    fn inbound_is_only_proven_when_measured_and_non_zero() {
        assert!(!PeerConnectivity::default().inbound_proven());
        // An unmeasured struct must never claim reachability from stale counts.
        assert!(!PeerConnectivity { inbound: 3, measured: false, ..Default::default() }.inbound_proven());
        assert!(!PeerConnectivity { inbound: 0, measured: true, ..Default::default() }.inbound_proven());
        assert!(PeerConnectivity { inbound: 1, measured: true, ..Default::default() }.inbound_proven());
    }
}
