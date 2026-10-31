//! End-to-end tests: real nodes talking over loopback.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use jamlink::{Config, Entity, Handle, Notice, Severity, start};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};

fn config(name: &str, ports: std::ops::RangeInclusive<u16>, seeds: &[String]) -> Config {
    let mut c = Config::new(name, name.to_lowercase().replace(' ', "_"));
    c.ports = ports;
    c.seeds = seeds.to_vec();
    c.discovery = false;
    c
}

/// Waits for a notice matching `pred`, failing the test on any rule-break notice.
async fn wait_for<F>(rx: &mut UnboundedReceiver<Notice>, what: &str, mut pred: F) -> Notice
where
    F: FnMut(&Notice) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let n = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("node stopped");
        if let Notice::Violation(v) = &n
            && v.severity >= Severity::Warn
        {
            panic!("unexpected violation while waiting for {what}: {v:?}");
        }
        if pred(&n) {
            return n;
        }
    }
}

/// Collects notices for `dur` and returns them, failing on any rule-break notice.
async fn drain(rx: &mut UnboundedReceiver<Notice>, dur: Duration) -> Vec<Notice> {
    let mut out = Vec::new();
    let end = tokio::time::Instant::now() + dur;
    while let Ok(Some(n)) = tokio::time::timeout_at(end, rx.recv()).await {
        if let Notice::Violation(v) = &n
            && v.severity >= Severity::Warn
        {
            panic!("unexpected violation: {v:?}");
        }
        out.push(n);
    }
    out
}

fn goblin(node: &Handle) -> Entity {
    let mut e = Entity::new(node.new_entity_id(), "creature", [1.0, 0.0, -2.0]);
    e.hp = Some(10.0);
    e.hp_max = Some(10.0);
    e.name = Some("Goblin".into());
    e
}

#[tokio::test]
async fn two_peers_full_session() {
    let (a, mut a_rx) = start(config("Alpha", 46801..=46809, &[])).await.unwrap();
    let g = goblin(&a);
    a.upsert(g.clone());

    // B joins later and must receive A's world in the snapshot.
    let (b, mut b_rx) = start(config("Beta", 46801..=46809, &[format!("127.0.0.1:{}", a.port())])).await.unwrap();
    wait_for(&mut b_rx, "B sees A", |n| matches!(n, Notice::PeerJoined(p) if p.peer_id == a.peer_id())).await;
    wait_for(&mut b_rx, "goblin in snapshot", |n| matches!(n, Notice::EntityUpdated { entity, new: true } if entity.id == g.id && entity.owner == a.peer_id())).await;
    wait_for(&mut b_rx, "snapshot_end", |n| matches!(n, Notice::SnapshotEnd { count: 1, .. })).await;
    wait_for(&mut a_rx, "A sees B", |n| matches!(n, Notice::PeerJoined(p) if p.peer_id == b.peer_id())).await;

    // Events with a result.
    let ev = b.send_event(&g.id, "damage", json!({"amount": 3}), None, true);
    let n = wait_for(&mut a_rx, "damage event", |n| matches!(n, Notice::Event { .. })).await;
    let Notice::Event { from, event } = n else { unreachable!() };
    assert_eq!(from, b.peer_id());
    assert_eq!(event.event, "damage");
    assert_eq!(event.data["amount"], 3);
    a.reply(&from, &event.event_id, true, None, None);
    let r =
        wait_for(&mut b_rx, "result", |n| matches!(n, Notice::EventResult { event_id, .. } if *event_id == ev)).await;
    assert!(matches!(r, Notice::EventResult { ok: true, .. }));

    // An event for an entity A does not own is answered by the node itself.
    let ev = b.send_event_to(a.peer_id(), "0000000000000000:1", "damage", json!({"amount": 1}), true);
    let r = wait_for(
        &mut b_rx,
        "unknown target result",
        |n| matches!(n, Notice::EventResult { event_id, .. } if *event_id == ev),
    )
    .await;
    assert!(matches!(r, Notice::EventResult { ok: false, reason: Some(ref s), .. } if s == "unknown_target"));

    // Unanswered event: A's node declines it automatically after 4 s.
    let ev = b.send_event(&g.id, "interact", json!({}), None, true);
    wait_for(&mut a_rx, "interact event", |n| matches!(n, Notice::Event { .. })).await;
    let end = tokio::time::Instant::now() + Duration::from_secs(6);
    let mut declined = false;
    while let Ok(Some(n)) = tokio::time::timeout_at(end, b_rx.recv()).await {
        if let Notice::EventResult { event_id, ok: false, reason: Some(r), .. } = &n
            && *event_id == ev
            && r == "declined"
        {
            declined = true;
            break;
        }
    }
    assert!(declined, "expected an automatic decline");
    // Swallow A's self-warning about the unanswered event.
    let n = tokio::time::timeout(Duration::from_secs(1), a_rx.recv()).await.unwrap().unwrap();
    assert!(matches!(n, Notice::Violation(ref v) if v.code == "self_no_reply"), "{n:?}");

    // Transfer the goblin from A to B.
    a.offer_transfer(&g.id, b.peer_id());
    let n = wait_for(&mut b_rx, "offer", |n| matches!(n, Notice::TransferOffer { .. })).await;
    let Notice::TransferOffer { from, id } = n else { unreachable!() };
    b.reply_transfer(&from, &id, true);
    wait_for(&mut a_rx, "accepted", |n| matches!(n, Notice::TransferAnswered { accepted: true, .. })).await;
    wait_for(&mut b_rx, "B owns goblin", |n| matches!(n, Notice::OwnershipChanged { to, .. } if to == b.peer_id()))
        .await;
    wait_for(
        &mut a_rx,
        "update from new owner",
        |n| matches!(n, Notice::EntityUpdated { entity, .. } if entity.id == g.id && entity.owner == b.peer_id()),
    )
    .await;

    // B leaves gracefully; A drops everything B owns, including the goblin.
    b.shutdown().await;
    wait_for(
        &mut a_rx,
        "goblin removed",
        |n| matches!(n, Notice::EntityRemoved { id, reason, .. } if *id == g.id && reason == "peer_left"),
    )
    .await;
    wait_for(&mut a_rx, "B left", |n| matches!(n, Notice::PeerLeft { graceful: true, .. })).await;
    a.shutdown().await;
}

