//! The Jamlink node: one task that owns all protocol state, plus small tasks
//! per WebSocket connection, for accepting, for dialling and for UDP discovery.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, LineWriter, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::proto::{self, Entity};
use crate::schema;

const TICK: Duration = Duration::from_millis(100);
const HEARTBEAT_AFTER: Duration = Duration::from_secs(1);
const SILENCE_WARN: Duration = Duration::from_millis(2500);
const PEER_TIMEOUT: Duration = Duration::from_secs(5);
const DIAL_FALLBACK: Duration = Duration::from_secs(5);
const REDIAL_INTERVAL: Duration = Duration::from_secs(3);
const RESULT_TIMEOUT: Duration = Duration::from_secs(5);
const AUTO_DECLINE_AFTER: Duration = Duration::from_secs(4);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const FORGET_UNSEEN_PEER: Duration = Duration::from_secs(15);
const MAX_RATE_HZ: usize = 20;
/// Text that marks a deliberate test-peer probe. Rule breaks found in such
/// messages are reported as [`Severity::Info`] instead of failures.
pub(crate) const PROBE_MARKER: &str = "jamlinkProbe";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Settings for a node.
#[derive(Clone, Debug)]
pub struct Config {
    /// Human-readable game name.
    pub name: String,
    /// The game's own faction, lowercase snake_case.
    pub faction: String,
    pub team: Option<String>,
    /// Standard and custom events the game acts on (a hint in `hello`).
    pub handles: Vec<String>,
    /// WebSocket ports to try, in order.
    pub ports: std::ops::RangeInclusive<u16>,
    /// `host:port` addresses to dial directly.
    pub seeds: Vec<String>,
    /// Send and listen for UDP beacons.
    pub discovery: bool,
    /// Append every sent and received message to this file as JSON lines.
    pub log_path: Option<PathBuf>,
}

impl Config {
    pub fn new(name: impl Into<String>, faction: impl Into<String>) -> Self {
        Config {
            name: name.into(),
            faction: faction.into(),
            team: None,
            handles: Vec::new(),
            ports: proto::WS_PORTS,
            seeds: Vec::new(),
            discovery: true,
            log_path: None,
        }
    }
}

/// What a peer said about itself in `hello`.
#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub peer_id: String,
    pub name: String,
    pub faction: String,
    pub team: Option<String>,
    pub version: String,
    pub handles: Vec<String>,
    pub host: String,
    pub port: u16,
}

/// An event sent to one of our entities.
#[derive(Clone, Debug)]
pub struct IncomingEvent {
    pub event_id: String,
    pub event: String,
    pub target: String,
    pub source: Option<String>,
    pub data: Value,
    pub want_result: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Worth knowing, not a rule break.
    Info,
    /// A SHOULD was broken, or something looks wrong but may be legitimate.
    Warn,
    /// A MUST was broken.
    Fail,
}

/// A rule break found in another peer's traffic (or in our own, when `peer_id` is `None`).
#[derive(Clone, Debug)]
pub struct Violation {
    pub peer_id: Option<String>,
    pub peer_name: Option<String>,
    pub severity: Severity,
    /// Short stable identifier, for grouping repeats.
    pub code: &'static str,
    /// Plain-language description.
    pub text: String,
}

/// Everything the node tells the game.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Notice {
    /// A peer finished `hello`; its snapshot follows.
    PeerJoined(PeerInfo),
    /// A peer is gone. Its entities have already been reported removed.
    PeerLeft {
        peer_id: String,
        name: String,
        graceful: bool,
    },
    /// A peer finished sending its snapshot.
    SnapshotEnd {
        peer_id: String,
        count: u64,
    },
    /// A replica was created or replaced. `new` is true the first time.
    EntityUpdated {
        entity: Entity,
        new: bool,
    },
    /// A replica is gone. `reason` is the owner's reason, or `peer_left`.
    EntityRemoved {
        id: String,
        owner: String,
        reason: String,
    },
    /// An event for one of our entities. Reply with [`Handle::reply`] when `want_result` is set.
    Event {
        from: String,
        event: IncomingEvent,
    },
    /// The answer to one of our events. `reason` is `timeout` or `peer_left` when no answer came.
    EventResult {
        event_id: String,
        ok: bool,
        reason: Option<String>,
        data: Value,
    },
    Chat {
        from: String,
        name: String,
        text: String,
    },
    /// A peer offers us one of its entities. Answer with [`Handle::reply_transfer`].
    TransferOffer {
        from: String,
        id: String,
    },
    /// The answer to one of our transfer offers. `timed_out` is true when no answer came.
    TransferAnswered {
        id: String,
        to: String,
        accepted: bool,
        timed_out: bool,
    },
    /// An entity changed owner. When `to` is our peer ID, we now own it and must keep it updated.
    OwnershipChanged {
        id: String,
        from: String,
        to: String,
    },
    /// A peer sent us an `error` message about something we sent.
    PeerError {
        peer_id: String,
        code: String,
        message: Option<String>,
    },
    Violation(Violation),
}

/// Cheap, cloneable handle for driving a node.
#[derive(Clone)]
pub struct Handle {
    tx: UnboundedSender<Cmd>,
    peer_id: Arc<str>,
    port: u16,
    entity_counter: Arc<AtomicU64>,
    event_counter: Arc<AtomicU64>,
}

impl Handle {
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }
    /// The WebSocket port the node listens on.
    pub fn port(&self) -> u16 {
        self.port
    }
    /// A fresh entity ID, `"<peerId>:<n>"`.
    pub fn new_entity_id(&self) -> String {
        format!("{}:{}", self.peer_id, self.entity_counter.fetch_add(1, Ordering::Relaxed))
    }
    /// Creates or replaces one of our entities and broadcasts it. `owner` and `t` are filled in.
    /// Call it at up to 10 Hz for moving entities, and on change otherwise.
    pub fn upsert(&self, entity: Entity) {
        let _ = self.tx.send(Cmd::Upsert(entity));
    }
    /// Removes one of our entities. `reason`: `destroyed`, `taken` or `despawned`.
    pub fn remove(&self, id: &str, reason: &str) {
        let _ = self.tx.send(Cmd::Remove { id: id.into(), reason: reason.into() });
    }
    /// Sends an event to the owner of `target`. Returns the event ID. When `want_result` is set,
    /// a [`Notice::EventResult`] always follows, from the owner or from a timeout.
    pub fn send_event(
        &self,
        target: &str,
        event: &str,
        data: Value,
        source: Option<&str>,
        want_result: bool,
    ) -> String {
        self.send_event_inner(None, target, event, data, source, want_result)
    }
    /// Like [`Handle::send_event`], but sent to `peer` whoever owns the target. For tests.
    pub fn send_event_to(&self, peer: &str, target: &str, event: &str, data: Value, want_result: bool) -> String {
        self.send_event_inner(Some(peer.into()), target, event, data, None, want_result)
    }
    fn send_event_inner(
        &self,
        to: Option<String>,
        target: &str,
        event: &str,
        data: Value,
        source: Option<&str>,
        want_result: bool,
    ) -> String {
        let event_id = format!("{}:e{}", self.peer_id, self.event_counter.fetch_add(1, Ordering::Relaxed));
        let _ = self.tx.send(Cmd::Event {
            to,
            event_id: event_id.clone(),
            target: target.into(),
            event: event.into(),
            data,
            source: source.map(Into::into),
            want_result,
        });
        event_id
    }
    /// Answers an event that asked for a result.
    pub fn reply(&self, to: &str, event_id: &str, ok: bool, reason: Option<&str>, data: Option<Value>) {
        let _ = self.tx.send(Cmd::Reply {
            to: to.into(),
            event_id: event_id.into(),
            ok,
            reason: reason.map(Into::into),
            data,
        });
    }
    pub fn chat(&self, name: &str, text: &str) {
        let _ = self.tx.send(Cmd::Chat { name: name.into(), text: text.into() });
    }
    /// Offers one of our entities to `to`. A [`Notice::TransferAnswered`] follows.
    pub fn offer_transfer(&self, id: &str, to: &str) {
        let _ = self.tx.send(Cmd::OfferTransfer { id: id.into(), to: to.into() });
    }
    /// Answers a [`Notice::TransferOffer`].
    pub fn reply_transfer(&self, to: &str, id: &str, accept: bool) {
        let _ = self.tx.send(Cmd::ReplyTransfer { to: to.into(), id: id.into(), accept });
    }
    /// Sends text to a peer without validation. Only for conformance tests.
    pub fn send_raw_text(&self, peer: &str, text: impl Into<String>) {
        let _ = self.tx.send(Cmd::RawText { peer: peer.into(), text: text.into() });
    }
    /// Says `bye` to everyone and stops the node.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Cmd::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

