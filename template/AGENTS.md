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
   WebSocket server on the first free port from 45801 to 45809 at path
   /jamlink, and a WebSocket client for dialling other peers.
3. Run networking off the game thread or poll it without blocking. Hand
   received messages to the game loop through a queue.
4. Support manual seed addresses (host:port) from config or command line.

## Hard rules

- peerId: 16 random lowercase hex characters, new on every start.
  Entity IDs are "<peerId>:<n>" with a counter from 1, never reused.
- Beacon every 1 s to 255.255.255.255:45800. Take the peer's host from the
  packet's source address, not the payload.
- Dial a discovered peer only if your peerId is smaller (string comparison).
  If a smaller peer has not dialled you within 5 s, dial it anyway. Always
  dial seeds. If two connections to one peer exist, keep the one dialled by
  the smaller peerId and close the other with code 4001.
- The first message on every connection is `hello`. Send nothing else until
  you have received theirs. Different major version: send error
  version_mismatch and close with code 4003.
- After hello, send `entity` for every entity you own, then `snapshot_end`.
- Only the owner sends `entity` and `remove` for an entity. Ignore those
  messages from anyone who is not the entity's current owner.
- Every `entity` message is the complete state. Replace the replica; an
  omitted optional field means its default.
- Never modify another game's entity locally. Send an `event` directly to
  its owner and let the owner decide.
- `take` and `give` always set wantResult: true. Change inventories only
  after a `result` with ok: true.
- Always apply an incoming `transfer` from the current owner (set the owner
  field). Offering and accepting transfers is optional: if not implemented,
  ignore `transfer_offer`.
- When a peer closes or is silent for 5 s, remove all entities it owns.
- Send `heartbeat` on any connection that carried nothing else for 1 s.
- Ignore unknown fields, message types and events. Draw an unknown kind as a
  marker using `display`. Treat an unknown faction as another game's side.
- Validate incoming messages against the schema and drop invalid ones. Bad
  input from another game must never crash this game.
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
  interpolating. Extrapolate with `vel` for at most 250 ms. Snap on jumps
  over 5 m. `t` is per owner and never compared across peers.

## Definition of done

- Two instances on one machine, connected through seeds such as
  127.0.0.1:45802, see and interact with each other's entities.
- An instance started late sees everything already in the world.
- Killing one instance removes its entities from the other within 5 s.
- Every message this game sends validates against jamlink.schema.json
  (add a debug-mode assertion).
- A command-line flag logs every sent and received message to a file.

## This game

- Name:
- Faction (lowercase snake_case):
- Entity kinds we own:
- Events we handle:
- Custom events and ext fields we use:
