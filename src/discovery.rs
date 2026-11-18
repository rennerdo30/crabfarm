//! Package B. Peer discovery: mDNS, static peers, gossip persistence
//! (DESIGN section 4).

use crate::proto::{NodeId, PeerInfo};
use std::path::Path;

/// Known peers, persisted in `peers.toml`.
#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerBook {
    pub peers: Vec<PeerInfo>,
}

impl PeerBook {
    pub fn load(data_dir: &Path) -> anyhow::Result<PeerBook> {
        let _ = data_dir;
        todo!("package B")
    }
    pub fn save(&self, data_dir: &Path) -> anyhow::Result<()> {
        let _ = data_dir;
        todo!("package B")
    }
    /// Merge gossip: union addresses, keep max last_seen, never add `self_id`.
    pub fn merge(&mut self, incoming: &[PeerInfo], self_id: NodeId) -> bool {
        let _ = (incoming, self_id);
        todo!("package B")
    }
}

/// mDNS advertisement + browsing for `_crabfarm._tcp.local.` filtered by
/// short farm id. Found peers arrive on the channel as `(node id hex, addr)`.
pub struct Mdns {
    // implementation-defined (mdns_sd::ServiceDaemon)
}

impl Mdns {
    pub fn start(
        node_id: NodeId,
        short_farm_id: &str,
        port: u16,
        found: tokio::sync::mpsc::UnboundedSender<(String, String)>,
    ) -> anyhow::Result<Mdns> {
        let _ = (node_id, short_farm_id, port, found);
        todo!("package B")
    }
    pub fn stop(self) {
        todo!("package B")
    }
}
