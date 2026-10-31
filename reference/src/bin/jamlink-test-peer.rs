//! Jamlink conformance test peer.
//!
//! Joins the LAN like any game, spawns a set of test entities, answers events
//! the way a well-behaved game should, probes every other game it meets, and
//! reports every rule break it sees in plain language.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use jamlink::{Config, DisplayHints, Entity, Handle, IncomingEvent, Notice, PeerInfo, Severity, start};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(
    version,
    about = "Jamlink conformance test peer: joins the LAN like a game, spawns test entities, \
probes every other game it meets and reports rule breaks in plain language."
)]
struct Args {
    /// Name shown to other games.
    #[arg(long, default_value = "Jamlink test peer")]
    name: String,
    /// Peer to dial directly, as host:port. Repeat for several.
    #[arg(long = "seed", value_name = "HOST:PORT")]
    seeds: Vec<String>,
    /// Do not send or listen for UDP beacons; use seeds only.
    #[arg(long)]
    no_discovery: bool,
    /// First WebSocket port to try (the next 8 are tried too).
    #[arg(long, default_value_t = 45801)]
    port: u16,
    /// Stop after this many seconds and print the summary. Default: run until Ctrl-C.
    #[arg(long, value_name = "SECONDS")]
    duration: Option<u64>,
    /// Write every sent and received message to this file as JSON lines.
    #[arg(long, value_name = "FILE")]
    log: Option<PathBuf>,
    /// Skip probes that change other games' state (damage, take).
    #[arg(long)]
    gentle: bool,
    /// Only watch: send no probes to other games.
    #[arg(long)]
    no_probes: bool,
    /// Also print informational notes about other test peers' deliberate probes.
    #[arg(long)]
    verbose: bool,
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    Info,
    Pass,
    Warn,
    Fail,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Info => "INFO",
            Outcome::Pass => "PASS",
            Outcome::Warn => "WARN",
            Outcome::Fail => "FAIL",
        }
    }
    fn from_severity(s: Severity) -> Self {
        match s {
            Severity::Info => Outcome::Info,
            Severity::Warn => Outcome::Warn,
            Severity::Fail => Outcome::Fail,
        }
    }
}

struct Entry {
    outcome: Outcome,
    text: String,
    count: u32,
}

#[derive(Default)]
struct Subject {
    name: String,
    entries: Vec<(String, Entry)>,
}

struct Report {
    start: Instant,
    subjects: Vec<(String, Subject)>,
}

const SELF: &str = "self";

impl Report {
    fn new() -> Self {
        Report { start: Instant::now(), subjects: Vec::new() }
    }

    fn subject(&mut self, key: &str, name: &str) -> &mut Subject {
        if let Some(i) = self.subjects.iter().position(|(k, _)| k == key) {
            let s = &mut self.subjects[i].1;
            if !name.is_empty() {
                s.name = name.to_string();
            }
            return s;
        }
        self.subjects.push((key.to_string(), Subject { name: name.to_string(), entries: Vec::new() }));
        &mut self.subjects.last_mut().expect("just pushed").1
    }

    /// Records an outcome. Repeats of the same `code` for the same subject are counted, not printed.
    fn record(&mut self, key: &str, name: &str, outcome: Outcome, code: &str, text: String) {
        let elapsed = self.start.elapsed().as_secs_f64();
        let s = self.subject(key, name);
        let who = who(key, &s.name);
        if let Some((_, e)) = s.entries.iter_mut().find(|(c, _)| c == code) {
            e.count += 1;
            if outcome > e.outcome {
                e.outcome = outcome;
                e.text = text.clone();
                println!("[{elapsed:7.1}s] {}  {who}: {text}", outcome.label());
            }
            return;
        }
        println!("[{elapsed:7.1}s] {}  {who}: {text}", outcome.label());
        s.entries.push((code.to_string(), Entry { outcome, text, count: 1 }));
    }

    /// Prints a line without recording it.
    fn note(&self, text: &str) {
        println!("[{:7.1}s] INFO  {text}", self.start.elapsed().as_secs_f64());
    }

    fn summary(&self) -> bool {
        println!("\n==================== Jamlink test summary ====================");
        let mut any_fail = false;
        if self.subjects.is_empty() {
            println!("No other games were seen. Check that they are on the same network,\nor pass --seed host:port.");
        }
        for (key, s) in &self.subjects {
            let count = |o: Outcome| s.entries.iter().filter(|(_, e)| e.outcome == o).count();
            let (fails, warns, passes) = (count(Outcome::Fail), count(Outcome::Warn), count(Outcome::Pass));
            any_fail |= fails > 0;
            let verdict = if fails > 0 {
                "FAIL"
            } else if warns > 0 {
                "OK with warnings"
            } else {
                "OK"
            };
            println!("\n{}: {verdict}  ({passes} passed, {warns} warnings, {fails} failures)", who(key, &s.name));
            let mut entries: Vec<&(String, Entry)> =
                s.entries.iter().filter(|(_, e)| e.outcome >= Outcome::Warn).collect();
            entries.sort_by_key(|e| std::cmp::Reverse(e.1.outcome));
            for (_, e) in entries {
                let times = if e.count > 1 { format!(" (x{})", e.count) } else { String::new() };
                println!("  {}  {}{times}", e.outcome.label(), e.text);
            }
        }
        println!("\nExit code: {}", if any_fail { "1 (failures found)" } else { "0" });
        !any_fail
    }
}

