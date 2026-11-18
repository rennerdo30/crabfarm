//! The node daemon (DESIGN sections 2, 4, 9, 10): peer listener, control
//! connections and gossip, status, local IPC. Job execution for peers lives in
//! `executor`, job submission for local wrappers in `submitter`.

use crate::cas::BlobStore;
use crate::config::Config;
use crate::depgraph::DepGraph;
use crate::discovery::{CandidateSet, ConnectLimiter, MDNS_FOUND_QUEUE, Mdns, PeerBook};
use crate::exec::NodeJobserver;
use crate::farm::{self, Identity, Invites};
use crate::inputs::HashCache;
use crate::ipc;
use crate::link::Link;
use crate::proto::{BlobHash, LocalReason, Message, NodeId, PeerInfo, Status};
use crate::toolchain::Toolchains;
use crate::transport::{self, Accepted, FramedConn, TlsContext};
use anyhow::Context;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;

/// Status push interval while either side has work, and otherwise.
const STATUS_BUSY: Duration = Duration::from_secs(1);
const STATUS_IDLE: Duration = Duration::from_secs(10);
/// A control connection with no frame (status included) for this long is dead.
const CONTROL_SILENCE: Duration = Duration::from_secs(60);
/// A job connection with no frame in either direction for this long is closed
/// (the executor lifts it while rustc runs; the job timeout bounds that).
const JOB_IDLE: Duration = Duration::from_secs(600);
/// Inbound connections served at once, and of those the ones that have not yet
/// proved to be a farm member (TLS and first frame pending, or a join).
const MAX_INBOUND: usize = 256;
const MAX_UNAUTHENTICATED: usize = 8;
/// IPC connections that may wait for `IpcAuth` at once (CF-19).
pub const MAX_IPC_UNAUTHENTICATED: usize = 16;
/// How long an inbound connection may take to say what it is.
const FIRST_MESSAGE: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Cap for the per-peer set of blobs known to be on the peer.
pub const MAX_SENT_HASHES: usize = 200_000;

/// Caps on inbound connections, so a flood of connects (or of slow handshakes)
/// cannot exhaust tasks and memory. Excess connections are dropped at accept.
pub struct InboundLimits {
    all: Arc<Semaphore>,
    unauthenticated: Arc<Semaphore>,
}

/// Held for the life of an inbound connection; `unauthenticated` is dropped as
/// soon as the peer proved it is a farm member.
pub struct InboundPermit {
    _all: tokio::sync::OwnedSemaphorePermit,
    pub unauthenticated: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl InboundLimits {
    pub fn new(max_all: usize, max_unauthenticated: usize) -> InboundLimits {
        InboundLimits {
            all: Arc::new(Semaphore::new(max_all)),
            unauthenticated: Arc::new(Semaphore::new(max_unauthenticated)),
        }
    }

    pub fn admit(&self) -> Option<InboundPermit> {
        let all = self.all.clone().try_acquire_owned().ok()?;
        let unauth = self.unauthenticated.clone().try_acquire_owned().ok()?;
        Some(InboundPermit {
            _all: all,
            unauthenticated: Some(unauth),
        })
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the node knows about one peer.
pub struct PeerState {
    pub info: PeerInfo,
    pub host_triple: String,
    pub status: Option<Status>,
    /// Live control connections (inbound and outbound).
    pub conns: u32,
    /// EWMA of upload throughput to this peer, bits per second.
    pub bandwidth_bps: u64,
    /// Jobs this node has reserved or is about to reserve there.
    pub pending: u32,
    /// Blobs this node already sent to the peer (they stay in its store).
    pub sent: HashSet<BlobHash>,
    /// The address a job connection last opened on first; tried first next time.
    pub good_addr: Option<String>,
}

impl PeerState {
    fn new(info: PeerInfo) -> PeerState {
        PeerState {
            info,
            host_triple: String::new(),
            status: None,
            conns: 0,
            bandwidth_bps: crate::scheduler::DEFAULT_BANDWIDTH_BPS,
            pending: 0,
            sent: HashSet::new(),
            good_addr: None,
        }
    }
}

pub struct Node {
    pub data_dir: PathBuf,
    pub cfg: Config,
    pub id: Identity,
    pub tls: TlsContext,
    pub host_triple: String,
    pub store: BlobStore,
    pub jobserver: NodeJobserver,
    pub toolchains: Toolchains,
    pub peers: Mutex<HashMap<NodeId, PeerState>>,
    book: Mutex<PeerBook>,
    dialers: Mutex<HashSet<String>>,
    pub depgraph: Mutex<DepGraph>,
    pub hashes: Mutex<HashCache>,
    /// FIFO permits for local rustc runs (`config.local_slots`).
    pub local_sem: Arc<Semaphore>,
    /// Remote jobs currently reserved or running on this node.
    pub serving: AtomicU32,
    pub running_local: AtomicU32,
    pub running_remote: AtomicU32,
    counters: Mutex<BTreeMap<String, u64>>,
    invites: Mutex<Invites>,
    /// Limits concurrent input collection and hashing.
    pub prep_sem: Semaphore,
    status_tx: watch::Sender<u64>,
    pub shutdown: CancellationToken,
    pub listen_addr: SocketAddr,
    inbound: InboundLimits,
    /// A configured static peer is on this machine, so gossiped loopback
    /// addresses are meant (CF-19).
    allow_loopback_gossip: bool,
}

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding one of these locks leaves plain data behind.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Addresses other machines can reach a node listening on `listen` at.
/// A wildcard listen address expands to the non-loopback IPv4 addresses.
pub fn reachable_addrs(listen: SocketAddr) -> Vec<String> {
    if !listen.ip().is_unspecified() {
        return vec![listen.to_string()];
    }
    lan_ipv4s()
        .into_iter()
        .map(|ip| format!("{ip}:{}", listen.port()))
        .collect()
}

/// Non-loopback, non-link-local IPv4 addresses of this machine.
pub fn lan_ipv4s() -> Vec<std::net::Ipv4Addr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(v4) if !v4.is_link_local() => Some(v4),
            _ => None,
        })
        .collect()
}