/// Starts a node: binds the first free WebSocket port in `config.ports`,
/// starts discovery and dials the seeds.
pub async fn start(config: Config) -> io::Result<(Handle, UnboundedReceiver<Notice>)> {
    let mut listener = None;
    let mut last_err = None;
    for port in config.ports.clone() {
        match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => {
                listener = Some(l);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let listener = listener
        .ok_or_else(|| last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::AddrInUse, "no port in range")))?;
    let port = listener.local_addr()?.port();

    let me = proto::new_peer_id();
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (in_tx, in_rx) = unbounded_channel();
    let (notice_tx, notice_rx) = unbounded_channel();
    let conn_ids = Arc::new(AtomicU64::new(1));

    let log = match &config.log_path {
        Some(p) => Some(LineWriter::new(std::fs::OpenOptions::new().create(true).append(true).open(p)?)),
        None => None,
    };

    tokio::spawn(accept_loop(listener, in_tx.clone(), conn_ids.clone()));

    let mut notices_early = Vec::new();
    if config.discovery {
        match bind_beacon_socket() {
            Ok(sock) => {
                let beacon = json!({
                    "type": "beacon", "protocol": "jamlink", "version": proto::VERSION,
                    "peerId": me, "name": config.name, "port": port
                })
                .to_string();
                tokio::spawn(beacon_loop(sock, beacon, in_tx.clone()));
            }
            Err(e) => notices_early.push(Notice::Violation(Violation {
                peer_id: None,
                peer_name: None,
                severity: Severity::Warn,
                code: "beacon_bind",
                text: format!("could not bind UDP {}: {e}; discovery is off, use seeds", proto::BEACON_PORT),
            })),
        }
    }

    let seeds = config
        .seeds
        .iter()
        .filter_map(|s| {
            let (host, port) = s.rsplit_once(':')?;
            Some(Seed {
                host: host.to_string(),
                port: port.parse().ok()?,
                peer: None,
                last_attempt: None,
                refused: false,
            })
        })
        .collect();

    let core = Core {
        me: me.clone(),
        cfg: config,
        port,
        start: Instant::now(),
        in_tx,
        notices: notice_tx,
        conn_ids,
        conns: HashMap::new(),
        known: HashMap::new(),
        refused: HashSet::new(),
        dialing: HashSet::new(),
        seeds,
        own: HashMap::new(),
        replicas: HashMap::new(),
        last_t: HashMap::new(),
        rates: HashMap::new(),
        rate_warned: HashSet::new(),
        pending_results: HashMap::new(),
        pending_incoming: HashMap::new(),
        offers_out: HashMap::new(),
        rebroadcast: Vec::new(),
        log,
    };
    for n in notices_early {
        let _ = core.notices.send(n);
    }
    tokio::spawn(core.run(cmd_rx, in_rx));

    Ok((
        Handle {
            tx: cmd_tx,
            peer_id: me.into(),
            port,
            entity_counter: Arc::new(AtomicU64::new(1)),
            event_counter: Arc::new(AtomicU64::new(1)),
        },
        notice_rx,
    ))
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

#[allow(clippy::large_enum_variant)]
enum Cmd {
    Upsert(Entity),
    Remove {
        id: String,
        reason: String,
    },
    Event {
        to: Option<String>,
        event_id: String,
        target: String,
        event: String,
        data: Value,
        source: Option<String>,
        want_result: bool,
    },
    Reply {
        to: String,
        event_id: String,
        ok: bool,
        reason: Option<String>,
        data: Option<Value>,
    },
    Chat {
        name: String,
        text: String,
    },
    OfferTransfer {
        id: String,
        to: String,
    },
    ReplyTransfer {
        to: String,
        id: String,
        accept: bool,
    },
    RawText {
        peer: String,
        text: String,
    },
    Shutdown(oneshot::Sender<()>),
}

enum In {
    Opened { conn: u64, tx: UnboundedSender<Out>, outbound: bool, host: String, key: Option<DialKey> },
    DialFailed { key: DialKey },
    Text { conn: u64, text: String },
    Binary { conn: u64 },
    Closed { conn: u64 },
    Beacon { text: String, from: SocketAddr },
}

enum Out {
    Text(String),
    Close(u16, &'static str),
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum DialKey {
    Peer(String),
    Seed(usize),
}

struct Conn {
    tx: UnboundedSender<Out>,
    outbound: bool,
    host: String,
    seed: Option<usize>,
    opened: Instant,
    last_rx: Instant,
    last_tx: Instant,
    silence_warned: bool,
    info: Option<PeerInfo>,
    pre_hello_msgs: u32,
    snapshot_done: bool,
    snapshot_ids: HashSet<String>,
    got_bye: bool,
    fallback_dial: bool,
    closing: bool,
}

struct Known {
    host: String,
    port: u16,
    /// Set once we have had a working connection; later re-dials are not dial-rule evidence.
    connected_before: bool,
    first_seen: Instant,
    last_seen: Instant,
    last_dial: Option<Instant>,
}

struct Seed {
    host: String,
    port: u16,
    peer: Option<String>,
    last_attempt: Option<Instant>,
    /// Speaks an incompatible major version; never dialled again.
    refused: bool,
}

struct PendingResult {
    to: String,
    deadline: Instant,
}

struct Core {
    me: String,
    cfg: Config,
    port: u16,
    start: Instant,
    in_tx: UnboundedSender<In>,
    notices: UnboundedSender<Notice>,
    conn_ids: Arc<AtomicU64>,
    conns: HashMap<u64, Conn>,
    known: HashMap<String, Known>,
    /// Peers with an incompatible major version; never dialled again.
    refused: HashSet<String>,
    dialing: HashSet<DialKey>,
    seeds: Vec<Seed>,
    own: HashMap<String, Entity>,
    replicas: HashMap<String, Entity>,
    last_t: HashMap<String, u64>,
    rates: HashMap<String, VecDeque<Instant>>,
    rate_warned: HashSet<String>,
    pending_results: HashMap<String, PendingResult>,
    /// Events we delivered to the game that want a result: (from, eventId) -> deadline.
    pending_incoming: HashMap<(String, String), Instant>,
    offers_out: HashMap<String, (String, Instant)>,
    rebroadcast: Vec<(String, Instant)>,
    log: Option<LineWriter<std::fs::File>>,
}

impl Core {
    async fn run(mut self, mut cmds: UnboundedReceiver<Cmd>, mut ins: UnboundedReceiver<In>) {
        let mut tick = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(Cmd::Shutdown(done)) => {
                        self.shutdown(&mut ins).await;
                        let _ = done.send(());
                        return;
                    }
                    Some(cmd) => self.on_cmd(cmd),
                    None => { self.shutdown(&mut ins).await; return; }
                },
                Some(msg) = ins.recv() => self.on_in(msg),
                _ = tick.tick() => self.on_tick(),
            }
        }
    }

    async fn shutdown(&mut self, ins: &mut UnboundedReceiver<In>) {
        let bye = json!({"type": "bye", "reason": "quit"});
        let ids: Vec<u64> = self.conns.keys().copied().collect();
        for id in ids {
            if self.conns[&id].info.is_some() {
                self.send(id, &bye);
            }
            self.close(id, 1000, "bye");
        }
        // Wait for the close handshakes, so `bye` is not lost to a connection reset.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
        while !self.conns.is_empty() {
            match tokio::time::timeout_at(deadline, ins.recv()).await {
                Ok(Some(In::Closed { conn })) => {
                    self.conns.remove(&conn);
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }
    }

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn notify(&self, n: Notice) {
        let _ = self.notices.send(n);
    }

    fn violation(&self, conn: Option<u64>, severity: Severity, code: &'static str, text: String) {
        let c = conn.and_then(|c| self.conns.get(&c));
        let info = c.and_then(|c| c.info.as_ref());
        // Before hello the peer is unidentified; name it by its address.
        let unidentified = c.filter(|c| c.info.is_none()).map(|c| format!("unidentified peer at {}", c.host));
        self.notify(Notice::Violation(Violation {
            peer_id: info.map(|i| i.peer_id.clone()),
            peer_name: info.map(|i| i.name.clone()).or(unidentified),
            severity,
            code,
            text,
        }));
    }

    fn log(&mut self, dir: &str, conn: u64, text: &str) {
        let peer = self.conns.get(&conn).and_then(|c| c.info.as_ref()).map(|i| i.peer_id.clone());
        let t = self.now_ms();
        if let Some(log) = &mut self.log {
            let msg: Value = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()));
            let line = json!({"t": t, "dir": dir, "conn": conn, "peer": peer, "msg": msg});
            let _ = writeln!(log, "{line}");
        }
    }

    // --- sending -----------------------------------------------------------

    fn send(&mut self, conn: u64, msg: &Value) {
        if let Err(errors) = schema::check(msg) {
            self.violation(
                None,
                Severity::Fail,
                "self_invalid",
                format!("BUG in this game: about to send an invalid {} message: {}", msg_type(msg), errors.join("; ")),
            );
        }
        self.send_text(conn, msg.to_string());
    }

    fn send_text(&mut self, conn: u64, text: String) {
        self.log("out", conn, &text);
        if let Some(c) = self.conns.get_mut(&conn) {
            if c.closing {
                return;
            }
            c.last_tx = Instant::now();
            let _ = c.tx.send(Out::Text(text));
        }
    }

    /// Connections that finished hello, one per peer.
    fn live_conns(&self) -> Vec<u64> {
        let mut v: Vec<u64> =
            self.conns.iter().filter(|(_, c)| c.info.is_some() && !c.closing).map(|(id, _)| *id).collect();
        v.sort();
        v
    }

    fn broadcast(&mut self, msg: &Value) {
        for id in self.live_conns() {
            self.send(id, msg);
        }
    }

    fn conn_of(&self, peer: &str) -> Option<u64> {
        self.conns
            .iter()
            .filter(|(_, c)| !c.closing && c.info.as_ref().is_some_and(|i| i.peer_id == peer))
            .map(|(id, _)| *id)
            .min()
    }

    fn close(&mut self, conn: u64, code: u16, reason: &'static str) {
        if let Some(c) = self.conns.get_mut(&conn) {
            c.closing = true;
            let _ = c.tx.send(Out::Close(code, reason));
        }
    }

    fn peer_list(&self) -> Value {
        let mut seen = HashSet::new();
        let list: Vec<Value> = self
            .conns
            .values()
            .filter(|c| !c.closing)
            .filter_map(|c| c.info.as_ref())
            .filter(|i| seen.insert(i.peer_id.clone()))
            .map(|i| json!({"peerId": i.peer_id, "host": i.host, "port": i.port}))
            .collect();
        Value::Array(list)
    }

    fn hello(&self) -> Value {
        let mut h = json!({
            "type": "hello", "protocol": "jamlink", "version": proto::VERSION,
            "peerId": self.me, "name": self.cfg.name, "faction": self.cfg.faction,
            "port": self.port, "handles": self.cfg.handles, "peers": self.peer_list(),
        });
        if let Some(team) = &self.cfg.team {
            h["team"] = Value::from(team.clone());
        }
        h
    }

    // --- commands from the game -------------------------------------------

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Upsert(mut e) => {
                if self.replicas.contains_key(&e.id) {
                    self.violation(None, Severity::Fail, "self_not_owner", format!(
                        "BUG in this game: tried to update entity {}, which belongs to another peer; send it an event instead", e.id));
                    return;
                }
                e.owner = self.me.clone();
                e.t = self.now_ms();
                let msg = e.to_message();
                self.own.insert(e.id.clone(), e);
                self.broadcast(&msg);
            }
            Cmd::Remove { id, reason } => {
                if self.own.remove(&id).is_some() {
                    self.offers_out.remove(&id);
                    self.broadcast(&json!({"type": "remove", "id": id, "reason": reason}));
                }
            }
            Cmd::Event { to, event_id, target, event, data, source, want_result } => {
                let owner = to.or_else(|| self.replicas.get(&target).map(|e| e.owner.clone()));
                let conn = owner.as_deref().and_then(|o| self.conn_of(o));
                let (Some(owner), Some(conn)) = (owner, conn) else {
                    if want_result {
                        self.notify(Notice::EventResult {
                            event_id,
                            ok: false,
                            reason: Some("unknown_target".into()),
                            data: json!({}),
                        });
                    }
                    return;
                };
                let mut msg =
                    json!({"type": "event", "eventId": event_id, "event": event, "target": target, "data": data});
                if let Some(s) = source {
                    msg["source"] = Value::from(s);
                }
                if want_result {
                    msg["wantResult"] = Value::from(true);
                    self.pending_results
                        .insert(event_id, PendingResult { to: owner, deadline: Instant::now() + RESULT_TIMEOUT });
                }
                self.send(conn, &msg);
            }
            Cmd::Reply { to, event_id, ok, reason, data } => {
                if self.pending_incoming.remove(&(to.clone(), event_id.clone())).is_none() {
                    return; // already answered, timed out, or never asked for
                }
                self.send_result(&to, &event_id, ok, reason.as_deref(), data);
            }
            Cmd::Chat { name, text } => self.broadcast(&json!({"type": "chat", "name": name, "text": text})),
            Cmd::OfferTransfer { id, to } => {
                let Some(conn) = self.conn_of(&to) else {
                    self.notify(Notice::TransferAnswered { id, to, accepted: false, timed_out: true });
                    return;
                };
                if !self.own.contains_key(&id) || self.offers_out.contains_key(&id) {
                    self.notify(Notice::TransferAnswered { id, to, accepted: false, timed_out: true });
                    return;
                }
                self.offers_out.insert(id.clone(), (to, Instant::now() + TRANSFER_TIMEOUT));
                self.send(conn, &json!({"type": "transfer_offer", "id": id}));
            }
            Cmd::ReplyTransfer { to, id, accept } => {
                if let Some(conn) = self.conn_of(&to) {
                    self.send(conn, &json!({"type": "transfer_reply", "id": id, "accept": accept}));
                }
            }
            Cmd::RawText { peer, text } => {
                if let Some(conn) = self.conn_of(&peer) {
                    self.send_text(conn, text);
                }
            }
            Cmd::Shutdown(_) => unreachable!("handled in run"),
        }
    }

    fn send_result(&mut self, to: &str, event_id: &str, ok: bool, reason: Option<&str>, data: Option<Value>) {
        let Some(conn) = self.conn_of(to) else { return };
        let mut msg = json!({"type": "result", "eventId": event_id, "ok": ok});
        if let Some(r) = reason {
            msg["reason"] = Value::from(r);
        }
        if let Some(d) = data {
            msg["data"] = d;
        }
        self.send(conn, &msg);
    }

    // --- network input -----------------------------------------------------

    fn on_in(&mut self, msg: In) {
        match msg {
            In::Opened { conn, tx, outbound, host, key } => {
                let seed = match &key {
                    Some(DialKey::Seed(i)) => Some(*i),
                    _ => None,
                };
                let fallback_dial = matches!(&key, Some(DialKey::Peer(p))
                    if *p < self.me && self.known.get(p).is_some_and(|k| !k.connected_before));
                if let Some(k) = key {
                    self.dialing.remove(&k);
                }
                let now = Instant::now();
                self.conns.insert(
                    conn,
                    Conn {
                        tx,
                        outbound,
                        host,
                        seed,
                        opened: now,
                        last_rx: now,
                        last_tx: now,
                        silence_warned: false,
                        info: None,
                        pre_hello_msgs: 0,
                        snapshot_done: false,
                        snapshot_ids: HashSet::new(),
                        got_bye: false,
                        fallback_dial,
                        closing: false,
                    },
                );
                let hello = self.hello();
                self.send(conn, &hello);
            }
            In::DialFailed { key } => {
                self.dialing.remove(&key);
            }
            In::Text { conn, text } => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.last_rx = Instant::now();
                    c.silence_warned = false;
                }
                self.log("in", conn, &text);
                self.on_text(conn, text);
            }
            In::Binary { conn } => {
                self.violation(
                    Some(conn),
                    Severity::Fail,
                    "binary_frame",
                    "sent a binary WebSocket frame; Jamlink uses text frames only".into(),
                );
            }
            In::Closed { conn } => self.on_closed(conn),
            In::Beacon { text, from } => self.on_beacon(&text, from),
        }
    }

    fn on_beacon(&mut self, text: &str, from: SocketAddr) {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return };
        if v.get("type").and_then(Value::as_str) != Some("beacon") {
            return;
        }
        if let Err(errors) = schema::check(&v) {
            self.notify(Notice::Violation(Violation {
                peer_id: None,
                peer_name: Some(format!("beacon from {}", from.ip())),
                severity: Severity::Fail,
                code: "invalid_beacon",
                text: format!("sent an invalid beacon: {}", errors.join("; ")),
            }));
            return;
        }
        let peer = v["peerId"].as_str().unwrap_or_default().to_string();
        let major = v["version"].as_str().and_then(proto::major_of);
        if peer == self.me || major != Some(proto::MAJOR) {
            return;
        }
        let port = v["port"].as_u64().unwrap_or(0) as u16;
        self.learn_peer(peer, from.ip().to_string(), port);
    }

    fn learn_peer(&mut self, peer: String, host: String, port: u16) {
        if peer == self.me || self.refused.contains(&peer) {
            return;
        }
        let now = Instant::now();
        let k = self.known.entry(peer).or_insert(Known {
            host: host.clone(),
            port,
            connected_before: false,
            first_seen: now,
            last_seen: now,
            last_dial: None,
        });
        k.host = host;
        k.port = port;
        k.last_seen = now;
    }

    fn on_text(&mut self, conn: u64, text: String) {
        let probe = text.contains(PROBE_MARKER);
        let sev = |s: Severity| if probe { Severity::Info } else { s };
        if text.len() > proto::MAX_MESSAGE_BYTES {
            self.violation(
                Some(conn),
                sev(Severity::Fail),
                "too_large",
                format!("sent a {} byte message; the limit is 64 KiB", text.len()),
            );
            self.send(conn, &json!({"type": "error", "code": "too_large"}));
            return;
        }
        let msg: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                self.violation(
                    Some(conn),
                    sev(Severity::Fail),
                    "not_json",
                    format!("sent text that is not JSON ({e})"),
                );
                self.send(conn, &json!({"type": "error", "code": "bad_message", "message": "not JSON"}));
                return;
            }
        };
        if let Err(errors) = schema::check(&msg) {
            let ty = msg_type(&msg);
            self.violation(
                Some(conn),
                sev(Severity::Fail),
                "invalid_message",
                format!("sent an invalid {ty} message: {}", errors.join("; ")),
            );
            self.send(conn, &json!({"type": "error", "code": "bad_message", "message": errors[0]}));
            return;
        }
        let ty = msg["type"].as_str().unwrap_or_default().to_string();

        let has_hello = self.conns.get(&conn).is_some_and(|c| c.info.is_some());
        if !has_hello {
            if ty == "hello" {
                self.on_hello(conn, &msg);
            } else {
                let n = self
                    .conns
                    .get_mut(&conn)
                    .map(|c| {
                        c.pre_hello_msgs += 1;
                        c.pre_hello_msgs
                    })
                    .unwrap_or(0);
                self.violation(
                    Some(conn),
                    sev(Severity::Fail),
                    "before_hello",
                    format!("sent {ty} before hello; hello must be the first message"),
                );
                self.send(conn, &json!({"type": "error", "code": "not_ready"}));
                if n >= 3 {
                    self.close(conn, 4004, "messages before hello");
                }
            }
            return;
        }
        let peer = self.conns[&conn].info.as_ref().map(|i| i.peer_id.clone()).unwrap_or_default();

        match ty.as_str() {
            "hello" => self.violation(
                Some(conn),
                sev(Severity::Warn),
                "second_hello",
                "sent hello twice on one connection".into(),
            ),
            "entity" => self.on_entity(conn, &peer, msg, probe),
            "snapshot_end" => {
                let count = msg["count"].as_u64().unwrap_or(0);
                let c = self.conns.get_mut(&conn).expect("conn exists");
                if c.snapshot_done {
                    self.violation(
                        Some(conn),
                        Severity::Warn,
                        "second_snapshot_end",
                        "sent snapshot_end twice on one connection".into(),
                    );
                    return;
                }
                c.snapshot_done = true;
                let got = c.snapshot_ids.len() as u64;
                if got < count {
                    self.violation(
                        Some(conn),
                        Severity::Fail,
                        "snapshot_count",
                        format!(
                            "snapshot_end says {count} entities, but only {got} distinct entities arrived before it"
                        ),
                    );
                }
                self.notify(Notice::SnapshotEnd { peer_id: peer, count });
            }
            "remove" => {
                let id = msg["id"].as_str().unwrap_or_default().to_string();
                if self.own.contains_key(&id) {
                    self.violation(
                        Some(conn),
                        sev(Severity::Fail),
                        "remove_not_owner",
                        format!("sent remove for {id}, which we own; only the owner may remove an entity"),
                    );
                    return;
                }
                match self.replicas.get(&id) {
                    Some(e) if e.owner == peer => {
                        self.replicas.remove(&id);
                        self.rates.remove(&id);
                        self.last_t.remove(&id);
                        let reason = msg["reason"].as_str().unwrap_or("despawned").to_string();
                        self.notify(Notice::EntityRemoved { id, owner: peer, reason });
                    }
                    Some(e) => {
                        let owner = e.owner.clone();
                        self.violation(
                            Some(conn),
                            sev(Severity::Fail),
                            "remove_not_owner",
                            format!("sent remove for {id}, which belongs to peer {owner}"),
                        );
                    }
                    None => {}
                }
            }
            "event" => self.on_event(conn, &peer, &msg, probe),
            "result" => {
                let event_id = msg["eventId"].as_str().unwrap_or_default().to_string();
                match self.pending_results.remove(&event_id) {
                    Some(p) => {
                        if p.to != peer {
                            self.violation(
                                Some(conn),
                                Severity::Warn,
                                "result_wrong_peer",
                                format!("answered event {event_id}, which was sent to peer {}", p.to),
                            );
                        }
                        self.notify(Notice::EventResult {
                            event_id,
                            ok: msg["ok"].as_bool().unwrap_or(false),
                            reason: msg.get("reason").and_then(Value::as_str).map(Into::into),
                            data: msg.get("data").cloned().unwrap_or_else(|| json!({})),
                        });
                    }
                    None => self.violation(
                        Some(conn),
                        Severity::Warn,
                        "result_unexpected",
                        format!(
                            "sent a result for event {event_id}, which we did not send to it or which already timed out"
                        ),
                    ),
                }
            }
            "transfer_offer" => {
                let id = msg["id"].as_str().unwrap_or_default().to_string();
                if self.replicas.get(&id).is_some_and(|e| e.owner == peer) {
                    self.notify(Notice::TransferOffer { from: peer, id });
                } else {
                    self.violation(
                        Some(conn),
                        Severity::Warn,
                        "offer_not_owner",
                        format!("offered entity {id}, which it does not own"),
                    );
                    self.send(conn, &json!({"type": "transfer_reply", "id": id, "accept": false}));
                }
            }
            "transfer_reply" => {
                let id = msg["id"].as_str().unwrap_or_default().to_string();
                let accept = msg["accept"].as_bool().unwrap_or(false);
                match self.offers_out.get(&id) {
                    Some((to, _)) if *to == peer => {
                        self.offers_out.remove(&id);
                        if accept && self.own.contains_key(&id) {
                            self.broadcast(&json!({"type": "transfer", "id": id, "from": self.me, "to": peer}));
                            let mut e = self.own.remove(&id).expect("checked");
                            e.owner = peer.clone();
                            self.replicas.insert(id.clone(), e);
                            self.last_t.remove(&id);
                            self.notify(Notice::OwnershipChanged {
                                id: id.clone(),
                                from: self.me.clone(),
                                to: peer.clone(),
                            });
                        }
                        self.notify(Notice::TransferAnswered { id, to: peer, accepted: accept, timed_out: false });
                    }
                    _ => self.violation(
                        Some(conn),
                        Severity::Warn,
                        "reply_unexpected",
                        format!("sent transfer_reply for {id}, which we did not offer to it"),
                    ),
                }
            }
            "transfer" => self.on_transfer(conn, &peer, &msg),
            "chat" => self.notify(Notice::Chat {
                from: peer,
                name: msg["name"].as_str().unwrap_or_default().into(),
                text: msg["text"].as_str().unwrap_or_default().chars().take(500).collect(),
            }),
            "peers" => {
                let sender = self.conns[&conn].host.clone();
                if let Some(list) = msg["peers"].as_array() {
                    for p in list.clone() {
                        self.gossip(&p, &sender);
                    }
                }
            }
            "heartbeat" => {}
            "bye" => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.got_bye = true;
                }
            }
            "error" => self.notify(Notice::PeerError {
                peer_id: peer,
                code: msg["code"].as_str().unwrap_or_default().into(),
                message: msg.get("message").and_then(Value::as_str).map(Into::into),
            }),
            "beacon" => self.violation(
                Some(conn),
                Severity::Warn,
                "beacon_on_ws",
                "sent a beacon over WebSocket; beacons go over UDP".into(),
            ),
            _ => {} // unknown types are ignored
        }
    }

    fn gossip(&mut self, p: &Value, sender: &str) {
        let (Some(id), Some(host), Some(port)) = (p["peerId"].as_str(), p["host"].as_str(), p["port"].as_u64()) else {
            return;
        };
        if id == self.me || self.conn_of(id).is_some() {
            return;
        }
        if !self.known.contains_key(id) {
            self.learn_peer(id.to_string(), gossip_host(host, sender).to_string(), port as u16);
        }
    }

    fn on_hello(&mut self, conn: u64, msg: &Value) {
        let peer = msg["peerId"].as_str().unwrap_or_default().to_string();
        let version = msg["version"].as_str().unwrap_or_default().to_string();
        if proto::major_of(&version) != Some(proto::MAJOR) {
            self.send(
                conn,
                &json!({"type": "error", "code": "version_mismatch",
                "message": format!("this peer speaks Jamlink {}", proto::VERSION)}),
            );
            self.violation(
                Some(conn),
                Severity::Fail,
                "version_mismatch",
                format!("peer {peer} speaks Jamlink {version}, which is incompatible with {}", proto::VERSION),
            );
            if let Some(s) = self.conns[&conn].seed {
                self.seeds[s].refused = true;
            }
            self.known.remove(&peer);
            self.refused.insert(peer);
            self.close(conn, 4003, "version mismatch");
            return;
        }
        if peer == self.me {
            if let Some(s) = self.conns[&conn].seed {
                self.seeds[s].peer = Some(peer);
            }
            self.close(conn, 4002, "self");
            return;
        }
        let c = self.conns.get_mut(&conn).expect("conn exists");
        let host = c.host.clone();
        let info = PeerInfo {
            peer_id: peer.clone(),
            name: msg["name"].as_str().unwrap_or_default().chars().take(64).collect(),
            faction: msg["faction"].as_str().unwrap_or_default().into(),
            team: msg.get("team").and_then(Value::as_str).map(Into::into),
            version,
            handles: msg
                .get("handles")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(Into::into).collect())
                .unwrap_or_default(),
            host: host.clone(),
            port: msg["port"].as_u64().unwrap_or(0) as u16,
        };
        let outbound = c.outbound;
        let fallback = c.fallback_dial;
        let seed = c.seed;

        // Duplicate connections: keep the one dialled by the smaller peer ID.
        let existing = self.conn_of(&peer);
        self.conns.get_mut(&conn).expect("conn exists").info = Some(info.clone());
        if let Some(s) = seed {
            self.seeds[s].peer = Some(peer.clone());
        }
        self.learn_peer(peer.clone(), host, info.port);
        if let Some(k) = self.known.get_mut(&peer) {
            k.last_dial = Some(Instant::now());
            k.connected_before = true;
        }
        if let Some(other) = existing {
            let keeper_is_me = self.me < peer;
            let this_by_keeper = outbound == keeper_is_me;
            let other_by_keeper = self.conns[&other].outbound == keeper_is_me;
            let drop = if this_by_keeper && !other_by_keeper {
                other
            } else if other_by_keeper && !this_by_keeper {
                conn
            } else {
                other
            };
            self.close(drop, 4001, "duplicate connection");
            if drop == conn {
                return;
            }
        }

        // Snapshot of everything we own, then tell everyone about our peers.
        let ours: Vec<Value> = self.own.values().map(Entity::to_message).collect();
        let count = ours.len();
        for m in ours {
            self.send(conn, &m);
        }
        self.send(conn, &json!({"type": "snapshot_end", "count": count}));
        let peers = json!({"type": "peers", "peers": self.peer_list()});
        self.broadcast(&peers);

        if let Some(list) = msg["peers"].as_array() {
            for p in list.clone() {
                self.gossip(&p, &info.host);
            }
        }
        if existing.is_none() {
            if fallback {
                self.violation(Some(conn), Severity::Warn, "dial_rule",
                    "did not dial us within 5 s although its peer ID is smaller (dial rule); we dialled it instead. It may not have seen our beacon".into());
            }
            self.notify(Notice::PeerJoined(info));
        }
    }

    fn on_entity(&mut self, conn: u64, peer: &str, msg: Value, probe: bool) {
        let entity: Entity = match serde_json::from_value(msg) {
            Ok(e) => e,
            Err(e) => {
                self.violation(
                    Some(conn),
                    Severity::Fail,
                    "invalid_message",
                    format!("sent an entity we could not read: {e}"),
                );
                return;
            }
        };
        let id = entity.id.clone();
        let snapshot_done = {
            let c = self.conns.get_mut(&conn).expect("conn exists");
            if !c.snapshot_done {
                c.snapshot_ids.insert(id.clone());
            }
            c.snapshot_done
        };
        if self.own.contains_key(&id) {
            self.violation(
                Some(conn),
                if probe { Severity::Info } else { Severity::Fail },
                "update_not_owner",
                format!("sent an update for {id}, which we own; only the owner may update an entity"),
            );
            return;
        }
        let current = self.replicas.get(&id).map(|e| e.owner.clone());
        match &current {
            Some(owner) if owner != peer => {
                self.violation(Some(conn), Severity::Warn, "update_not_owner",
                    format!("sent an update for {id}, which belongs to peer {owner} (ignored; fine only right after a transfer)"));
                return;
            }
            None if entity.owner != peer => {
                self.violation(
                    Some(conn),
                    Severity::Fail,
                    "owner_mismatch",
                    format!(
                        "sent new entity {id} with owner {}, but the sender is the owner of what it sends",
                        entity.owner
                    ),
                );
                return;
            }
            Some(_) if entity.owner != peer => {
                self.violation(
                    Some(conn),
                    Severity::Fail,
                    "owner_mismatch",
                    format!("sent {id} with owner field {}, but it is the owner", entity.owner),
                );
            }
            _ => {}
        }

        // Clock and rate checks.
        let last = self.last_t.entry(id.clone()).or_insert(0);
        if entity.t < *last {
            let back = *last - entity.t;
            self.violation(
                Some(conn),
                Severity::Warn,
                "t_backwards",
                format!("t of {id} went backwards by {back} ms; t must come from one monotonic clock"),
            );
        } else {
            *last = entity.t;
        }
        if snapshot_done {
            let now = Instant::now();
            let q = self.rates.entry(id.clone()).or_default();
            q.push_back(now);
            while q.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1)) {
                q.pop_front();
            }
            let span = q.front().map(|t| now.duration_since(*t).as_secs_f64()).unwrap_or(0.0);
            // Judge the rate over at least half a second of updates.
            if span >= 0.5
                && (q.len() - 1) as f64 / span > MAX_RATE_HZ as f64 + 2.0
                && self.rate_warned.insert(id.clone())
            {
                let hz = ((q.len() - 1) as f64 / span).round();
                self.violation(
                    Some(conn),
                    Severity::Warn,
                    "rate",
                    format!("updates {id} at about {hz} Hz; the maximum is 20 Hz (10 Hz recommended)"),
                );
            }
        }

        let new = current.is_none();
        self.replicas.insert(id, entity.clone());
        self.notify(Notice::EntityUpdated { entity, new });
    }

    fn on_event(&mut self, conn: u64, peer: &str, msg: &Value, probe: bool) {
        let event = IncomingEvent {
            event_id: msg["eventId"].as_str().unwrap_or_default().into(),
            event: msg["event"].as_str().unwrap_or_default().into(),
            target: msg["target"].as_str().unwrap_or_default().into(),
            source: msg.get("source").and_then(Value::as_str).map(Into::into),
            data: msg.get("data").cloned().unwrap_or_else(|| json!({})),
            want_result: msg.get("wantResult").and_then(Value::as_bool).unwrap_or(false),
        };
        if !event.event_id.starts_with(peer) && !probe {
            self.violation(
                Some(conn),
                Severity::Warn,
                "event_id",
                format!("used event ID {}, which does not start with its own peer ID", event.event_id),
            );
        }
        if self.own.contains_key(&event.target) {
            if event.want_result {
                self.pending_incoming
                    .insert((peer.to_string(), event.event_id.clone()), Instant::now() + AUTO_DECLINE_AFTER);
            }
            self.notify(Notice::Event { from: peer.to_string(), event });
        } else if event.want_result {
            let reason = if self.replicas.contains_key(&event.target) { "not_owner" } else { "unknown_target" };
            self.send_result(peer, &event.event_id, false, Some(reason), None);
        }
    }

    fn on_transfer(&mut self, conn: u64, peer: &str, msg: &Value) {
        let id = msg["id"].as_str().unwrap_or_default().to_string();
        let from = msg["from"].as_str().unwrap_or_default().to_string();
        let to = msg["to"].as_str().unwrap_or_default().to_string();
        if from != peer {
            self.violation(
                Some(conn),
                Severity::Fail,
                "transfer_from",
                format!("sent transfer of {id} with from = {from}; only the current owner sends transfer"),
            );
            return;
        }
        if self.own.contains_key(&id) {
            self.violation(
                Some(conn),
                Severity::Fail,
                "transfer_not_owner",
                format!("sent transfer of {id}, which we own"),
            );
            return;
        }
        let Some(e) = self.replicas.get_mut(&id) else { return };
        if e.owner != peer {
            let owner = e.owner.clone();
            self.violation(
                Some(conn),
                Severity::Warn,
                "transfer_not_owner",
                format!("sent transfer of {id}, which belongs to peer {owner}"),
            );
            return;
        }
        if to == self.me {
            let mut e = self.replicas.remove(&id).expect("checked");
            e.owner = self.me.clone();
            e.t = self.now_ms();
            self.own.insert(id.clone(), e);
            self.rebroadcast.push((id.clone(), Instant::now()));
            self.rebroadcast.push((id.clone(), Instant::now() + Duration::from_secs(1)));
        } else {
            e.owner = to.clone();
        }
        // A new owner has its own clock.
        self.last_t.remove(&id);
        self.notify(Notice::OwnershipChanged { id, from, to });
    }

    fn on_closed(&mut self, conn: u64) {
        let Some(c) = self.conns.remove(&conn) else { return };
        let Some(info) = c.info else { return };
        if self.conn_of(&info.peer_id).is_some() {
            return; // a duplicate was closed; the peer is still connected
        }
        let peer = info.peer_id.clone();
        let gone: Vec<String> =
            self.replicas.iter().filter(|(_, e)| e.owner == peer).map(|(id, _)| id.clone()).collect();
        for id in gone {
            self.replicas.remove(&id);
            self.rates.remove(&id);
            self.last_t.remove(&id);
            self.notify(Notice::EntityRemoved { id, owner: peer.clone(), reason: "peer_left".into() });
        }
        let failed: Vec<String> =
            self.pending_results.iter().filter(|(_, p)| p.to == peer).map(|(id, _)| id.clone()).collect();
        for event_id in failed {
            self.pending_results.remove(&event_id);
            self.notify(Notice::EventResult { event_id, ok: false, reason: Some("peer_left".into()), data: json!({}) });
        }
        let offers: Vec<String> =
            self.offers_out.iter().filter(|(_, (to, _))| *to == peer).map(|(id, _)| id.clone()).collect();
        for id in offers {
            self.offers_out.remove(&id);
            self.notify(Notice::TransferAnswered { id, to: peer.clone(), accepted: false, timed_out: true });
        }
        self.pending_incoming.retain(|(from, _), _| *from != peer);
        if c.got_bye {
            self.known.remove(&peer);
        }
        self.notify(Notice::PeerLeft { peer_id: peer, name: info.name, graceful: c.got_bye });
    }

    // --- timers ------------------------------------------------------------

    fn on_tick(&mut self) {
        let now = Instant::now();

        // Liveness and heartbeats.
        let ids: Vec<u64> = self.conns.keys().copied().collect();
        for id in ids {
            let c = &self.conns[&id];
            if c.closing {
                continue;
            }
            if c.info.is_none() {
                if now.duration_since(c.opened) > HELLO_TIMEOUT {
                    self.violation(
                        Some(id),
                        Severity::Fail,
                        "no_hello",
                        format!("connection from {} sent no hello within 10 s", c.host),
                    );
                    self.close(id, 4004, "no hello");
                }
                continue;
            }
            let silent = now.duration_since(c.last_rx);
            if silent > PEER_TIMEOUT {
                self.violation(
                    Some(id),
                    Severity::Fail,
                    "timeout",
                    "sent nothing for 5 s, so it counts as gone; send heartbeat after sending nothing for 1 s, even while receiving".into(),
                );
                self.close(id, 4005, "timeout");
                continue;
            }
            if silent > SILENCE_WARN && !c.silence_warned {
                self.conns.get_mut(&id).expect("exists").silence_warned = true;
                self.violation(
                    Some(id),
                    Severity::Warn,
                    "heartbeat",
                    format!(
                        "sent nothing for {:.1} s; heartbeat is due after sending nothing for 1 s, even while receiving",
                        silent.as_secs_f64()
                    ),
                );
            }
            if now.duration_since(self.conns[&id].last_tx) >= HEARTBEAT_AFTER {
                self.send(id, &json!({"type": "heartbeat"}));
            }
        }

        // Dialling known peers (dial rule with fallback).
        self.known.retain(|_, k| now.duration_since(k.last_seen) < FORGET_UNSEEN_PEER);
        let connected: HashSet<String> =
            self.conns.values().filter_map(|c| c.info.as_ref()).map(|i| i.peer_id.clone()).collect();
        let to_dial: Vec<(String, String, u16)> = self
            .known
            .iter()
            .filter(|(id, _)| !connected.contains(*id) && !self.dialing.contains(&DialKey::Peer((*id).clone())))
            // Seeds re-dial their own peers.
            .filter(|(id, _)| !self.seeds.iter().any(|s| s.peer.as_deref() == Some(id.as_str())))
            .filter(|(id, k)| {
                let due = k.last_dial.is_none_or(|t| now.duration_since(t) > REDIAL_INTERVAL);
                due && (self.me < **id || now.duration_since(k.first_seen) > DIAL_FALLBACK)
            })
            .map(|(id, k)| (id.clone(), k.host.clone(), k.port))
            .collect();
        for (id, host, port) in to_dial {
            self.known.get_mut(&id).expect("exists").last_dial = Some(now);
            self.dial(host, port, DialKey::Peer(id));
        }

        // Seeds are always dialled until connected.
        for i in 0..self.seeds.len() {
            let s = &self.seeds[i];
            let up = s.peer.as_ref().is_some_and(|p| *p == self.me || connected.contains(p));
            let due = s.last_attempt.is_none_or(|t| now.duration_since(t) > REDIAL_INTERVAL);
            if !up && !s.refused && due && !self.dialing.contains(&DialKey::Seed(i)) {
                let (host, port) = (s.host.clone(), s.port);
                self.seeds[i].last_attempt = Some(now);
                self.dial(host, port, DialKey::Seed(i));
            }
        }

        // Timeouts.
        let expired: Vec<String> =
            self.pending_results.iter().filter(|(_, p)| p.deadline <= now).map(|(id, _)| id.clone()).collect();
        for event_id in expired {
            let p = self.pending_results.remove(&event_id).expect("exists");
            if let Some(conn) = self.conn_of(&p.to) {
                self.violation(
                    Some(conn),
                    Severity::Fail,
                    "no_result",
                    format!("did not answer event {event_id} within 5 s although it asked for a result"),
                );
            }
            self.notify(Notice::EventResult { event_id, ok: false, reason: Some("timeout".into()), data: json!({}) });
        }
        let unanswered: Vec<(String, String)> =
            self.pending_incoming.iter().filter(|(_, d)| **d <= now).map(|(k, _)| k.clone()).collect();
        for (from, event_id) in unanswered {
            self.pending_incoming.remove(&(from.clone(), event_id.clone()));
            self.violation(
                None,
                Severity::Warn,
                "self_no_reply",
                format!("this game did not answer event {event_id} within 4 s; declined it automatically"),
            );
            self.send_result(&from, &event_id, false, Some("declined"), None);
        }
        let offers: Vec<String> =
            self.offers_out.iter().filter(|(_, (_, d))| *d <= now).map(|(id, _)| id.clone()).collect();
        for id in offers {
            let (to, _) = self.offers_out.remove(&id).expect("exists");
            self.notify(Notice::TransferAnswered { id, to, accepted: false, timed_out: true });
        }

        // Re-broadcasts after gaining ownership.
        let due: Vec<String> = self.rebroadcast.iter().filter(|(_, at)| *at <= now).map(|(id, _)| id.clone()).collect();
        self.rebroadcast.retain(|(_, at)| *at > now);
        for id in due {
            if let Some(e) = self.own.get_mut(&id) {
                e.t = self.start.elapsed().as_millis() as u64;
                let m = e.to_message();
                self.broadcast(&m);
            }
        }
    }

    fn dial(&mut self, host: String, port: u16, key: DialKey) {
        self.dialing.insert(key.clone());
        let core = self.in_tx.clone();
        let ids = self.conn_ids.clone();
        tokio::spawn(async move {
            match dial_ws(&host, port).await {
                Ok(ws) => {
                    let conn = ids.fetch_add(1, Ordering::Relaxed);
                    let (tx, rx) = unbounded_channel();
                    let _ = core.send(In::Opened { conn, tx, outbound: true, host, key: Some(key) });
                    run_conn(ws, conn, rx, core).await;
                }
                Err(_) => {
                    let _ = core.send(In::DialFailed { key });
                }
            }
        });
    }
}

