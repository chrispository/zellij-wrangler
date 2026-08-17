# pane-comms — handoff status (2026-08-16)

Working state for the next session. Spec: `pane_comms.md` (§1.1 = plugin + companion CLI +
wrappers on stock zellij, no fork). Repo root = the zellij checkout (`zellij-wrangler/`), the
deliverable lives in `pane-comms/`.

## Status: E2E suite GREEN (28/28), pz unit tests 7/7

All previously-open failures are fixed. The E2E now passes end-to-end on stock zellij 0.44.3.

## Root causes found & fixed this session (do NOT re-derive)

1. **Listeners/ask CLIs exited immediately** — the server auto-unblocks a Cli pipe when the
   plugin's `pipe()` returns (`zellij-server/src/plugins/pipes.rs:107-130`). Fix: hub calls
   `block_cli_pipe_input(pipe_id)` in the `listen` arm (before the ack) and the `ask` arm
   (when registering the pending ask); the existing `reply()` sends the explicit unblock when
   answering. (`hub/src/main.rs`)
2. **Ask never answered — ticks never fired.** `set_timeout` alone is NOT enough: the plugins
   thread only delivers an event if the plugin is subscribed to its `EventType`
   (`wasm_bridge.rs update_plugins` gates on `subs.contains(&event_type)`). Fix: hub `load()`
   now calls `subscribe(&[EventType::Timer])`.
3. **`status` panicked the hub** — `get_pane_info` requires the `ReadApplicationState`
   permission, which the hub never requested. The shim panics (shim.rs:269) when denied,
   killing the plugin instance; blocked ask/listen pipes then never unblock (plugin death
   only unblocks non-explicitly-blocked pipes), so every subsequent pipe command hung until
   its outer timeout. Fix: added `PermissionType::ReadApplicationState` to `load()`, and
   `ReadApplicationState` to the `tests/e2e.sh` permissions heredoc (a pre-seed mismatch
   leaves the hub at a permission prompt and caches all CliPipes — "did not answer in time"
   everywhere).
4. **Listeners never printed payloads — hub output had no newlines.** All `cli_pipe_output`
   payloads were emitted as bare JSON with no line terminator; the client writes them to
   stdout verbatim, so `pz listen`'s `BufReader::lines()` never saw a complete line. Fix:
   every hub envelope (replies, acks, fan-out, status, channels) is now newline-terminated
   (NDJSON). `hub_rpc`'s `stdout.trim()` parse is unaffected.
5. **Listen ack leaked into `pz listen --format json`** — the ack envelope lacked the `event`
   field, so the `_` arm printed it raw. Fix: hub sets `"event":"ack"` on the listen ack;
   pz's existing `Some("ack") | Some("subscribed") => continue` arm suppresses it.
6. **`wait --until '/^READY-REGEX/'` timed out (E2E bug, not product bug)** — two compounding
   issues in `tests/e2e.sh`:
   - `$(printf 'echo READY-REGEX\n')` — command substitution strips the trailing newline, so
     the marker was typed but never submitted. ANSI-C quoting (`$'...\n'`) preserves it.
   - Earlier raw writes (marker-A, READY-NOW) leave a partially-typed line on the pane; the
     echo marker then merges into one garbage command (`marker-A…echo: command not found`)
     and never produces the output line. Fix: the regex marker leads with `\n` to submit any
     pending line first, then runs `echo READY-REGEX` on a clean prompt.
   - The "race" hypothesis in the earlier notes was wrong — the is_initial snapshot arrives
     well before the 1.5s marker.
7. **`ask timeout exits 3` test premise was wrong** — the tty driver echoes typed input even
   while a foreground job runs, so a plain `sleep 8` pane still produced output and the ask
   succeeded (it only passed before because ticks never ran). Fix: test sends
   `stty -echo; sleep 8` first, so typed input produces no output and the ask genuinely times
   out.
8. **`status terminal_999` exited 2, spec says 4** — `cmd_status` mapped resolve_target errors
   to `fail(2)`; per the spec (`pz status <target>` — exit 4 unknown/expired) it now fails
   with 4. (`pz/src/main.rs`)

## Environment (verified)