impl Node {
    // ------------------------------------------------------------ counters

    pub fn count(&self, name: &str, by: u64) {
        *lock(&self.counters).entry(name.to_string()).or_insert(0) += by;
    }

    pub fn count_local(&self, reason: LocalReason) {
        self.count(&format!("local.{reason:?}"), 1);
    }

    pub fn counters_snapshot(&self) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = lock(&self.counters)
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        v.push(("local_slots".into(), self.cfg.local_slots as u64));
        v.push((
            "running_local".into(),
            self.running_local.load(Ordering::Relaxed) as u64,
        ));
        v.push((
            "running_remote".into(),
            self.running_remote.load(Ordering::Relaxed) as u64,
        ));
        v
    }

    // -------------------------------------------------------------- status

    pub fn own_status(&self) -> Status {
        let list = self.toolchains.list();
        Status {
            node_id: self.id.node_id,
            node_name: self.id.node_name.clone(),
            host_triple: self.host_triple.clone(),
            slots_total: self.cfg.slots,
            slots_busy: self.serving.load(Ordering::Relaxed),
            queue_len: 0,
            speed: crate::scheduler::REFERENCE_SPEED,
            toolchains: list.iter().map(|t| t.id.clone()).collect(),
            targets: list.iter().map(|t| t.targets.clone()).collect(),
            accepting: self.cfg.accept_remote_jobs,
        }
    }

    /// Tell every control connection to push a fresh status now.
    pub fn status_changed(&self) {
        self.status_tx.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn is_busy(&self) -> bool {
        self.serving.load(Ordering::Relaxed) > 0
            || self.running_local.load(Ordering::Relaxed) > 0
            || self.running_remote.load(Ordering::Relaxed) > 0
    }

    /// Take one serving slot, or `None` when full. The guard frees it.
    pub fn reserve_slot(self: &Arc<Self>) -> Option<SlotGuard> {
        loop {
            let cur = self.serving.load(Ordering::SeqCst);
            if cur >= self.cfg.slots {
                return None;
            }
            if self
                .serving
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                self.status_changed();
                return Some(SlotGuard(self.clone()));
            }
        }
    }

    pub fn peer_infos_for_status(&self) -> Vec<(PeerInfo, Option<Status>)> {
        let mut v: Vec<(PeerInfo, Option<Status>)> = lock(&self.peers)
            .values()
            .map(|p| {
                (
                    p.info.clone(),
                    if p.conns > 0 { p.status.clone() } else { None },
                )
            })
            .collect();
        v.sort_by(|a, b| a.0.node_name.cmp(&b.0.node_name));
        v
    }

    // -------------------------------------------------------------- peers

    fn gossip_list(&self) -> Vec<PeerInfo> {
        let mut v: Vec<PeerInfo> = lock(&self.peers).values().map(|p| p.info.clone()).collect();
        v.push(PeerInfo {
            node_id: self.id.node_id,
            node_name: self.id.node_name.clone(),
            addrs: reachable_addrs(self.listen_addr),
            last_seen: unix_now(),
        });
        v
    }

    /// Merge learned peers into the table and the persisted book, and start
    /// dialing the ones with addresses.
    pub fn learn_peers(self: &Arc<Self>, incoming: &[PeerInfo]) {
        let mut changed = false;
        {
            let mut book = lock(&self.book);
            if book.merge(incoming, self.id.node_id) {
                changed = true;
                if let Err(e) = book.save(&self.data_dir) {
                    tracing::warn!("could not save peers.toml: {e:#}");
                }
            }
        }
        let mut to_dial = Vec::new();
        {
            let book = lock(&self.book);
            let mut peers = lock(&self.peers);
            for p in &book.peers {
                if p.node_id == self.id.node_id {
                    continue;
                }
                let e = peers
                    .entry(p.node_id)
                    .or_insert_with(|| PeerState::new(p.clone()));
                if e.info.addrs != p.addrs {
                    e.info.addrs = p.addrs.clone();
                }
                if p.last_seen > e.info.last_seen {
                    e.info.last_seen = p.last_seen;
                    e.info.node_name = p.node_name.clone();
                }
                if !p.addrs.is_empty() {
                    to_dial.push(p.node_id);
                }
            }
        }
        let _ = changed;
        for id in to_dial {
            self.ensure_peer_dialer(id);
        }
    }

    fn has_control(&self, id: NodeId) -> bool {
        lock(&self.peers).get(&id).is_some_and(|p| p.conns > 0)
    }

    fn peer_connected(&self, id: NodeId, name: &str, host: &str) {
        let mut peers = lock(&self.peers);
        let e = peers.entry(id).or_insert_with(|| {
            PeerState::new(PeerInfo {
                node_id: id,
                node_name: name.to_string(),
                addrs: Vec::new(),
                last_seen: unix_now(),
            })
        });
        e.conns += 1;
        e.info.node_name = name.to_string();
        e.info.last_seen = unix_now();
        e.host_triple = host.to_string();
        tracing::info!(
            peer = crate::proto::log_safe(name).as_str(),
            "peer connected"
        );
    }

    fn peer_disconnected(&self, id: NodeId) {
        let mut peers = lock(&self.peers);
        if let Some(e) = peers.get_mut(&id) {
            e.conns = e.conns.saturating_sub(1);
            if e.conns == 0 {
                e.status = None;
                tracing::info!(
                    peer = crate::proto::log_safe(&e.info.node_name).as_str(),
                    "peer disconnected"
                );
            }
        }
    }

    // ------------------------------------------------------------ dialing

    fn ensure_peer_dialer(self: &Arc<Self>, id: NodeId) {
        let key = format!("id:{}", id.to_hex());
        if !lock(&self.dialers).insert(key) {
            return;
        }
        let node = self.clone();
        tokio::spawn(async move { node.dial_peer_loop(id).await });
    }

    fn ensure_addr_dialer(self: &Arc<Self>, addr: String) {
        if !lock(&self.dialers).insert(format!("addr:{addr}")) {
            return;
        }
        let node = self.clone();
        tokio::spawn(async move { node.dial_addr_loop(addr).await });
    }

    async fn sleep_or_shutdown(&self, d: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(d) => false,
            _ = self.shutdown.cancelled() => true,
        }
    }

    async fn dial_peer_loop(self: Arc<Self>, id: NodeId) {
        let mut backoff = Duration::from_secs(1);
        loop {
            if self.shutdown.is_cancelled() {
                return;
            }
            if self.has_control(id) {
                backoff = Duration::from_secs(1);
                if self.sleep_or_shutdown(Duration::from_secs(5)).await {
                    return;
                }
                continue;
            }
            let addrs = lock(&self.peers)
                .get(&id)
                .map(|p| p.info.addrs.clone())
                .unwrap_or_default();
            let mut connected = false;
            for a in addrs {
                match transport::connect(&self.tls, &self.id, &a).await {
                    Ok((pid, hello, conn)) if pid == id => {
                        connected = true;
                        self.clone()
                            .run_control(pid, hello, Link::new(conn), None)
                            .await;
                        break;
                    }
                    Ok(_) => tracing::debug!("address {a} answered as a different node"),
                    Err(e) => tracing::debug!("dial {a}: {e:#}"),
                }
            }
            backoff = if connected {
                Duration::from_secs(1)
            } else {
                (backoff * 2).min(MAX_BACKOFF)
            };
            if self.sleep_or_shutdown(backoff).await {
                return;
            }
        }
    }

    /// Dial an address without knowing the node behind it (static `peers`).
    async fn dial_addr_loop(self: Arc<Self>, addr: String) {
        let mut backoff = Duration::from_secs(1);
        loop {
            if self.shutdown.is_cancelled() {
                return;
            }
            match transport::connect(&self.tls, &self.id, &addr).await {
                Ok((pid, hello, conn)) => {
                    let name = match &hello {
                        Message::Hello { node_name, .. } => node_name.clone(),
                        _ => String::new(),
                    };
                    self.learn_peers(&[PeerInfo {
                        node_id: pid,
                        node_name: name,
                        addrs: vec![addr.clone()],
                        last_seen: unix_now(),
                    }]);
                    if self.has_control(pid) {
                        // Already connected the other way round.
                        drop(conn);
                        if self.sleep_or_shutdown(Duration::from_secs(10)).await {
                            return;
                        }
                        continue;
                    }
                    backoff = Duration::from_secs(1);
                    self.clone()
                        .run_control(pid, hello, Link::new(conn), None)
                        .await;
                }
                Err(e) => {
                    tracing::debug!("dial {addr}: {e:#}");
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
            if self.sleep_or_shutdown(backoff).await {
                return;
            }
        }
    }

    /// One connection attempt to an mDNS-learned candidate. A candidate that
    /// does not prove to be the farm member it announced is forgotten; a real
    /// member is handed to the normal per-node dial loop by `learn_peers`.
    async fn try_mdns_candidate(
        self: Arc<Self>,
        instance: String,
        addr: String,
        cands: Arc<Mutex<CandidateSet>>,
    ) {
        match transport::connect(&self.tls, &self.id, &addr).await {
            Ok((pid, hello, conn)) => {
                if pid.to_hex() != instance {
                    tracing::debug!("mDNS candidate {addr} answered as a different node");
                    return;
                }
                lock(&cands).mark_verified(&instance, &addr);
                let name = match &hello {
                    Message::Hello { node_name, .. } => node_name.clone(),
                    _ => String::new(),
                };
                self.learn_peers(&[PeerInfo {
                    node_id: pid,
                    node_name: name,
                    addrs: vec![addr],
                    last_seen: unix_now(),
                }]);
                if self.has_control(pid) {
                    return;
                }
                self.clone()
                    .run_control(pid, hello, Link::new(conn), None)
                    .await;
            }
            Err(e) => tracing::debug!("mDNS candidate {addr}: {e:#}"),
        }
    }

    // ------------------------------------------------------------ control

    /// Serve one control connection (either direction) until it ends.
    /// `first` is a message already read from an inbound connection.
    async fn run_control(
        self: Arc<Self>,
        peer: NodeId,
        hello: Message,
        mut link: Link,
        first: Option<Message>,
    ) {
        let Message::Hello {
            node_name,
            host_triple,
            ..
        } = &hello
        else {
            return;
        };
        self.peer_connected(peer, node_name, host_triple);
        if let Err(e) = self.control_loop(peer, &mut link, first).await {
            tracing::debug!("control connection ended: {e:#}");
        }
        self.peer_disconnected(peer);
    }

    async fn push_status(&self, link: &Link) -> anyhow::Result<()> {
        link.send(Message::StatusUpdate(self.own_status())).await
    }

    async fn control_loop(
        self: &Arc<Self>,
        peer: NodeId,
        link: &mut Link,
        first: Option<Message>,
    ) -> anyhow::Result<()> {
        link.send(Message::PeerList {
            peers: self.gossip_list(),
        })
        .await?;
        self.push_status(link).await?;
        if let Some(m) = first {
            self.on_control_message(peer, m);
        }
        let mut status_rx = self.status_tx.subscribe();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut last_push = Instant::now();
        let mut last_rx = Instant::now();
        loop {
            tokio::select! {
                m = link.recv() => {
                    let Some(m) = m else { return Ok(()) };
                    last_rx = Instant::now();
                    self.on_control_message(peer, m);
                }
                _ = tick.tick() => {
                    if last_rx.elapsed() > CONTROL_SILENCE {
                        anyhow::bail!("peer silent for {}s", CONTROL_SILENCE.as_secs());
                    }
                    let period = if self.is_busy() { STATUS_BUSY } else { STATUS_IDLE };
                    if last_push.elapsed() >= period {
                        self.push_status(link).await?;
                        last_push = Instant::now();
                    }
                }
                _ = status_rx.changed() => {
                    self.push_status(link).await?;
                    last_push = Instant::now();
                }
                _ = self.shutdown.cancelled() => return Ok(()),
            }
        }
    }

    fn on_control_message(self: &Arc<Self>, peer: NodeId, m: Message) {
        match m {
            Message::PeerList { peers } => {
                // Addresses from the network: only ones another machine can use (CF-19).
                let peers = crate::discovery::sanitize_gossip(&peers, self.allow_loopback_gossip);
                self.learn_peers(&peers)
            }
            Message::StatusUpdate(s) => {
                // A member may only report for itself.
                if s.node_id != peer {
                    tracing::debug!("status for a different node ignored");
                    return;
                }
                let mut peers = lock(&self.peers);
                if let Some(e) = peers.get_mut(&peer) {
                    e.info.last_seen = unix_now();
                    e.status = Some(s);
                }
            }
            _ => tracing::debug!("unexpected message on a control connection ignored"),
        }
    }

    // ---------------------------------------------------------- listeners

    async fn peer_listener(self: Arc<Self>, listener: tokio::net::TcpListener) {
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return,
                acc = listener.accept() => {
                    let Ok((tcp, _)) = acc else { continue };
                    let Some(permit) = self.inbound.admit() else {
                        tracing::debug!("inbound connection refused: too many connections");
                        continue;
                    };
                    let node = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = node.handle_inbound(tcp, permit).await {
                            tracing::debug!("inbound connection: {e:#}");
                        }
                    });
                }
            }
        }
    }

    async fn handle_inbound(
        self: Arc<Self>,
        tcp: tokio::net::TcpStream,
        mut permit: InboundPermit,
    ) -> anyhow::Result<()> {
        match transport::accept(&self.tls, &self.id, tcp).await? {
            // The join keeps its unauthenticated slot until the reply is sent.
            Accepted::Join { request, mut conn } => {
                let reply = {
                    let mut invites = lock(&self.invites);
                    farm::handle_join(&self.id, &mut invites, &self.data_dir, &request)?
                };
                if matches!(reply, Message::JoinAccepted { .. }) {
                    tracing::info!("a new node joined the farm");
                }
                conn.send(&reply).await?;
                Ok(())
            }
            Accepted::Peer {
                node_id,
                hello,
                conn,
            } => {
                permit.unauthenticated = None;
                let mut link = Link::new_with_idle(conn, Some(JOB_IDLE));
                let first = tokio::time::timeout(FIRST_MESSAGE, link.recv())
                    .await
                    .context("peer sent nothing after Hello")?;
                match first {
                    Some(Message::Reserve { job_id }) => {
                        self.serve_job(node_id, link, job_id).await;
                        Ok(())
                    }
                    Some(m @ (Message::PeerList { .. } | Message::StatusUpdate(_))) => {
                        link.set_idle(Some(CONTROL_SILENCE));
                        self.run_control(node_id, hello, link, Some(m)).await;
                        Ok(())
                    }
                    Some(_) => anyhow::bail!("unexpected first message from a peer"),
                    None => Ok(()),
                }
            }
        }
    }

    async fn ipc_listener(self: Arc<Self>, listener: tokio::net::TcpListener, token: [u8; 32]) {
        // Any local process can connect to the loopback port: at most
        // `MAX_IPC_UNAUTHENTICATED` connections may wait for their `IpcAuth`
        // (each for 2 s at most); the rest are dropped at accept (CF-19).
        let waiting = Arc::new(Semaphore::new(MAX_IPC_UNAUTHENTICATED));
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return,
                acc = listener.accept() => {
                    let Ok((tcp, _)) = acc else { continue };
                    let Ok(auth_slot) = waiting.clone().try_acquire_owned() else {
                        tracing::debug!("IPC connection refused: too many waiting for authentication");
                        continue;
                    };
                    tcp.set_nodelay(true).ok();
                    let node = self.clone();
                    tokio::spawn(async move {
                        let mut conn = FramedConn::new(tcp);
                        let authenticated = ipc::authenticate(&mut conn, &token).await;
                        // The slot is for the wait, not for the life of the connection.
                        drop(auth_slot);
                        if !authenticated {
                            tracing::debug!("IPC connection failed authentication");
                            return;
                        }
                        node.handle_ipc(Link::new(conn)).await;
                    });
                }
            }
        }
    }

    // ------------------------------------------------------------ housekeeping

    fn depgraph_path(&self) -> PathBuf {
        self.data_dir.join("depgraph.bin")
    }

    fn hashcache_path(&self) -> PathBuf {
        self.data_dir.join("hashcache.bin")
    }

    /// Write the caches that survive a restart.
    pub fn persist(&self) {
        if let Err(e) = lock(&self.depgraph).save(&self.depgraph_path()) {
            tracing::warn!("could not save the dependency graph: {e:#}");
        }
        let mut hashes = lock(&self.hashes);
        // A cache this big no longer pays for itself; start over.
        if hashes.entries.len() > 500_000 {
            hashes.entries.clear();
        }
        match postcard::to_stdvec(&*hashes) {
            Ok(bytes) => {
                let tmp = self.hashcache_path().with_extension("tmp");
                if std::fs::write(&tmp, bytes)
                    .and_then(|_| std::fs::rename(&tmp, self.hashcache_path()))
                    .is_err()
                {
                    tracing::warn!("could not save the hash cache");
                }
            }
            Err(e) => tracing::warn!("could not encode the hash cache: {e}"),
        }
    }

    async fn housekeeping(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        // Wrappers that found this node silent skip it for a few minutes; a
        // node that is running and answering says so soon (CF-25).
        let mut alive = tokio::time::interval(Duration::from_secs(5));
        let mut n = 0u32;
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return,
                _ = alive.tick() => {
                    ipc::clear_unresponsive(&self.data_dir);
                    continue;
                }
                _ = tick.tick() => {}
            }
            n += 1;
            let node = self.clone();
            let _ = tokio::task::spawn_blocking(move || {
                node.persist();
                // Every 10 minutes.
                if n.is_multiple_of(20)
                    && let Err(e) = node.store.evict()
                {
                    tracing::warn!("blob store eviction failed: {e:#}");
                }
            })
            .await;
        }
    }
}