fn msg_type(msg: &Value) -> String {
    msg.get("type").and_then(Value::as_str).unwrap_or("untyped").to_string()
}

/// The host to dial for a gossiped peer. A loopback host means "on the
/// sender's machine", so it is reached through the sender's address.
fn gossip_host<'a>(host: &'a str, sender: &'a str) -> &'a str {
    let loopback =
        host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if loopback { sender } else { host }
}

// ---------------------------------------------------------------------------
// Socket tasks
// ---------------------------------------------------------------------------

async fn dial_ws(
    host: &str,
    port: u16,
) -> Result<WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, String> {
    let url = format!("ws://{host}:{port}{}", proto::WS_PATH);
    let connect = tokio_tungstenite::connect_async_with_config(url.as_str(), None, true);
    match tokio::time::timeout(Duration::from_secs(3), connect).await {
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("timeout".into()),
    }
}

async fn accept_loop(listener: TcpListener, core: UnboundedSender<In>, ids: Arc<AtomicU64>) {
    loop {
        let Ok((stream, addr)) = listener.accept().await else { continue };
        let core = core.clone();
        let ids = ids.clone();
        let _ = stream.set_nodelay(true);
        tokio::spawn(async move {
            #[allow(clippy::result_large_err)]
            let callback = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                if req.uri().path() != proto::WS_PATH {
                    return Err(http::Response::builder()
                        .status(http::StatusCode::NOT_FOUND)
                        .body(Some(format!("Jamlink lives at {}", proto::WS_PATH)))
                        .expect("valid response"));
                }
                Ok(resp)
            };
            let Ok(Ok(ws)) =
                tokio::time::timeout(Duration::from_secs(5), tokio_tungstenite::accept_hdr_async(stream, callback))
                    .await
            else {
                return;
            };
            let conn = ids.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = unbounded_channel();
            let _ = core.send(In::Opened { conn, tx, outbound: false, host: addr.ip().to_string(), key: None });
            run_conn(ws, conn, rx, core).await;
        });
    }
}

async fn run_conn<S>(ws: WebSocketStream<S>, conn: u64, mut out: UnboundedReceiver<Out>, core: UnboundedSender<In>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut sink, mut stream) = ws.split();
    loop {
        tokio::select! {
            m = stream.next() => match m {
                Some(Ok(Message::Text(t))) => { let _ = core.send(In::Text { conn, text: t.to_string() }); }
                Some(Ok(Message::Binary(_))) => { let _ = core.send(In::Binary { conn }); }
                Some(Ok(Message::Close(_))) => {
                    // Flush tungstenite's automatic close reply.
                    let _ = tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
                    break;
                }
                Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            o = out.recv() => match o {
                Some(Out::Text(s)) => {
                    if sink.send(Message::text(s)).await.is_err() { break; }
                }
                Some(Out::Close(code, reason)) => {
                    let frame = CloseFrame { code: CloseCode::from(code), reason: reason.into() };
                    let _ = sink.send(Message::Close(Some(frame))).await;
                    // Wait for the peer's close reply before dropping the socket.
                    let _ = tokio::time::timeout(Duration::from_secs(1), async {
                        while let Some(Ok(m)) = stream.next().await {
                            if m.is_close() {
                                break;
                            }
                        }
                    })
                    .await;
                    break;
                }
                None => break,
            },
        }
    }
    let _ = core.send(In::Closed { conn });
}

