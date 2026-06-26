use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use basis::component::*;
use basis::config::{EngineConf, NodeConf};
use basis::interface::*;
use field::*;
use sys::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedReadHalf;

use crate::handler::*;
use crate::p2p::*;
use crate::peer::{KNOWLEDGE_SIZE, KnowKey, Knowledge, Peer, PeerKey};
use crate::*;

mod api;
mod metrics;
mod network;
mod protocol;
mod runtime;
mod submit;
mod sync;
mod tasks;
mod transport;

pub use api::HacashNode;
pub use sync::SyncTracker;

pub(crate) use metrics::{
    PqcMetrics, RuntimeMetrics, install_global_pqc_metrics, install_pqc_metrics_hook,
};
pub use metrics::PqcMetrics as NodePqcMetrics;
pub(crate) use protocol::{
    handle_new_block, handle_new_tx, receive_blocks, receive_hashs, receive_status, send_blocks,
    send_hashs, send_status,
};
pub(crate) use runtime::NodeRuntime;
pub(crate) use tasks::TaskGroup;
pub(crate) use transport::{
    broadcast_unaware, connect_boot_nodes, connect_node, connect_stable_nodes,
    connect_stable_then_boot, event_loop, handle_conn, insert_peer,
};
