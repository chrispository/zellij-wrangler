# pane-comms — cross-pane & cross-tab communication for zellij

## Agent-aware prompting

`pane-comms` can prompt another LLM as if you typed into its Zellij pane. Agent names are
resolved from the live pane command and title on every invocation, so they do not depend on
fixed pane numbers or tab names such as `Tab #1`.

Built-in agent profiles recognize:

```text
claude, codex, antigravity, opencode (including opencode2+), crush,
pi, omp, hermes, vibe, z-code (and zcode)
```

Use the friendly name directly:

```sh
pz send codex $'Please review the failing test.\n'
pz ask opencode $'What are you working on?\n'
pz send other:codex $'Coordinate with the other Codex pane.\n'
```

You do not have to run `pz` yourself. Once the pane-comms components and the
`zellij-wrangler` skill are installed, just tell an agent what you want: “ask Codex to review
this,” “coordinate with OpenCode,” or “send this to the other Claude.” The agent discovers the
target and invokes `pz` for you, asking which pane to use if there is more than one match.

The trailing newline submits the prompt. `pz` writes the text through the target pane's PTY and
turns that final newline into Zellij's explicit `Enter` key action, which works with full-screen
agent TUIs as well as ordinary shells.

If a name matches more than one pane, `pz` refuses to guess and lists each candidate's pane id,
tab number/name, working directory, and command. The calling agent should ask the user which
candidate to use, then retry with the selected concrete pane id. `other:NAME` excludes the
calling pane from the search.

Discover the current matches with:

```sh
pz agents
pz agents --json
```

Common profiles are built in, but custom wrappers can be added in
`$XDG_CONFIG_HOME/pane-comms/agents.toml` (or `~/.config/pane-comms/agents.toml`):

```toml
[agents.my-codex]
commands = ["my-codex-wrapper"]
aliases = ["backend-codex"]
titles = ["Backend Codex"]

[agents.opencode]
commands = ["opencode-beta"]
aliases = ["oc"]
```

Set `PZ_AGENTS_CONFIG` to use a different config file. Config entries with a built-in name
extend that profile; new names add custom profiles.

Let panes (and tabs) exchange data directly: any pane can send input to any other pane, read
any other pane, and join named channels — **without forking zellij**. Three artifacts in this
directory, running on stock zellij:

1. **`hub/`** — `hub.wasm`, a normal zellij plugin (the only always-on component). Holds named
   channels and in-flight `ask` waits inside the session's server.
2. **`pz/`** — companion CLI. Every invocation is short-lived and stateless (like `git`): it
   resolves the target, calls stock `zellij action` / `zellij pipe` / `zellij subscribe`, and
   exits. No daemon.
3. **`layouts/` + `tests/`** — a test session layout and the end-to-end suite.

Model: herdr's socket API (`pane.send_text`, `pane.read`, `events.subscribe`). Design rules
(from `../pane_comms.md`): communication is **on the ask only** — nothing is pushed unless
something explicitly asked for it; idle cost is zero; status is self-reported, never
output-derived (M5, deferred).

## Build

Requires the `wasm32-wasip1` Rust target (`rustup target add wasm32-wasip1`, or Arch:
`sudo pacman -S rust-wasm`).

```sh
cargo build -p hub --target wasm32-wasip1 --release   # -> hub/target/wasm32-wasip1/release/hub.wasm
cargo build -p pz                                      # -> target/debug/pz
```

The hub is pinned to `zellij-tile = "=0.44.3"` (matches zellij 0.44.x, the tested floor).
Pipes require zellij ≥ 0.40.

## Install (real use)

```sh
export CARGO_TARGET_DIR="$PWD/target"
cargo build -p hub --target wasm32-wasip1 --release
cargo build -p pz
cp target/wasm32-wasip1/release/hub.wasm ~/.local/share/zellij-wrangler/hub.wasm
cp target/debug/pz ~/.local/bin/pz
```

`pz` auto-discovers the installed hub at `~/.local/share/zellij-wrangler/hub.wasm`
(`$PZ_HUB_URL` overrides). Then seed the permissions cache once (see below), and install the
agent skill:

