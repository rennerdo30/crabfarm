//! Package B. Peer discovery: mDNS, static peers, gossip persistence
//! (DESIGN section 4).

use crate::proto::{NodeId, PROTO_VERSION, PeerInfo};
use anyhow::Context;
use std::path::Path;

/// Gossip comes from farm members, but a buggy one must not grow the book
/// without bound.
const MAX_ADDRS_PER_PEER: usize = 16;
const MAX_PEERS: usize = 1024;
pub const MDNS_SERVICE: &str = "_crabfarm._tcp.local.";

/// Known peers, persisted in `peers.toml`.
#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerBook {
    #[serde(default)]
    pub peers: Vec<PeerInfo>,
}

impl PeerBook {
    /// A missing file is an empty book.
    pub fn load(data_dir: &Path) -> anyhow::Result<PeerBook> {
        let path = data_dir.join("peers.toml");
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parse {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(PeerBook::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn save(&self, data_dir: &Path) -> anyhow::Result<()> {
        let path = data_dir.join("peers.toml");
        std::fs::create_dir_all(data_dir).with_context(|| format!("create {}", data_dir.display()))?;
        let text = toml::to_string(self).context("encode peers")?;
        // Replace atomically: a node killed mid-write must not corrupt the book.
        let tmp = data_dir.join("peers.toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }

    /// Merge gossip: union addresses, keep max last_seen, never add `self_id`.
    /// Returns true when the book changed.
    pub fn merge(&mut self, incoming: &[PeerInfo], self_id: NodeId) -> bool {
        let mut changed = false;
        for inc in incoming {
            if inc.node_id == self_id {
                continue;
            }
            let full = self.peers.len() >= MAX_PEERS;
            match self.peers.iter_mut().find(|p| p.node_id == inc.node_id) {
                Some(known) => {
                    for a in &inc.addrs {
                        if !known.addrs.contains(a) && known.addrs.len() < MAX_ADDRS_PER_PEER {
                            known.addrs.push(a.clone());
                            changed = true;
                        }
                    }
                    if inc.last_seen > known.last_seen {
                        known.last_seen = inc.last_seen;
                        // The freshest report also has the freshest name.
                        if known.node_name != inc.node_name {
                            known.node_name = inc.node_name.clone();
                        }
                        changed = true;
                    }
                }
                None if !full => {
                    let mut p = inc.clone();
                    let mut seen = std::collections::HashSet::new();
                    p.addrs.retain(|a| seen.insert(a.clone()));
                    p.addrs.truncate(MAX_ADDRS_PER_PEER);
                    self.peers.push(p);
                    changed = true;
                }
                None => {}
            }
        }
        changed
    }
}

/// mDNS advertisement + browsing for `_crabfarm._tcp.local.` filtered by
/// short farm id. Found peers arrive on the channel as `(node id hex, addr)`.
pub struct Mdns {
    daemon: mdns_sd::ServiceDaemon,
    fullname: String,
}

impl Mdns {
    pub fn start(
        node_id: NodeId,
        short_farm_id: &str,
        port: u16,
        found: tokio::sync::mpsc::UnboundedSender<(String, String)>,
    ) -> anyhow::Result<Mdns> {
        let daemon = mdns_sd::ServiceDaemon::new().context("start mDNS daemon")?;
        let own = node_id.to_hex();
        let host = format!("crabfarm-{}.local.", &own[..12]);
        let version = PROTO_VERSION.to_string();
        // Empty address + addr_auto: the daemon advertises every local interface.
        let info = mdns_sd::ServiceInfo::new(
            MDNS_SERVICE,
            &own,
            &host,
            "",
            port,
            &[("f", short_farm_id), ("v", version.as_str())][..],
        )
        .context("build mDNS service info")?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info).context("register mDNS service")?;
        let events = daemon.browse(MDNS_SERVICE).context("browse mDNS")?;

        let farm = short_farm_id.to_string();
        // mdns-sd hands out a blocking channel, so a plain thread forwards it.
        // The thread ends when the daemon shuts down (channel closes) or the
        // consumer drops its receiver.
        std::thread::Builder::new()
            .name("crabfarm-mdns".into())
            .spawn(move || {
                while let Ok(ev) = events.recv() {
                    let mdns_sd::ServiceEvent::ServiceResolved(svc) = ev else { continue };
                    if svc.get_property_val_str("f") != Some(farm.as_str()) {
                        continue;
                    }
                    // The instance name is the node id; anything else is not ours.
                    let Some(instance) = svc.get_fullname().strip_suffix(&format!(".{MDNS_SERVICE}")) else { continue };
                    if instance == own || instance.len() != 32 || !instance.bytes().all(|b| b.is_ascii_hexdigit()) {
                        continue;
                    }
                    for ip in svc.get_addresses() {
                        let ip = ip.to_ip_addr();
                        let addr = match ip {
                            std::net::IpAddr::V4(v4) => format!("{v4}:{}", svc.get_port()),
                            std::net::IpAddr::V6(v6) => format!("[{v6}]:{}", svc.get_port()),
                        };
                        if found.send((instance.to_string(), addr)).is_err() {
                            return;
                        }
                    }
                }
            })
            .context("spawn mDNS thread")?;
        Ok(Mdns { daemon, fullname })
    }

    /// Withdraw the advertisement and stop the daemon.
    pub fn stop(self) {
        // Best effort: the process is usually exiting anyway.
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: u8, name: &str, addrs: &[&str], seen: u64) -> PeerInfo {
        PeerInfo { node_id: NodeId([id; 16]), node_name: name.into(), addrs: addrs.iter().map(|s| s.to_string()).collect(), last_seen: seen }
    }

    #[test]
    fn merge_unions_addrs_keeps_max_seen_and_skips_self() {
        let me = NodeId([1; 16]);
        let mut book = PeerBook::default();
        assert!(book.merge(&[peer(2, "b", &["10.0.0.2:47070"], 100), peer(1, "me", &["10.0.0.1:47070"], 5)], me));
        assert_eq!(book.peers.len(), 1, "own id is never added");

        assert!(book.merge(&[peer(2, "b", &["10.0.0.2:47070", "b.tailnet:47070"], 50)], me));
        let b = &book.peers[0];
        assert_eq!(b.addrs, vec!["10.0.0.2:47070", "b.tailnet:47070"]);
        assert_eq!(b.last_seen, 100, "older report does not lower last_seen");

        assert!(book.merge(&[peer(2, "b2", &[], 200)], me));
        assert_eq!(book.peers[0].last_seen, 200);
        assert_eq!(book.peers[0].node_name, "b2");

        assert!(!book.merge(&[peer(2, "b2", &["b.tailnet:47070"], 200)], me), "no change reports false");
    }

    #[test]
    fn merge_bounds_growth() {
        let me = NodeId([0; 16]);
        let mut book = PeerBook::default();
        let many: Vec<String> = (0..100).map(|i| format!("h{i}:1")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        book.merge(&[peer(2, "b", &refs, 1)], me);
        assert_eq!(book.peers[0].addrs.len(), MAX_ADDRS_PER_PEER);
        book.merge(&[peer(2, "b", &["extra:1"], 1)], me);
        assert_eq!(book.peers[0].addrs.len(), MAX_ADDRS_PER_PEER);
    }

    #[test]
    fn book_round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        assert!(PeerBook::load(dir.path()).unwrap().peers.is_empty());
        let mut book = PeerBook::default();
        book.merge(&[peer(2, "b", &["10.0.0.2:47070"], 100), peer(3, "c", &[], 0)], NodeId([1; 16]));
        book.save(dir.path()).unwrap();
        let back = PeerBook::load(dir.path()).unwrap();
        assert_eq!(back.peers, book.peers);
        std::fs::write(dir.path().join("peers.toml"), "peers = 5").unwrap();
        assert!(PeerBook::load(dir.path()).is_err());
    }

    /// Needs working multicast on the machine; run by hand:
    /// `cargo test mdns_smoke -- --ignored`.
    #[tokio::test]
    #[ignore = "needs multicast"]
    async fn mdns_smoke() {
        let (tx_a, mut rx_a) = tokio::sync::mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = tokio::sync::mpsc::unbounded_channel();
        let (tx_c, mut rx_c) = tokio::sync::mpsc::unbounded_channel();
        let a = Mdns::start(NodeId([0xaa; 16]), "farm0000000000000000000000000001", 41001, tx_a).unwrap();
        let b = Mdns::start(NodeId([0xbb; 16]), "farm0000000000000000000000000001", 41002, tx_b).unwrap();
        // Same network, other farm: must be invisible to a and b.
        let c = Mdns::start(NodeId([0xcc; 16]), "farm0000000000000000000000000002", 41003, tx_c).unwrap();
        let want = "bb".repeat(16);
        let got = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            while let Some((id, addr)) = rx_a.recv().await {
                assert_ne!(id, "cc".repeat(16), "other farm leaked");
                if id == want && addr.ends_with(":41002") {
                    return true;
                }
            }
            false
        })
        .await;
        assert!(got.unwrap_or(false), "node a never saw node b");
        let seen_b = rx_b.try_recv().map(|(id, _)| id);
        assert_ne!(seen_b, Ok("cc".repeat(16)));
        assert!(rx_c.try_recv().map(|(id, _)| id == "aa".repeat(16) || id == "bb".repeat(16)) != Ok(true));
        a.stop();
        b.stop();
        c.stop();
    }
}
