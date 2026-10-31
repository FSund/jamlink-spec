# Jamlink reference implementation and test peer

This folder is a Rust crate with two parts:

- **`jamlink-test-peer`**, a conformance test peer. It joins the LAN like any
  game, spawns a set of test entities, answers events the way a well-behaved
  game should, probes every game it meets, and reports every rule break in
  plain language.
- **The `jamlink` library**, a complete implementation of `SPEC.md`. Rust and
  Bevy games can use it directly; teams in other languages can point their
  coding agent at `src/node.rs` as a working example.

The schema is embedded from `../template/jamlink.schema.json`, so build the
crate from inside this repo.

## Build

Install Rust from <https://rustup.rs>, then run this in this folder:

```
cargo build --release
```

The binary is `target/release/jamlink-test-peer` (`.exe` on Windows). The
first run may trigger a firewall prompt: allow private networks, or other
machines cannot connect.

## Test your game

Start your game, then run the test peer on any machine on the same network:

```
jamlink-test-peer
```

It finds games through UDP beacons. To test against a game on the same
machine, or on a network that blocks broadcast, point it at the game directly:

```
jamlink-test-peer --seed 127.0.0.1:45801 --port 45810
```

Leave it running for about 20 seconds, then press Ctrl-C to get a summary. The
exit code is 1 if any game has a failure, so it also works in scripts:

```
jamlink-test-peer --seed 127.0.0.1:45801 --duration 30
```

| Option | Meaning |
| --- | --- |
| `--seed HOST:PORT` | Dial this peer directly. Repeat for several |
| `--no-discovery` | No UDP beacons; seeds only |
| `--port N` | First WebSocket port to try (default 45801; the next 8 are tried too) |
| `--duration SECONDS` | Stop after this long and print the summary |
| `--log FILE` | Write every sent and received message to a JSON-lines file |
| `--gentle` | Skip probes that change other games' state (damage, take) |
| `--no-probes` | Only watch and check, send no probes |
| `--name NAME` | Name shown to other games |
| `--verbose` | Also list every entity other games own, and notes about other test peers' deliberate probes |

### What it checks

Every message the test peer receives is checked against the schema and the
rules in `SPEC.md`:

- hello comes first, versions match, and the snapshot count is right
- only owners send `entity`, `remove` and `transfer`, and `owner` matches the sender
- `t` never goes backwards for an entity, and updates stay at or below 20 Hz
- a heartbeat arrives after 1 s of silence, and the peer is dropped after 5 s
- every event that asks for a result gets one within 5 s
- the dial rule is followed, and no binary frames, oversized messages or invalid JSON are sent

It also probes each game after the game's snapshot:

| Probe | Expected reaction |
| --- | --- |
| Unknown message type, and a chat with an unknown field | Ignored, connection stays up |
| Event for an entity the game does not own | Result `not_owner` or `unknown_target` |
| Unknown custom event with `wantResult` | Result `unsupported` |
| `damage 1` on an entity with hp | hp drops (warning only if not) |
| `take 1` on a `pickup` entity | A result, with `subkind` and `qty` when ok |
| `interact` on an `interactable` entity | A result |
| A transfer offer of a "Gift" item | Accept and rebroadcast as owner, or decline (transfers are optional) |
| Malformed messages, sent last | Dropped, connection stays up |

### What you should see in your game

The test peer spawns these near the origin, so you can check coordinates and
rendering by eye:

- **"Walking north"** moves toward -Z at 1.5 m/s, then jumps 10 m back south
  every ~7 s. If it walks south in your game, your Z axis is flipped. The jump
  should be a snap, not a slide.
- **"Test dummy"** circles around (8, 0, 0) facing where it walks, with 100 hp.
  It respawns after being destroyed.
- **"Test wood"** (item) and **"Test tree"** (resource) accept `take`.
  **"Test chest"** accepts `give` and `interact`.
- **"Test unit"** is `commandable`: it obeys `move`, `attack`, `follow` and `stop`.
- A yellow cone labelled **"Unknown kind"** has a kind no game knows, so it
  should be drawn from its display hints.

## Use the library

```rust
use jamlink::{start, Config, Entity, Notice};

let mut cfg = Config::new("My Game", "my_game");
cfg.seeds = vec!["127.0.0.1:45802".into()]; // optional
let (node, mut notices) = start(cfg).await?;

let hero = Entity::new(node.new_entity_id(), "player", [0.0, 0.0, 0.0]);
node.upsert(hero.clone()); // call again at 10 Hz while it moves

while let Some(n) = notices.recv().await {
    match n {
        Notice::EntityUpdated { entity, .. } => { /* draw the replica */ }
        Notice::EntityRemoved { id, .. } => { /* remove it */ }
        Notice::Event { from, event } => {
            // Apply it to our entity, upsert the new state,
            // and reply when event.want_result is set:
            node.reply(&from, &event.event_id, true, None, None);
        }
        Notice::Violation(v) => eprintln!("{:?}: {}", v.severity, v.text),
        _ => {}
    }
}
```

The node runs on Tokio. In Bevy, run it on a Tokio runtime in a background
thread and pass `Notice`s to a system through a channel.

## Develop

```
cargo test     # three nodes talking over loopback
cargo clippy --all-targets
```