```sh
ln -s ~/Documents/zellij-wrangler/pane-comms/skills/zellij-wrangler ~/.agents/skills/zellij-wrangler
ln -s ~/.agents/skills/zellij-wrangler ~/.codex/skills/zellij-wrangler
ln -s ~/.agents/skills/zellij-wrangler ~/.config/opencode/skills/zellij-wrangler
```

The skill (`skills/zellij-wrangler/SKILL.md`) teaches codex/opencode/claude agents in other
panes how to discover, read, prompt, and wait on each other. It is the canonical copy —
`~/.agents/skills` and each agent's skill dir symlink back to it, so a git push updates every
installed agent.

Skill activation alone is retrieval-based (the agent sees the skill's description and decides
to read it) — usually enough, not guaranteed. The always-loaded glue makes it deterministic:

- **codex**: `~/.codex/AGENTS.md` carries a "Pane comms (zellij)" section pointing at the
  skill (AGENTS.md is loaded into every codex session at startup).
- **opencode**: `~/.config/opencode/opencode.json` has
  `"instructions": ["/home/chris/.agents/skills/zellij-wrangler/SKILL.md"]`, which loads the
  skill unconditionally at startup (config `instructions` field).

Agents already running must be restarted to pick up either change.

## Load the hub + grant permissions

The hub requests `ReadCliPipes`, `WriteToStdin`, `ReadPaneContents`, and
`ReadApplicationState` (the last is required by `pz status`; without it the hub panics and
every blocked pipe hangs). Grant all four per session by pre-seeding the permissions cache
(avoids the UI prompt; zellij reads `~/.cache/zellij/permissions.kdl` — or
`$XDG_CACHE_HOME/zellij/permissions.kdl`). Keys are the plugin's stored location — the plain
path for file URLs, not the `file://` form:

```kdl
"/abs/path/hub.wasm" {
    ReadCliPipes
    WriteToStdin
    ReadPaneContents
    ReadApplicationState
}
```

Load it (once per session) with a keybind, a layout `run_plugin`, or:

```sh
zellij action start-or-reload-plugin file:///abs/path/hub.wasm
```

`pz` also launches the hub on demand the first time a `pz ask/listen/status/send --channel`
command runs (`zellij pipe --plugin ...` auto-launches the plugin), so pre-loading is optional.

## Usage

```
pz send <target> <text...>             # write into a pane's stdin (cross-tab)
pz send --channel <name> <text...>     # broadcast to all listeners of a channel
pz ask <target> <prompt...> [--timeout N]   # prompt, block until the pane prints new output
pz wait <target> --until <pat> [--timeout N]  # block until output matches (substring or /regex/)
pz listen <channel> [--format raw|json]  # stream a channel (Ctrl-C stops)
pz status <target>                     # one-shot pane status (title/focused/exited)
pz targets [--json]                    # list panes with tab ids/names and inferred agent roles
pz agents [--json]                     # list discovered agent panes and selectors
```

Targets: `terminal_2` | `plugin_1` | `3` (bare == `terminal_3`) | `tab:3` | `tab-name:work`
(first match wins; tab names are not unique) | `agent:NAME` / `NAME` | `other:NAME` |
`active` (the single focused pane).

Agent targets are resolved client-side from the live pane command/title and must match exactly
one agent pane. `NAME` is shorthand for `agent:NAME`. If two panes run the same agent, pz
refuses to guess and lists the concrete pane ids; use the intended id for the recipient.

Session: `$ZELLIJ_SESSION_NAME` (inside zellij), else the single active `zellij ls` session,
else `--session <name>`.

Hub location: `$PZ_HUB_URL`, else auto-discovered next to the `pz` binary.

### Exit codes (contract)

| code | meaning |
|---|---|
| 0 | success |
| 1 | session resolution failure; `wait` timeout / pane closed; `listen` ended |
| 2 | bad or missing target; zellij command failed |
| 3 | `ask` timed out |
| 4 | `status` target unknown |