#[tokio::test]
async fn duplicate_connections_resolve_to_one() {
    // Each peer seeds the other, so both dial at once.
    let (a, mut a_rx) = start(config("Alpha", 46821..=46821, &["127.0.0.1:46822".into()])).await.unwrap();
    let (b, mut b_rx) = start(config("Beta", 46822..=46822, &["127.0.0.1:46821".into()])).await.unwrap();
    let a_notices = drain(&mut a_rx, Duration::from_secs(4)).await;
    let b_notices = drain(&mut b_rx, Duration::from_secs(1)).await;
    for (name, notices) in [("A", &a_notices), ("B", &b_notices)] {
        let joined = notices.iter().filter(|n| matches!(n, Notice::PeerJoined(_))).count();
        let left = notices.iter().filter(|n| matches!(n, Notice::PeerLeft { .. })).count();
        assert_eq!((joined, left), (1, 0), "{name}: {notices:?}");
    }
    // The surviving connection works.
    let e = goblin(&a);
    a.upsert(e.clone());
    wait_for(
        &mut b_rx,
        "entity over surviving connection",
        |n| matches!(n, Notice::EntityUpdated { entity, .. } if entity.id == e.id),
    )
    .await;
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn gossip_completes_the_mesh() {
    let (a, mut a_rx) = start(config("Alpha", 46831..=46839, &[])).await.unwrap();
    let (b, mut b_rx) = start(config("Beta", 46831..=46839, &[format!("127.0.0.1:{}", a.port())])).await.unwrap();
    wait_for(&mut b_rx, "B joined A", |n| matches!(n, Notice::PeerJoined(_))).await;
    // C only knows B, and must find A through gossip.
    let (c, mut c_rx) = start(config("Gamma", 46831..=46839, &[format!("127.0.0.1:{}", b.port())])).await.unwrap();
    let mut seen = std::collections::HashSet::new();
    while seen.len() < 2 {
        if let Notice::PeerJoined(p) = wait_for(&mut c_rx, "C joins both", |n| matches!(n, Notice::PeerJoined(_))).await
        {
            seen.insert(p.peer_id);
        }
    }
    assert!(seen.contains(a.peer_id()) && seen.contains(b.peer_id()));
    wait_for(&mut a_rx, "A sees C", |n| matches!(n, Notice::PeerJoined(p) if p.peer_id == c.peer_id())).await;
    for n in [a, b, c] {
        n.shutdown().await;
    }
}

/// Accepts WebSocket connections and answers each with a hello from a peer of
/// the given version. Returns its port and how many subprotocol offers and
/// connections it has seen.
async fn fake_peer(version: &'static str) -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (accepted, offers) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (accepted_out, offers_out) = (accepted.clone(), offers.clone());
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            accepted.fetch_add(1, Ordering::SeqCst);
            let offers = offers.clone();
            tokio::spawn(async move {
                #[allow(clippy::result_large_err)]
                let callback = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                    if req.headers().contains_key("Sec-WebSocket-Protocol") {
                        offers.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(resp)
                };
                let mut ws = tokio_tungstenite::accept_hdr_async(stream, callback).await.unwrap();
                let hello = json!({"type": "hello", "protocol": "jamlink", "version": version,
                    "peerId": "0123456789abcdef", "name": "Fake", "faction": "fake", "port": port, "peers": []});
                ws.send(Message::text(hello.to_string())).await.unwrap();
                while let Some(Ok(_)) = ws.next().await {}
            });
        }
    });
    (port, accepted_out, offers_out)
}

#[tokio::test]
async fn dials_without_a_subprotocol() {
    let (port, accepted, offers) = fake_peer("1.0").await;
    let (a, _a_rx) = start(config("Alpha", 46841..=46849, &[format!("127.0.0.1:{port}")])).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    assert_eq!(offers.load(Ordering::SeqCst), 0);
    a.shutdown().await;
}

#[tokio::test]
async fn incompatible_seed_is_not_redialled() {
    let (port, accepted, _) = fake_peer("2.0").await;
    let (a, mut a_rx) = start(config("Alpha", 46851..=46859, &[format!("127.0.0.1:{port}")])).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut mismatches = 0;
    while let Ok(Some(n)) = tokio::time::timeout_at(deadline, a_rx.recv()).await {
        if matches!(n, Notice::Violation(ref v) if v.code == "version_mismatch") {
            mismatches += 1;
        }
    }
    assert_eq!(mismatches, 1);
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    a.shutdown().await;
}