| Item | Value |
|---|---|
| Installed zellij | **0.44.3** (`/usr/bin/zellij`) — the E2E target |
| Checkout | 0.45.0-dev (reference source; line numbers above are this checkout) |
| rust | 1.97.1 system; `rust-wasm` installed |
| Build | `cargo build -p hub --target wasm32-wasip1 --release` from `pane-comms/` |
| Artifacts | land in `pane-comms/target/` ONLY when `CARGO_TARGET_DIR=pane-comms/target` is set — the parent repo's `.cargo/config.toml` redirects `target` to the zellij repo root otherwise |
| Running sessions | user's `glowing-echidna` (+`adventurous-goose`) — DO NOT touch. Test sessions `pztest` / `pzdbg` (kill/delete freely) |

## What's built and verified

- **`pane-comms/pz/`** — companion CLI (`send` pane+`--channel`, `ask`, `wait`, `listen`,
  `status`, `targets`; exit codes 0/1/2/3/4 per spec §"pz CLI contract"). Builds clean; unit
  tests 7/7 (`cargo test -p pz`).
- **`pane-comms/hub/`** — hub plugin: channels, ask-wait (Timer-subscribed poll loop), status,
  targets, NDJSON envelopes, `block_cli_pipe_input` on listen/ask pipes. Builds to `hub.wasm`
  and loads into the 0.44.3 server.
- **`pane-comms/tests/e2e.sh`** — **PASS=28 FAIL=0** (ran twice, stable). Full surface:
  write-chars + dump-screen (incl. cross-tab), `tab:`/`tab-name:`/`active` resolution, all
  exit-code-2 paths, `wait` substring + regex + timeout + pre-existing-excluded, channel
  listen raw/json + fan-out to 2 listeners, `ask` output + missing-pane + timeout, `status`
  ok + unknown (exit 4), `targets`.
- **`pane-comms/README.md`**, `layouts/e2e.kdl`, `layouts/e2e-config.kdl` (README updated:
  NDJSON termination + `event:"ack"` on the listen ack documented).

## Installed for real use (2026-08-16)

- **`pz`** → `~/.local/bin/pz` (on PATH). Rebuilt with `default_hub_url` patched to prefer
  `~/.local/share/zellij-wrangler/hub.wasm` (installed layout) before the cargo candidates;
  `$PZ_HUB_URL` still overrides.
- **`hub.wasm`** → `~/.local/share/zellij-wrangler/hub.wasm`.
- **Permissions** pre-seeded in `~/.cache/zellij/permissions.kdl` for the installed path
  (same 4 permissions as the E2E heredoc) — hub loads without the UI prompt.
- **Agent skill**: `pane-comms/skills/zellij-pane-comms/SKILL.md` (canonical, in-repo so it
  can be reviewed/pushed). Installed via symlinks:
  `~/.agents/skills/zellij-pane-comms` → repo copy,
  `~/.codex/skills/zellij-pane-comms` and `~/.config/opencode/skills/zellij-pane-comms` →
  that. `git push` updates every installed agent.
- **Verified live** in the user's session: `pz targets`/`status` (hub auto-launch), 
  `dump-screen --full` reads both agent panes, `pz send --channel` → `pz listen` round-trip
  (delivered to 1 listener, exit 0).
- **Deterministic agent glue** (skill activation alone is retrieval-based, not guaranteed):
  `~/.codex/AGENTS.md` gained a "Pane comms (zellij)" section (always loaded at startup);
  `~/.config/opencode/opencode.json` gained `"instructions": [...zellij-pane-comms/SKILL.md]`
  (always loaded at startup).

Note: the running codex/opencode TUIs load skills at startup — restart the agent pane to pick
up the new skill.

## Remaining (optional / out of scope unless asked)

- Layout-based hub pre-load (`run_plugin` in `layouts/e2e.kdl` — makes the plugin pane exist
  before any pipe, sidestepping the screen's permission-request caching).
- Spec's upstream PR candidates (fork/upstream route, M5 status-token model).

## Commands

```sh
cd /home/chris/Documents/zellij-wrangler/pane-comms
export CARGO_TARGET_DIR="$PWD/target"
cargo build -p hub --target wasm32-wasip1 --release   # -> target/wasm32-wasip1/release/hub.wasm
cargo build -p pz                                      # -> target/debug/pz
cargo test -p pz                                       # 7 unit tests
./tests/e2e.sh                                         # full E2E, own session, cleans up
```

Manual hub RPC probe (session must have the permissions file; keys are plain paths in
`$XDG_CACHE_HOME/zellij/permissions.kdl` — the plugin's stored location, NOT the file:// URL):
`zellij --session pztest pipe --plugin file://<abs>/hub.wasm --name dbg1 -- '{"cmd":"channels"}'`