### Cross-tab

Pane ids are globally unique across tabs, so `send`/`wait`/`status` reach panes in any tab with
no extra work. `tab:N` / `tab-name:X` are resolved client-side via
`zellij action list-panes --json` (`tab_id`, `tab_name`, `is_focused`) before any zellij call —
zero protocol changes.

## Wire protocol (pz ⇄ hub)

Requests ride `zellij pipe --plugin hub.wasm --name <n> -- <json>`. The hub answers on the
**pipe id** (`PipeMessage.source == PipeSource::Cli(pipe_id)`, the UUID the CLI client is
registered under) — never on the human-readable name — via `cli_pipe_output` +
`unblock_cli_pipe_input`, so the invoking CLI gets the reply and exits.

```json
{"cmd":"send","channel":"demo","text":"hi"}
{"cmd":"listen","channel":"demo"}
{"cmd":"unlisten","channel":"demo"}
{"cmd":"ask","target":"terminal_2","prompt":"...","timeout_ms":60000}
{"cmd":"status","target":"terminal_2"}
{"cmd":"channels"}
```

Replies: `{"ok":true,"reply":"...","reply_type":"output"}` / `{"ok":false,"error":"..."}`
(`error == "ask_timeout"` ⇒ exit 3). Every envelope is newline-terminated (NDJSON), so
streaming consumers can read line-wise (`pz listen` does).

`listen` acknowledges with `{"ok":true,"event":"ack","reply":"subscribed",
"reply_type":"subscribed"}` — the `event` field marks it as internal (pz suppresses it).

Channel fan-out sends `{"event":"channel","channel":"demo","payload":"hi"}` on each
subscriber's pipe **without unblocking** — subscribers stay open and stream (`pz listen` wraps
this as `{"event":"pipe_output","pipe_name":"demo","output":"hi"}` in json format).

`ask` semantics: snapshot the target's scrollback, write the prompt, then poll every 300 ms for
new lines (truncation-safe suffix diff). The reply contains everything the pane printed after
the snapshot — including the echo of the prompt itself. Limitations: an app with terminal echo
disabled, or a pane busy running a command that doesn't read stdin, never produces output, so
`ask` times out; herdr's status-token model (M5) is the proper fix for that.

## Testing

```sh
tests/e2e.sh          # full E2E on a dedicated session (never touches your other sessions)
cargo test -p pz      # matcher/line-tracker unit tests (M4 L4)
```

E2E coverage: M1 baseline (write-chars, dump-screen round-trip, cross-tab), target resolution
(`tab:`, `tab-name:`, missing/empty/unknown → exit 2), channels (`listen` raw+json,
`send --channel`), `wait` (match, regex, timeout exit 1, pre-existing text never matches),
`ask` (output reply, missing pane → 2, timeout → 3), `status` (ok, unknown → 4), `targets`.

## Known limitations / open questions

- **Full-screen apps** (vim, REPLs): `send` injects keystrokes into the app — same limitation
  as herdr. Documented, not solved.
- **Stale listeners**: if a `pz listen` client dies, its pipe registration is removed by the
  server, but the hub keeps the subscriber until `unlisten` or session end. A fan-out to a dead
  pipe hits the server's broadcast fallback (harmless: every other client filters by its own
  pipe id), but subscriptions should be treated as session-scoped.
- **`active`** is ambiguous with multiple attached clients (each has a focused pane) — `pz`
  errors and lists candidates.
- **Per-session state**: channels and asks live only in the hub instance of one session.
- **Version pin**: tested against zellij 0.44.3. The plugin protocol is versioned upstream and
  old plugins keep loading, but pin zellij in CI before running the E2E suite.

## Not yet built (deferred, per pane_comms.md)

- M5 agent status + prompting (`pz status` here is pane status, not agent status; agent
  status tokens with TTLs and `zstatus`-style wrappers are M5).
- Upstream PR candidates: `pipe --pane-id`, `listen`, `subscribe --until/--timeout`,
  `--tab-name` — the thin client features this CLI already covers.