/// Frees one serving slot when dropped.
pub struct SlotGuard(Arc<Node>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.serving.fetch_sub(1, Ordering::SeqCst);
        self.0.status_changed();
    }
}

/// Tracing to stderr and to `<data>/logs/node.log` (appended; rotation is P2).
/// `CRABFARM_LOG` selects the level (default `info`).
pub fn init_logging(data_dir: &Path) {
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct Tee(Option<Arc<Mutex<std::fs::File>>>);
    struct TeeWriter(Tee);
    impl std::io::Write for TeeWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let _ = std::io::stderr().write_all(buf);
            if let Some(f) = &(self.0).0 {
                let _ = lock(f).write_all(buf);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Tee {
        type Writer = TeeWriter;
        fn make_writer(&'a self) -> TeeWriter {
            TeeWriter(self.clone())
        }
    }
    let logs = data_dir.join("logs");
    let file = std::fs::create_dir_all(&logs)
        .ok()
        .and_then(|_| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(logs.join("node.log"))
                .ok()
        })
        .map(|f| Arc::new(Mutex::new(f)));
    let filter = tracing_subscriber::EnvFilter::try_from_env("CRABFARM_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(Tee(file))
        .try_init();
}

/// Exclusive claim on a data directory (CF-19): one node per directory.
/// Held for the life of the node; the operating system releases it when the
/// process ends, however it ends.
pub struct NodeLock {
    file: std::fs::File,
}

impl NodeLock {
    /// Take `<data>/node.lock`, or fail at once with a message that names the
    /// problem. Nothing else in the directory is touched: callers take this
    /// before logging, the job sweep, or binding any port.
    pub fn acquire(data_dir: &Path) -> anyhow::Result<NodeLock> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("create {}", data_dir.display()))?;
        let path = data_dir.join("node.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(NodeLock { file }),
            Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                "another crabfarm node is already running with this data directory ({}); stop it first, or set CRABFARM_HOME to run a second node",
                data_dir.display()
            ),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("lock {}", path.display()))
            }
        }
    }
}

