# Jamlink multiplayer: rules for coding agents

This game joins a LAN game jam where every game shares one world through the
Jamlink 1.0 protocol. The full spec is SPEC.md in this repo. Every message is
described by jamlink.schema.json in this repo.

Follow the spec exactly. If the spec and your assumptions disagree, the spec
wins. Never invent message types or top-level fields: game-specific data goes
in `ext` (entities) or `data` (custom events named like `ourgame.thing`).
If something is unclear, ask the human instead of guessing.

## Architecture

1. Put all networking in one Jamlink module. Game code talks to that module
   (spawn, update, remove, send event, read replicas), never to sockets.
2. The module needs: a UDP broadcast sender and listener on port 45800, a
   WebSocket server bound to 0.0.0.0 on the first free port from 45801 to
   45809 at path /jamlink, and a WebSocket client for dialling other peers.
   Do not offer a WebSocket subprotocol.
3. Run networking off the game thread or poll it without blocking. Hand
   received messages to the game loop through a queue.
4. Support manual seed addresses (host:port) from config or command line.
5. If binding UDP 45800 fails, as it can for a second instance on one
   machine, log it and carry on with seeds. It must never crash the game.
6. Set TCP_NODELAY on every connection where the engine allows it.

## Hard rules

- peerId: 16 random lowercase hex characters, new on every start.
  Entity IDs are "<peerId>:<n>" with a counter from 1, never reused.
- Treat received IDs as opaque. Find an entity's owner from its `owner`
  field, never the ID prefix: a transfer changes the owner but not the ID.
- Beacon every 1 s to 255.255.255.255:45800. Take the peer's host from the
  packet's source address, not the payload.
- Dial a discovered peer only if your peerId is smaller (string comparison).
  If a smaller peer has not dialled you within 5 s, dial it anyway. Always
  dial seeds. If two connections to one peer exist, keep the one dialled by
  the smaller peerId and close the other with code 4001.
- Retry failed dials every 3 s until connected. If a seed turns out to be
  yourself, close with code 4002 and do not dial it again.
- The first message on every connection is `hello`. Send nothing else until
  you have received theirs. Different major version: send error
  version_mismatch, close with code 4003 and do not dial that peer again.
- `hello` lists the peers you are connected to. Broadcast `peers` whenever a
  connection finishes hello, and apply the dial rule to any peer you learn
  from either. If a listed host is loopback, use the sender's address.
- After hello, send `entity` for every entity you own, then `snapshot_end`.
- Broadcast only on connections that have finished hello.
- Only the owner sends `entity` and `remove` for an entity. Ignore those
  messages from anyone who is not the entity's current owner.
- Every `entity` message is the complete state. Replace the replica; an
  omitted optional field means its default.
- Never modify another game's entity locally. Send an `event` directly to
  its owner and let the owner decide.
- When an event you receive has wantResult: true, always reply with a
  `result`, even if you decline.
- `take` and `give` always set wantResult: true. Change inventories only
  after a `result` with ok: true.
- Always apply an incoming `transfer` from the current owner (set the owner
  field). Offering and accepting transfers is optional: if not implemented,
  ignore `transfer_offer`. If you do accept, broadcast a full `entity` with
  yourself as owner when `transfer` arrives, and again 1 s later.
- When a peer's last connection closes, or it is silent for 5 s (close with
  code 4005), remove all entities it owns. Closing a duplicate with 4001 is
  not a departure: keep its entities.
- On quit, send `bye` on every connection, then close with code 1000.
- Send `heartbeat` on any connection you have sent nothing on for 1 s, even
  if you are receiving on it.
- Ignore unknown fields, message types and events. Draw an unknown kind as a
  marker using `display`. Treat an unknown faction as another game's side.
- Check incoming messages and drop invalid ones: required fields, types,
  vector lengths, finite numbers. A hand-written check is fine if the engine
  has no JSON Schema validator. Bad input from another game must never crash
  this game.
- One JSON object per WebSocket text frame. Numbers must be finite.

## Coordinates: get this right

- Metres, seconds, radians. Right-handed, +Y up, +X east, -Z north.
- yaw = radians about +Y. 0 faces north (-Z). Positive turns
  counter-clockwise seen from above, so pi/2 faces west.
- 2D top-down games use the XZ plane with y = 0.
- Conversions to Jamlink (position; yaw):
  - Godot 3D, Bevy: identical.
  - Unity 3D: (x, y, -z); yaw = -radians(eulerY).
  - Unreal: (Y/100, Z/100, -X/100); yaw = -radians(Yaw).
  - 2D with screen y down: (x/ppm, 0, y/ppm); yaw = -angle - pi/2.
  - 2D with y up: (x, 0, -y); yaw = angle - pi/2.
  (2D angles: 0 rad faces +x on screen.)
- Write a test: moving north in this game must make the sent z decrease.

## Rates and smoothing

- Moving entities: 10 Hz, never above 20 Hz. Static entities: on change.
- Render remote entities 100 ms behind the newest `t` from their owner,
  interpolating. Extrapolate with `vel` for at most 250 ms, except a
  `projectile`, which keeps going until its `remove`. Snap on jumps over
  5 m. `t` is per owner and never compared across peers.

## Definition of done

- Two instances on one machine, connected through seeds such as
  127.0.0.1:45802, see and interact with each other's entities.
- An instance started late sees everything already in the world.
- Killing one instance removes its entities from the other within 5 s.
- Every message this game sends validates against jamlink.schema.json
  (add a debug-mode assertion).
- A command-line flag logs every sent and received message to a file.
- The Jamlink test peer reports no FAIL against this game. Ask the human to
  run it (`jamlink-test-peer --seed 127.0.0.1:<our port> --duration 30`) and
  paste the output, then fix every FAIL. Fix each WARN that is a real bug and
  tell the human about any you leave: some are allowed, such as declining
  damage. It also spawns a "Walking north" NPC: in this game it must move
  north (-Z), or the coordinate conversion is wrong.
- If you are unsure how a rule works, ask the human for
  reference/src/node.rs from the jam repo. It shows one correct way to do it.

## This game

- Name:
- Faction (lowercase snake_case):
- Entity kinds we own:
- Events we handle:
- Custom events and ext fields we use:
