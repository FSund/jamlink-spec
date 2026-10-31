# AI game jam

Several teams build different games, mostly with coding agents, and connect
them on the same LAN through Jamlink, a small peer-to-peer protocol over
WebSocket and JSON. Each game owns its own entities, sees everyone else's,
and affects them by sending events to their owner.

## Files

| File | What it is |
| --- | --- |
| [SPEC.md](SPEC.md) | The Jamlink 1.0 protocol spec |
| [template/AGENTS.md](template/AGENTS.md) | Rules for coding agents. Copy into every game repo |
| [template/jamlink.schema.json](template/jamlink.schema.json) | JSON Schema (draft 2020-12) for every message |
| [reference/](reference/) | Rust reference implementation and `jamlink-test-peer`, a conformance tester |

## Starting a game

1. Create the game repo.
2. Copy `SPEC.md`, `template/AGENTS.md` and `template/jamlink.schema.json`
   into its root. Rename `AGENTS.md` to `CLAUDE.md` if your agent expects that.
3. Fill in the "This game" section at the end of `AGENTS.md`.
4. Ask your agent to build the Jamlink module first, and test it against a
   second instance of the game before building gameplay on top.

## Testing a game

Build the test peer once (see [reference/README.md](reference/README.md)),
start your game, then run:

```
jamlink-test-peer --seed 127.0.0.1:45801 --port 45810 --duration 30
```

It joins like another game, spawns test entities you should see in your game,
probes your game, and prints PASS, WARN and FAIL lines in plain language. The
exit code is 1 when anything fails. Paste its output to your coding agent and
ask it to fix every FAIL.