impl NodeLock {
    /// Whether a node holds `<data>/node.lock` right now, without side
    /// effects: nothing is created (a missing directory or file means no node)
    /// and the file is only opened to ask for a shared lock. Being denied
    /// access to the file (a sharing violation) counts as held.
    pub fn is_held(data_dir: &Path) -> bool {
        let file = match std::fs::File::open(data_dir.join("node.lock")) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) => return true,
        };
        match file.try_lock_shared() {
            Ok(()) => {
                let _ = file.unlock();
                false
            }
            Err(std::fs::TryLockError::WouldBlock) => true,
            Err(std::fs::TryLockError::Error(_)) => false,
        }
    }
}

impl Drop for NodeLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Run the node until `shutdown` is cancelled. `_lock` is the data directory
/// claim from [`NodeLock::acquire`], kept until the node has shut down.
pub async fn run(
    data_dir: PathBuf,
    shutdown: CancellationToken,
    _lock: NodeLock,
) -> anyhow::Result<()> {
    let cfg = Config::load(&data_dir).context("read config.toml")?;
    let id = farm::load(&data_dir)?
        .context("this machine is not part of a farm: run `crabfarm farm create` or `crabfarm farm join <code>` first")?;
    for d in ["jobs", "cache", "logs"] {
        std::fs::create_dir_all(data_dir.join(d))
            .with_context(|| format!("create {d} directory"))?;
    }
    // Sandboxes left by a crashed node hold plaintext sources: remove them now,
    // and keep retrying in the background what cannot be removed yet (CF-9).
    crate::exec::sweep_jobs(&data_dir.join("jobs"));

    let listener = tokio::net::TcpListener::bind(&cfg.listen)
        .await
        .with_context(|| format!("listen on {}", cfg.listen))?;
    let listen_addr = listener.local_addr()?;
    let (ipc_listener, ipc_token) = ipc::bind(&data_dir).await?;
    ipc::write_private(
        &data_dir.join("listen.addr"),
        listen_addr.to_string().as_bytes(),
    )?;

    let dd = data_dir.clone();
    let auto = cfg.auto_install;
    let max_auto = cfg.max_auto_toolchains;
    let (toolchains, host_triple) = tokio::task::spawn_blocking(move || {
        let host = crate::toolchain::query(Path::new("rustc"))
            .map(|t| t.host)
            .ok();
        (Toolchains::scan_with(&dd, auto, max_auto), host)
    })
    .await?;
    let toolchains = toolchains.context("scan toolchains")?;
    let host_triple = match host_triple {
        Some(h) => h,
        None => {
            tracing::warn!("`rustc -vV` failed; using a guess for the host triple");
            format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
        }
    };
    let mut tls = TlsContext::new(&id)?;
    tls.host_triple = host_triple.clone();

    let depgraph = DepGraph::load(&data_dir.join("depgraph.bin"));
    let hashes: HashCache = std::fs::read(data_dir.join("hashcache.bin"))
        .ok()
        .and_then(|b| postcard::from_bytes(&b).ok())
        .unwrap_or_default();
    let (status_tx, _) = watch::channel(0u64);
    let node = Arc::new(Node {
        store: BlobStore::open(
            &data_dir.join("cache"),
            cfg.cache_max_gb.saturating_mul(1 << 30),
        )?,
        jobserver: NodeJobserver::new(cfg.slots)?,
        toolchains,
        peers: Mutex::new(HashMap::new()),
        book: Mutex::new(PeerBook::load(&data_dir)?),
        dialers: Mutex::new(HashSet::new()),
        depgraph: Mutex::new(depgraph),
        hashes: Mutex::new(hashes),
        local_sem: Arc::new(Semaphore::new(cfg.local_slots.max(1) as usize)),
        serving: AtomicU32::new(0),
        running_local: AtomicU32::new(0),
        running_remote: AtomicU32::new(0),
        counters: Mutex::new(BTreeMap::new()),
        invites: Mutex::new(Invites::load(&data_dir)?),
        prep_sem: Semaphore::new(8),
        status_tx,
        shutdown: shutdown.clone(),
        listen_addr,
        inbound: InboundLimits::new(MAX_INBOUND, MAX_UNAUTHENTICATED),
        allow_loopback_gossip: cfg
            .peers
            .iter()
            .any(|p| crate::discovery::is_loopback_peer(p)),
        host_triple,
        tls,
        id,
        cfg,
        data_dir: data_dir.clone(),
    });

    tracing::info!(
        "node {} listening on {} (local slots {}, serving slots {}, host {})",
        node.id.node_name,
        listen_addr,
        node.cfg.local_slots,
        node.cfg.slots,
        node.host_triple
    );

    // Peers from earlier runs and from the config.
    let known = lock(&node.book).peers.clone();
    node.learn_peers(&known);
    for a in node.cfg.peers.clone() {
        node.ensure_addr_dialer(a);
    }
    let mdns = if node.cfg.mdns {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, String)>(MDNS_FOUND_QUEUE);
        match Mdns::start(
            node.id.node_id,
            &node.id.short_farm_id(),
            listen_addr.port(),
            tx,
        ) {
            Ok(m) => {
                let n = node.clone();
                tokio::spawn(async move {
                    // Unauthenticated multicast feeds this: bounded candidates and
                    // rate-limited attempts (DESIGN section 4).
                    let cands = Arc::new(Mutex::new(CandidateSet::default()));
                    let mut limiter = ConnectLimiter::default();
                    while let Some((instance, addr)) = rx.recv().await {
                        if !lock(&cands).offer(&instance, &addr)
                            || !limiter.allow(&instance, &addr, Instant::now())
                        {
                            continue;
                        }
                        let (n, cands) = (n.clone(), cands.clone());
                        tokio::spawn(
                            async move { n.try_mdns_candidate(instance, addr, cands).await },
                        );
                    }
                });
                Some(m)
            }
            Err(e) => {
                tracing::warn!(
                    "mDNS unavailable ({e:#}); only configured and learned peers are used"
                );
                None
            }
        }
    } else {
        None
    };

    let tasks = vec![
        tokio::spawn(node.clone().peer_listener(listener)),
        tokio::spawn(node.clone().ipc_listener(ipc_listener, ipc_token)),
        tokio::spawn(node.clone().housekeeping()),
    ];

    shutdown.cancelled().await;
    tracing::info!("node shutting down");
    ipc::unpublish(&data_dir);
    node.toolchains.shutdown();
    if let Some(m) = mdns {
        m.stop();
    }
    for t in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
    }
    let n = node.clone();
    let _ = tokio::task::spawn_blocking(move || n.persist()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_limits_cap_total_and_unauthenticated() {
        let l = InboundLimits::new(5, 2);
        let a = l.admit().unwrap();
        let b = l.admit().unwrap();
        assert!(
            l.admit().is_none(),
            "third unauthenticated connection refused"
        );
        // Authenticating frees the unauthenticated slot but keeps the connection slot.
        let mut a = a;
        a.unauthenticated = None;
        let c = l.admit().unwrap();
        assert!(l.admit().is_none());
        drop((b, c));
        assert!(l.admit().is_some(), "dropped connections free their slots");

        // The total cap: authenticated connections still count.
        let l = InboundLimits::new(3, 8);
        let mut held = Vec::new();
        for _ in 0..3 {
            let mut p = l.admit().unwrap();
            p.unauthenticated = None;
            held.push(p);
        }
        assert!(l.admit().is_none(), "total cap reached");
        held.pop();
        assert!(l.admit().is_some());
    }
}