fn bind_beacon_socket() -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(unix)]
    s.set_reuse_port(true)?;
    s.set_broadcast(true)?;
    s.set_nonblocking(true)?;
    s.bind(&SocketAddr::from(([0, 0, 0, 0], proto::BEACON_PORT)).into())?;
    UdpSocket::from_std(s.into())
}

async fn beacon_loop(sock: UdpSocket, beacon: String, core: UnboundedSender<In>) {
    let target = SocketAddr::from(([255, 255, 255, 255], proto::BEACON_PORT));
    let mut every = tokio::time::interval(Duration::from_secs(1));
    let mut buf = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = every.tick() => { let _ = sock.send_to(beacon.as_bytes(), target).await; }
            r = sock.recv_from(&mut buf) => {
                if let Ok((n, from)) = r
                    && let Ok(text) = std::str::from_utf8(&buf[..n])
                    && core.send(In::Beacon { text: text.to_string(), from }).is_err()
                {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::gossip_host;

    #[test]
    fn gossip_host_replaces_loopback_with_sender() {
        for host in ["127.0.0.1", "127.1.2.3", "::1", "localhost", "LOCALHOST"] {
            assert_eq!(gossip_host(host, "192.168.1.5"), "192.168.1.5", "{host}");
        }
        assert_eq!(gossip_host("192.168.1.23", "192.168.1.5"), "192.168.1.23");
        assert_eq!(gossip_host("game-pc.local", "192.168.1.5"), "game-pc.local");
    }
}