fn who(key: &str, name: &str) -> String {
    if key == SELF {
        "this test peer".to_string()
    } else {
        let name = if name.is_empty() { "unnamed peer" } else { name };
        if key.len() == 16 && key.bytes().all(|b| b.is_ascii_hexdigit()) {
            format!("{name} ({})", &key[..8])
        } else {
            name.to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Test world
// ---------------------------------------------------------------------------

const FACTION: &str = "jamlink_test";
const TICK: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Dummy,
    Walker,
    Wood,
    Tree,
    Chest,
    Unit,
    Mystery,
    Gift,
    Adopted,
}

struct Own {
    role: Role,
    entity: Entity,
    dirty: bool,
}

enum Respawn {
    Dummy,
    Wood,
}

struct World {
    node: Handle,
    name: String,
    own: HashMap<String, Own>,
    others: HashMap<String, Entity>,
    respawns: Vec<(Instant, Respawn)>,
    dummy_angle: f64,
    dummy_center: [f64; 3],
    unit_target: Option<[f64; 3]>,
    unit_follow: Option<String>,
    chest_contents: serde_json::Map<String, Value>,
}

fn yaw_towards(d: [f64; 3]) -> f64 {
    // 0 faces north (-Z); positive turns counter-clockwise seen from above.
    (-d[0]).atan2(-d[2])
}

fn display(color: &str, shape: &str, label: Option<&str>) -> Option<DisplayHints> {
    Some(DisplayHints { color: Some(color.into()), shape: Some(shape.into()), label: label.map(Into::into) })
}

impl World {
    fn new(node: Handle, name: String) -> Self {
        let mut w = World {
            node,
            name,
            own: HashMap::new(),
            others: HashMap::new(),
            respawns: Vec::new(),
            dummy_angle: 0.0,
            dummy_center: [8.0, 0.0, 0.0],
            unit_target: None,
            unit_follow: None,
            chest_contents: serde_json::Map::new(),
        };
        w.spawn_dummy();
        w.spawn_wood();

        let mut walker = w.base("npc", [-2.0, 0.0, 5.0], "Walking north");
        walker.vel = Some([0.0, 0.0, -1.5]);
        walker.yaw = Some(0.0);
        walker.height = Some(1.8);
        walker.radius = Some(0.4);
        walker.state = Some("moving".into());
        walker.display = display("#2e86de", "capsule", Some("Walking north"));
        walker.ext = Some(
            json!({"jamlinkTest": "Should move north (towards -Z) at 1.5 m/s, then jump back south every ~7 s"})
                .as_object()
                .cloned()
                .unwrap(),
        );
        w.add(Role::Walker, walker);

        let mut tree = w.base("resource", [-4.0, 0.0, 2.0], "Test tree");
        tree.subkind = Some("wood".into());
        tree.qty = Some(20);
        tree.radius = Some(0.6);
        tree.height = Some(4.0);
        tree.tags = vec!["solid".into(), "pickup".into()];
        tree.display = display("#27ae60", "cone", None);
        w.add(Role::Tree, tree);

        let mut chest = w.base("structure", [0.0, 0.0, -6.0], "Test chest");
        chest.subkind = Some("chest".into());
        chest.radius = Some(0.5);
        chest.height = Some(0.8);
        chest.tags = vec!["solid".into(), "interactable".into()];
        chest.display = display("#a0522d", "box", None);
        w.add(Role::Chest, chest);

        let mut unit = w.base("npc", [4.0, 0.0, -3.0], "Test unit");
        unit.radius = Some(0.4);
        unit.height = Some(1.6);
        unit.hp = Some(50.0);
        unit.hp_max = Some(50.0);
        unit.tags = vec!["commandable".into()];
        unit.display = display("#8e44ad", "capsule", None);
        w.add(Role::Unit, unit);

        let mut mystery = w.base("jamlink_test_mystery", [-6.0, 0.0, -3.0], "Unknown kind");
        mystery.radius = Some(0.5);
        mystery.height = Some(1.5);
        mystery.display = display("#f1c40f", "cone", Some("Unknown kind"));
        w.add(Role::Mystery, mystery);
        w
    }

    fn base(&self, kind: &str, pos: [f64; 3], name: &str) -> Entity {
        let mut e = Entity::new(self.node.new_entity_id(), kind, pos);
        e.name = Some(name.into());
        e.faction = Some(FACTION.into());
        e
    }

    fn add(&mut self, role: Role, entity: Entity) {
        self.own.insert(entity.id.clone(), Own { role, entity, dirty: true });
    }

    fn spawn_dummy(&mut self) {
        let mut d = self.base("creature", [0.0; 3], "Test dummy");
        d.subkind = Some("training_dummy".into());
        d.hp = Some(100.0);
        d.hp_max = Some(100.0);
        d.radius = Some(0.5);
        d.height = Some(1.8);
        d.tags = vec!["solid".into()];
        d.state = Some("moving".into());
        d.display = display("#c0392b", "capsule", None);
        self.add(Role::Dummy, d);
    }

    fn spawn_wood(&mut self) {
        let mut w = self.base("item", [2.0, 0.0, 2.0], "Test wood");
        w.subkind = Some("wood".into());
        w.qty = Some(5);
        w.radius = Some(0.3);
        w.height = Some(0.3);
        w.tags = vec!["pickup".into()];
        w.display = display("#d4a373", "box", None);
        self.add(Role::Wood, w);
    }

    fn id_of(&self, role: Role) -> Option<String> {
        self.own.iter().find(|(_, o)| o.role == role).map(|(id, _)| id.clone())
    }

    fn spawn_gift(&mut self, for_name: &str) -> String {
        let mut g = self.base("item", [1.0, 0.0, 1.0], &format!("Gift for {for_name}"));
        g.name = Some(g.name.unwrap().chars().take(64).collect());
        g.subkind = Some("coin".into());
        g.qty = Some(1);
        g.radius = Some(0.2);
        g.height = Some(0.2);
        g.tags = vec!["pickup".into()];
        g.display = display("#f39c12", "sphere", None);
        let id = g.id.clone();
        self.add(Role::Gift, g);
        id
    }

    /// Advances the simulation one tick and sends what changed.
    fn tick(&mut self, dt: f64) {
        let now = Instant::now();
        let due: Vec<Respawn> = {
            let (due, keep): (Vec<_>, Vec<_>) = self.respawns.drain(..).partition(|(at, _)| *at <= now);
            self.respawns = keep;
            due.into_iter().map(|(_, r)| r).collect()
        };
        for r in due {
            match r {
                Respawn::Dummy => self.spawn_dummy(),
                Respawn::Wood => self.spawn_wood(),
            }
        }

        self.dummy_angle += 0.3 * dt;
        let (center, a, r) = (self.dummy_center, self.dummy_angle, 4.0);
        let follow_pos = self
            .unit_follow
            .as_ref()
            .and_then(|id| self.others.get(id).or_else(|| self.own.get(id).map(|o| &o.entity)))
            .map(|e| e.pos);
        let target = follow_pos.or(self.unit_target);

        for o in self.own.values_mut() {
            let e = &mut o.entity;
            match o.role {
                Role::Dummy => {
                    e.pos = [center[0] + r * a.cos(), 0.0, center[2] + r * a.sin()];
                    let v = [-r * 0.3 * a.sin(), 0.0, r * 0.3 * a.cos()];
                    e.vel = Some(v);
                    e.yaw = Some(yaw_towards(v));
                    o.dirty = true;
                }
                Role::Walker => {
                    e.pos[2] -= 1.5 * dt;
                    if e.pos[2] < -5.0 {
                        e.pos[2] = 5.0; // a 10 m jump: receivers should snap, not slide
                    }
                    o.dirty = true;
                }
                Role::Unit => {
                    if let Some(t) = target {
                        let d = [t[0] - e.pos[0], 0.0, t[2] - e.pos[2]];
                        let dist = (d[0] * d[0] + d[2] * d[2]).sqrt();
                        let stop_at = if follow_pos.is_some() { 1.5 } else { 0.2 };
                        if dist > stop_at {
                            let step = (3.0 * dt).min(dist - stop_at);
                            e.pos[0] += d[0] / dist * step;
                            e.pos[2] += d[2] / dist * step;
                            e.vel = Some([d[0] / dist * 3.0, 0.0, d[2] / dist * 3.0]);
                            e.yaw = Some(yaw_towards(d));
                            e.state = Some("moving".into());
                            o.dirty = true;
                        } else if e.state.as_deref() == Some("moving") {
                            e.vel = None;
                            e.state = None;
                            o.dirty = true;
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(t) = self.unit_target
            && follow_pos.is_none()
            && let Some(id) = self.id_of(Role::Unit)
            && let Some(u) = self.own.get(&id)
        {
            let d = ((t[0] - u.entity.pos[0]).powi(2) + (t[2] - u.entity.pos[2]).powi(2)).sqrt();
            if d <= 0.2 {
                self.unit_target = None;
            }
        }
        for o in self.own.values_mut() {
            if o.dirty {
                o.dirty = false;
                self.node.upsert(o.entity.clone());
            }
        }
    }

    /// Answers an event for one of our entities. Returns a description for the report.
    fn on_event(&mut self, from: &str, ev: &IncomingEvent) -> String {
        let Some(o) = self.own.get_mut(&ev.target) else {
            self.reply(from, ev, false, Some("not_owner"), None);
            return format!("{} for an entity we no longer own", ev.event);
        };
        let target_name = o.entity.name.clone().unwrap_or_else(|| ev.target.clone());
        let role = o.role;
        let num = |k: &str| ev.data.get(k).and_then(Value::as_f64);
        match ev.event.as_str() {
            "damage" | "heal" => {
                let Some(hp) = o.entity.hp else {
                    self.reply(from, ev, false, Some("declined"), None);
                    return format!("{} on {target_name}, which has no hp (declined)", ev.event);
                };
                let amount = num("amount").unwrap_or(0.0);
                let max = o.entity.hp_max.unwrap_or(hp);
                let new = if ev.event == "damage" { hp - amount } else { (hp + amount).min(max) };
                o.entity.hp = Some(new.max(0.0));
                o.dirty = true;
                let text = format!("{} {amount} on {target_name}: hp {hp} -> {}", ev.event, new.max(0.0));
                if new <= 0.0 {
                    self.own.remove(&ev.target);
                    self.node.remove(&ev.target, "destroyed");
                    if role == Role::Dummy {
                        self.respawns.push((Instant::now() + Duration::from_secs(3), Respawn::Dummy));
                    }
                }
                self.reply(from, ev, true, None, None);
                text
            }
            "push" => {
                let imp = ev
                    .data
                    .get("impulse")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>())
                    .unwrap_or_default();
                if imp.len() == 3 {
                    if role == Role::Dummy {
                        self.dummy_center[0] += imp[0] * 0.3;
                        self.dummy_center[2] += imp[2] * 0.3;
                    } else {
                        o.entity.pos[0] += imp[0] * 0.3;
                        o.entity.pos[2] += imp[2] * 0.3;
                        o.dirty = true;
                    }
                }
                self.reply(from, ev, true, None, None);
                format!("push on {target_name}")
            }
            "status" => {
                self.reply(from, ev, true, None, None);
                format!("status {} on {target_name}", ev.data.get("effect").and_then(Value::as_str).unwrap_or("?"))
            }
            "interact" => {
                let text = format!("Hello from {}! You used {target_name}.", self.name);
                self.reply(from, ev, true, None, Some(json!({"text": text})));
                format!("interact on {target_name}")
            }
            "take" if o.entity.has_tag("pickup") => {
                let have = o.entity.qty_or_default();
                let default = if o.entity.kind == "resource" { 3 } else { have };
                let want = ev.data.get("qty").and_then(Value::as_u64).unwrap_or(default);
                let qty = want.min(have);
                if qty == 0 {
                    self.reply(from, ev, false, Some("declined"), None);
                    return format!("take on {target_name}: nothing left (declined)");
                }
                let subkind = o.entity.subkind.clone().unwrap_or_else(|| "item".into());
                let left = have - qty;
                if left == 0 && o.entity.kind != "resource" {
                    self.own.remove(&ev.target);
                    self.node.remove(&ev.target, "taken");
                    if role == Role::Wood {
                        self.respawns.push((Instant::now() + Duration::from_secs(5), Respawn::Wood));
                    }
                } else {
                    o.entity.qty = Some(left);
                    o.dirty = true;
                }
                self.reply(from, ev, true, None, Some(json!({"subkind": subkind, "qty": qty})));
                format!("take {qty} {subkind} from {target_name}")
            }
            "give" if role == Role::Chest => {
                let sub = ev.data.get("subkind").and_then(Value::as_str).unwrap_or("item").to_string();
                let qty = ev.data.get("qty").and_then(Value::as_u64).unwrap_or(1);
                let entry = self.chest_contents.entry(sub.clone()).or_insert(json!(0));
                *entry = json!(entry.as_u64().unwrap_or(0) + qty);
                o.entity.ext = Some(serde_json::Map::from_iter([(
                    "contents".to_string(),
                    Value::Object(self.chest_contents.clone()),
                )]));
                o.dirty = true;
                self.reply(from, ev, true, None, None);
                format!("give {qty} {sub} to {target_name}")
            }
            "command" if role == Role::Unit => {
                let order = ev.data.get("order").and_then(Value::as_str).unwrap_or("");
                let pos = ev
                    .data
                    .get("pos")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>());
                let target_id = ev.data.get("targetId").and_then(Value::as_str).map(String::from);
                match order {
                    "move" | "attack" => {
                        self.unit_follow = None;
                        let p = pos
                            .filter(|p| p.len() == 3)
                            .map(|p| [p[0], p[1], p[2]])
                            .or_else(|| target_id.and_then(|t| self.others.get(&t).map(|e| e.pos)));
                        self.unit_target = p;
                    }
                    "follow" => {
                        self.unit_follow = target_id;
                        self.unit_target = None;
                    }
                    _ => {
                        self.unit_follow = None;
                        self.unit_target = None;
                    }
                }
                self.reply(from, ev, true, None, None);
                format!("command {order} on {target_name}")
            }
            "take" | "give" | "command" => {
                self.reply(from, ev, false, Some("declined"), None);
                format!("{} on {target_name}, which does not accept it (declined)", ev.event)
            }
            other => {
                self.reply(from, ev, false, Some("unsupported"), None);
                format!("unknown event {other} on {target_name} (answered unsupported)")
            }
        }
    }

    fn reply(&self, from: &str, ev: &IncomingEvent, ok: bool, reason: Option<&str>, data: Option<Value>) {
        if ev.want_result {
            self.node.reply(from, &ev.event_id, ok, reason, data);
        }
    }
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

enum Probe {
    UnknownTarget,
    UnknownEvent,
    Take(String),
    Interact(String),
}

#[derive(Default)]
struct PeerState {
    name: String,
    joined: Option<Instant>,
    snapshot_at: Option<Instant>,
    snapshot_checked: bool,
    step: u8,
    connected: bool,
    malformed_at: Option<Instant>,
    malformed_reported: bool,
    damage: Option<(String, f64, Instant)>,
    gift: Option<(String, Option<Instant>)>,
    finished: bool,
}

struct Tester {
    report: Report,
    world: World,
    peers: HashMap<String, PeerState>,
    probes: HashMap<String, (String, Probe)>,
    args: Args,
}

impl Tester {
    fn peer_name(&self, peer: &str) -> String {
        self.peers.get(peer).map(|p| p.name.clone()).unwrap_or_default()
    }

    fn rec(&mut self, peer: &str, outcome: Outcome, code: &str, text: String) {
        let name = self.peer_name(peer);
        self.report.record(peer, &name, outcome, code, text);
    }

    fn owned_by<'a>(&'a self, peer: &'a str) -> impl Iterator<Item = &'a Entity> + 'a {
        self.world.others.values().filter(move |e| e.owner == peer)
    }

    fn on_notice(&mut self, n: Notice) {
        match n {
            Notice::PeerJoined(info) => self.on_joined(info),
            Notice::PeerLeft { peer_id, name, graceful } => {
                let st = self.peers.entry(peer_id.clone()).or_default();
                st.connected = false;
                // A peer that said bye left on purpose; only a lost connection can be a crash.
                let malformed_recent = !graceful
                    && st.malformed_at.is_some_and(|t| t.elapsed() < Duration::from_secs(5))
                    && !st.malformed_reported;
                if malformed_recent {
                    st.malformed_reported = true;
                    self.rec(&peer_id, Outcome::Fail, "malformed", "disconnected right after we sent it a malformed message; invalid messages must be dropped, not fatal".into());
                }
                let how =
                    if graceful { "left gracefully (bye)" } else { "left without bye (connection lost or timed out)" };
                self.report.record(&peer_id, &name, Outcome::Info, "left", how.into());
            }
            Notice::SnapshotEnd { peer_id, count } => {
                let st = self.peers.entry(peer_id.clone()).or_default();
                st.snapshot_at = Some(Instant::now());
                self.rec(
                    &peer_id,
                    Outcome::Pass,
                    "snapshot",
                    format!("finished its snapshot (snapshot_end, count {count})"),
                );
            }
            Notice::EntityUpdated { entity, new } => {
                let owner = entity.owner.clone();
                if new {
                    let kind = entity.kind.clone();
                    let name = entity.name.clone().unwrap_or_default();
                    if self.args.verbose {
                        self.report.note(&format!("{} owns {kind} \"{name}\" ({})", self.peer_name(&owner), entity.id));
                    }
                }
                // Gift accepted earlier: the new owner must broadcast it as its own.
                if let Some(st) = self.peers.get_mut(&owner)
                    && let Some((gift, Some(_))) = &st.gift
                    && *gift == entity.id
                {
                    st.gift = None;
                    self.rec(
                        &owner,
                        Outcome::Pass,
                        "transfer",
                        "accepted a transfer and broadcast the entity as its own".into(),
                    );
                }
                // Damage probe: hp must drop.
                if let Some(st) = self.peers.get_mut(&owner)
                    && let Some((id, before, _)) = &st.damage
                    && *id == entity.id
                    && entity.hp.is_some_and(|hp| hp < *before)
                {
                    st.damage = None;
                    self.rec(&owner, Outcome::Pass, "damage", "applied our damage event".into());
                }
                self.world.others.insert(entity.id.clone(), entity);
            }
            Notice::EntityRemoved { id, owner, reason } => {
                self.world.others.remove(&id);
                if let Some(st) = self.peers.get_mut(&owner)
                    && st.damage.as_ref().is_some_and(|(d, _, _)| *d == id)
                    && reason == "destroyed"
                {
                    st.damage = None;
                    self.rec(
                        &owner,
                        Outcome::Pass,
                        "damage",
                        "applied our damage event (the target was destroyed)".into(),
                    );
                }
            }
            Notice::Event { from, event } => {
                let text = self.world.on_event(&from, &event);
                let code = format!("recv_{}", event.event);
                let name = self.peer_name(&from);
                self.report.record(&from, &name, Outcome::Info, &code, format!("sent us {text}"));
            }
            Notice::EventResult { event_id, ok, reason, data } => self.on_result(&event_id, ok, reason, data),
            Notice::Chat { from, name, text } => {
                let who = self.peer_name(&from);
                self.report.note(&format!("chat from {who}: <{name}> {text}"));
            }
            Notice::TransferOffer { from, id } => {
                // Accept everything, to exercise the other side's transfer code.
                self.world.node.reply_transfer(&from, &id, true);
                let name = self.peer_name(&from);
                self.report.record(&from, &name, Outcome::Info, "offer_in", format!("offered us {id}; accepted"));
            }
            Notice::TransferAnswered { id, to, accepted, timed_out } => {
                let Some(st) = self.peers.get_mut(&to) else { return };
                if accepted {
                    st.gift = Some((id.clone(), Some(Instant::now())));
                    self.world.own.remove(&id);
                } else {
                    st.gift = None;
                    self.world.own.remove(&id);
                    self.world.node.remove(&id, "despawned");
                    let text = if timed_out {
                        "did not answer a transfer offer (transfers are optional in 1.0, so this is fine if they are not implemented)"
                    } else {
                        "declined a transfer offer (allowed)"
                    };
                    self.rec(&to, Outcome::Info, "transfer", text.into());
                }
            }
            Notice::OwnershipChanged { id, to, .. } => {
                if to == self.world.node.peer_id()
                    && let Some(e) = self.world.others.remove(&id)
                {
                    let mut e = e;
                    e.owner = to;
                    self.world.add(Role::Adopted, e);
                }
            }
            Notice::PeerError { peer_id, code, message } => {
                let m = message.map(|m| format!(": {m}")).unwrap_or_default();
                let text = format!("reported an error about a message we sent: {code}{m}");
                self.rec(&peer_id, Outcome::Info, &format!("error_{code}"), text);
            }
            Notice::Violation(v) => {
                if v.code == "no_result" {
                    return; // the probes report these themselves
                }
                if v.severity == Severity::Info && !self.args.verbose {
                    return;
                }
                let outcome = Outcome::from_severity(v.severity);
                match v.peer_id {
                    Some(p) => {
                        let name = v.peer_name.unwrap_or_default();
                        self.report.record(&p, &name, outcome, v.code, v.text);
                    }
                    None => {
                        let key = v.peer_name.as_deref().map(|n| n.to_string()).unwrap_or_else(|| SELF.to_string());
                        let name = if key == SELF { String::new() } else { key.clone() };
                        self.report.record(&key, &name, outcome, v.code, v.text);
                    }
                }
            }
        }
    }

    fn on_joined(&mut self, info: PeerInfo) {
        let st = self.peers.entry(info.peer_id.clone()).or_default();
        *st =
            PeerState { name: info.name.clone(), joined: Some(Instant::now()), connected: true, ..Default::default() };
        let handles = if info.handles.is_empty() { "none listed".to_string() } else { info.handles.join(", ") };
        self.report.record(
            &info.peer_id,
            &info.name,
            Outcome::Pass,
            "hello",
            format!(
                "connected: Jamlink {}, faction {}, {}:{}, handles: {handles}",
                info.version, info.faction, info.host, info.port
            ),
        );
    }

    fn on_result(&mut self, event_id: &str, ok: bool, reason: Option<String>, data: Value) {
        let Some((peer, probe)) = self.probes.remove(event_id) else { return };
        let r = reason.clone().unwrap_or_default();
        let timed_out = r == "timeout";
        if r == "peer_left" {
            return;
        }
        match probe {
            Probe::UnknownTarget => {
                if timed_out {
                    self.rec(&peer, Outcome::Fail, "unknown_target", "did not answer an event (with wantResult) for an entity it does not own; it must reply not_owner or unknown_target".into());
                } else if !ok && (r == "not_owner" || r == "unknown_target") {
                    self.rec(
                        &peer,
                        Outcome::Pass,
                        "unknown_target",
                        format!("answered an event for an entity it does not own with {r}"),
                    );
                } else {
                    self.rec(&peer, Outcome::Warn, "unknown_target", format!("answered an event for an entity it does not own with ok={ok}, reason={r:?}; expected not_owner or unknown_target"));
                }
            }
            Probe::UnknownEvent => {
                if timed_out {
                    self.rec(
                        &peer,
                        Outcome::Fail,
                        "unknown_event",
                        "did not answer an unknown event that asked for a result; reply unsupported".into(),
                    );
                } else if !ok && r == "unsupported" {
                    self.rec(
                        &peer,
                        Outcome::Pass,
                        "unknown_event",
                        "answered an unknown event with unsupported".into(),
                    );
                } else {
                    self.rec(
                        &peer,
                        Outcome::Warn,
                        "unknown_event",
                        format!("answered an unknown event with ok={ok}, reason={r:?}; expected unsupported"),
                    );
                }
            }
            Probe::Take(name) => {
                if timed_out {
                    self.rec(
                        &peer,
                        Outcome::Fail,
                        "take",
                        format!("did not answer take on \"{name}\" within 5 s; take always needs a result"),
                    );
                } else if ok {
                    let good = data.get("subkind").and_then(Value::as_str).is_some()
                        && data.get("qty").and_then(Value::as_u64).is_some();
                    if good {
                        self.rec(
                            &peer,
                            Outcome::Pass,
                            "take",
                            format!("answered take on \"{name}\" with what was taken ({data})"),
                        );
                    } else {
                        self.rec(
                            &peer,
                            Outcome::Warn,
                            "take",
                            format!("answered take on \"{name}\" with ok but without subkind and qty in data ({data})"),
                        );
                    }
                } else {
                    self.rec(&peer, Outcome::Pass, "take", format!("answered take on \"{name}\" ({r})"));
                }
            }
            Probe::Interact(name) => {
                if timed_out {
                    self.rec(
                        &peer,
                        Outcome::Fail,
                        "interact",
                        format!("did not answer interact on \"{name}\" although it asked for a result"),
                    );
                } else {
                    self.rec(&peer, Outcome::Pass, "interact", format!("answered interact on \"{name}\" (ok={ok})"));
                }
            }
        }
    }

    /// Reports probes whose expected reaction did not come in time.
    fn check_deadlines(&mut self, peer: &str) {
        let st = self.peers.get_mut(peer).expect("exists");
        let mut out = Vec::new();
        if let Some(t) = st.malformed_at
            && t.elapsed() > Duration::from_secs(4)
            && !st.malformed_reported
        {
            st.malformed_reported = true;
            out.push((Outcome::Pass, "malformed", "stayed connected after receiving malformed messages"));
        }
        if let Some((_, _, t)) = &st.damage
            && t.elapsed() > Duration::from_secs(3)
        {
            st.damage = None;
            out.push((Outcome::Warn, "damage", "did not lower hp within 3 s after a damage event; owners may decline, but should honour reasonable requests"));
        }
        if let Some((_, Some(t))) = &st.gift
            && t.elapsed() > Duration::from_secs(2)
        {
            st.gift = None;
            out.push((
                Outcome::Fail,
                "transfer",
                "accepted a transfer but did not broadcast the entity as its own within 2 s",
            ));
        }
        for (o, c, t) in out {
            self.rec(peer, o, c, t.to_string());
        }
    }

    /// Runs the probe schedule for each peer.
    fn on_tick(&mut self) {
        let ids: Vec<String> = self.peers.keys().cloned().collect();
        for peer in ids {
            let st = &self.peers[&peer];
            if !st.connected || st.finished {
                continue;
            }
            let Some(joined) = st.joined else { continue };
            if st.snapshot_at.is_none() {
                if joined.elapsed() > Duration::from_secs(5) && !st.snapshot_checked {
                    self.peers.get_mut(&peer).expect("exists").snapshot_checked = true;
                    self.rec(&peer, Outcome::Fail, "snapshot", "sent no snapshot_end within 5 s of hello".into());
                }
                continue;
            }
            if self.args.no_probes {
                continue;
            }
            let since = st.snapshot_at.expect("checked").elapsed();
            self.check_deadlines(&peer);
            let step = self.peers[&peer].step;
            let node = self.world.node.clone();
            match step {
                0 if since > Duration::from_secs(1) => {
                    node.send_raw_text(
                        &peer,
                        json!({"type": "jamlink_probe_unknown_type", "jamlinkProbe": true}).to_string(),
                    );
                    node.send_raw_text(&peer, json!({"type": "chat", "name": self.args.name, "text": format!("{} is checking this game's Jamlink support", self.args.name), "jamlinkProbe": {"unknownField": true}}).to_string());
                    let id = node.send_event_to(&peer, "0000000000000000:1", "damage", json!({"amount": 1}), true);
                    self.probes.insert(id, (peer.clone(), Probe::UnknownTarget));
                    let target = self.owned_by(&peer).map(|e| e.id.clone()).next();
                    match target {
                        Some(t) => {
                            let id =
                                node.send_event(&t, "jamlink_test.ping", json!({"jamlinkProbe": true}), None, true);
                            self.probes.insert(id, (peer.clone(), Probe::UnknownEvent));
                        }
                        None => self.rec(
                            &peer,
                            Outcome::Info,
                            "no_entities",
                            "owns no entities yet, so the event probes were skipped".into(),
                        ),
                    }
                    self.peers.get_mut(&peer).expect("exists").step = 1;
                }
                1 if since > Duration::from_secs(2) => {
                    if !self.args.gentle {
                        let dmg = self
                            .owned_by(&peer)
                            .find(|e| e.hp.is_some_and(|hp| hp > 1.0) && !e.has_tag("invulnerable"))
                            .map(|e| (e.id.clone(), e.hp.unwrap_or(0.0)));
                        if let Some((id, hp)) = dmg {
                            node.send_event(
                                &id,
                                "damage",
                                json!({"amount": 1, "damageType": "physical", "jamlinkProbe": true}),
                                None,
                                false,
                            );
                            self.peers.get_mut(&peer).expect("exists").damage = Some((id, hp, Instant::now()));
                        }
                        let take = self
                            .owned_by(&peer)
                            .find(|e| e.has_tag("pickup"))
                            .map(|e| (e.id.clone(), e.name.clone().unwrap_or_else(|| e.id.clone())));
                        if let Some((id, name)) = take {
                            let ev = node.send_event(&id, "take", json!({"qty": 1}), None, true);
                            self.probes.insert(ev, (peer.clone(), Probe::Take(name)));
                        }
                    }
                    let inter = self
                        .owned_by(&peer)
                        .find(|e| e.has_tag("interactable"))
                        .map(|e| (e.id.clone(), e.name.clone().unwrap_or_else(|| e.id.clone())));
                    if let Some((id, name)) = inter {
                        let ev = node.send_event(&id, "interact", json!({}), None, true);
                        self.probes.insert(ev, (peer.clone(), Probe::Interact(name)));
                    }
                    let name = self.peer_name(&peer);
                    let gift = self.world.spawn_gift(&name);
                    self.world.tick(0.0);
                    node.offer_transfer(&gift, &peer);
                    self.peers.get_mut(&peer).expect("exists").gift = Some((gift, None));
                    self.peers.get_mut(&peer).expect("exists").step = 2;
                }
                // Malformed input last, after the event probes have their answers,
                // so a game that crashes on it still gets its event results reported.
                2 if since > Duration::from_secs(8) => {
                    node.send_raw_text(&peer, r#"{"type":"entity","id":"jamlinkProbe","pos":[1,2]}"#);
                    node.send_raw_text(&peer, "jamlinkProbe: this is not JSON");
                    let st = self.peers.get_mut(&peer).expect("exists");
                    st.malformed_at = Some(Instant::now());
                    st.step = 3;
                }
                3 => {
                    let st = &self.peers[&peer];
                    let done = since > Duration::from_secs(13)
                        && st.damage.is_none()
                        && st.gift.is_none()
                        && st.malformed_reported
                        && !self.probes.values().any(|(p, _)| *p == peer);
                    if done {
                        self.peers.get_mut(&peer).expect("exists").finished = true;
                        let name = self.peer_name(&peer);
                        self.report.note(&format!("{} : probes finished; passive checks continue", who(&peer, &name)));
                    }
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let mut cfg = Config::new(args.name.clone(), FACTION);
    cfg.team = Some("Jamlink".into());
    cfg.handles =
        ["damage", "heal", "push", "status", "interact", "take", "give", "command"].map(String::from).to_vec();
    cfg.ports = args.port..=args.port.saturating_add(8);
    cfg.seeds = args.seeds.clone();
    cfg.discovery = !args.no_discovery;
    cfg.log_path = args.log.clone();

    let (node, mut notices) = match start(cfg).await {
        Ok(x) => x,
        Err(e) => {
            eprintln!("Could not start: {e}");
            return ExitCode::from(2);
        }
    };

    println!("Jamlink test peer {} \"{}\"", env!("CARGO_PKG_VERSION"), args.name);
    println!("  peer ID {}, listening on ws://0.0.0.0:{}/jamlink", node.peer_id(), node.port());
    println!(
        "  discovery: {}, seeds: {}",
        if args.no_discovery { "off" } else { "UDP 45800" },
        if args.seeds.is_empty() { "none".into() } else { args.seeds.join(", ") }
    );
    println!("\nIn your game you should now see, near the origin:");
    println!("  - \"Walking north\": moves toward -Z (north) at 1.5 m/s, then jumps back every ~7 s");
    println!("  - \"Test dummy\": circles around (8, 0, 0), 100 hp, faces where it walks");
    println!(
        "  - \"Test wood\" (item, pickup), \"Test tree\" (resource, pickup), \"Test chest\" (accepts give/interact)"
    );
    println!("  - \"Test unit\" (commandable) and a yellow cone labelled \"Unknown kind\" (an unknown kind)");
    println!("Press Ctrl-C to stop and print the summary.\n");

    let world = World::new(node.clone(), args.name.clone());
    let mut t = Tester { report: Report::new(), world, peers: HashMap::new(), probes: HashMap::new(), args };
    let mut tick = tokio::time::interval(TICK);
    let stop_at = t.args.duration.map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    let stop = async {
        match stop_at {
            Some(at) => tokio::time::sleep_until(at).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(stop);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = &mut stop => break,
            _ = tick.tick() => {
                t.world.tick(TICK.as_secs_f64());
                t.on_tick();
            }
            n = notices.recv() => match n {
                Some(n) => t.on_notice(n),
                None => break,
            },
        }
    }
    node.shutdown().await;
    if t.report.summary() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
