# Pane-to-Pane Communication in Zellij ("pane comms")

Implementation spec. Verified against the source tree in this repo (do not re-derive the
architecture section; trust it, but re-check line numbers with grep before editing).

## 1. Goal

Let panes (and tabs) exchange data directly: any pane can send input to any other pane, read
any other pane (snapshot AND live stream), and join named channels ("pipes") as a receiver —
without a plugin in the middle. This is **cross-pane AND cross-tab**: a pane in tab 1 must be
able to reach a pane in tab 3. Model: herdr's socket API (`pane.send_text`, `pane.read`,
`events.subscribe`).

### 1.1 — Deliverable shape (what you're actually building)

One repo, three artifacts — **no fork of zellij, no patched zellij binary, no protocol changes**:

1. **Hub plugin** (`hub.wasm` — a normal zellij plugin; ~all the complexity lives here): named
   channels, cross-pane/cross-tab read+write, wait-for-output, agent-status tokens, optional
   status dashboard pane. Loaded like any plugin: a layout `run_plugin`, `zellij action
   start-or-reload-plugin <url>` (actions.rs:1854), or a keybind.
2. **Companion CLI** (a small standalone binary/scripts in the SAME repo — this is the "new
   version of the CLI", but it is NOT zellij; it is YOUR tool that wraps stock zellij commands):
   - `zjw send <target> <text>` → `zellij action write-chars --pane-id ...` (targets resolved
     client-side via `list-panes --json` / `list-tabs --json`, which is how `--tab-name`
     support exists without touching zellij);
   - `zjw ask <target> <prompt>` → blocking `zellij pipe --plugin hub.wasm --name ask -- ...`;
   - `zjw status <target>` → one-shot status query through the hub;
   - `zjw wait <target> --until X [--timeout N]` → bounded wait (subscribe loop or hub poll).
   This is where all CLI ergonomics live, so they never collide with upstream zellij churn.
3. **Wrapper/install scripts** (M5): `zstatus`-style hooks that wrap agent CLIs
   (claude/codex/opencode/...) so they self-report status to the hub via pipes or files.

Optional, not required to ship: submit the thin client features upstream as PRs
(`subscribe --until`, `--tab-name`, `pipe --listen`; precedent: `zellij subscribe` was merged as
PR #4814, CHANGELOG.md:155). If they land, part of the companion CLI becomes native flags; if
not, the companion CLI already covers it. Either way the deliverable is **plugin + companion CLI
+ wrappers, running on stock zellij**.

Non-goals (explicit): no fork; no changes to the client-server protocol; native OSC status
(M5's fork route) is a future option only if you choose to contribute it upstream later.

## 2. What ALREADY EXISTS — do not rebuild

| Capability | Command (run from any pane shell) | Code |
|---|---|---|
| Send text/bytes/keys to a SPECIFIC pane (cross-tab too) | `zellij action write` / `write-chars` / `paste` / `send-keys` with `--pane-id terminal_2` | `zellij-utils/src/input/actions.rs:709,733` → `Action::WriteToPaneId`/`WriteCharsToPaneId` (`zellij-utils/src/data.rs:3496`), routed `zellij-server/src/route.rs:296,308` → `ScreenInstruction::WriteToPaneId` (`zellij-server/src/screen.rs:11161`) |
| Snapshot-read a pane | `zellij action dump-screen --pane-id X [--full] [--ansi]` | `route.rs:473` |
| LIVE-stream a pane's output (diffs, raw or JSON, scrollback included) | `zellij subscribe --pane-id terminal_2 [--format raw|json] [--scrollback N] [--ansi]` | `ClientToServerMsg::SubscribeToPaneRenders` (`zellij-utils/src/ipc.rs:212`), client `zellij-client/src/cli_client.rs:261`, server `screen.rs:7351` + diff loop `screen.rs:7481` |
| Discover panes/tabs | `zellij action list-panes --json`, `list-tabs`, `list-clients` | `zellij-utils/src/cli.rs:1411` |
| Bidirectional channel WITH a plugin (stdin→plugin, plugin→stdout) | `zellij pipe --name X --plugin URL` | `cli_client.rs:83`, plugin `pipe()` export `zellij-tile/src/lib.rs:43` |
| Broadcast input to every pane in a tab (optionally a SPECIFIC tab) | `zellij action toggle-active-sync-tab [--tab-id N]` | `cli.rs:873` |
| Pane-creation returns the new pane id | `zellij action new-pane` | `cli.rs:882` |

Every pane's shell gets `ZELLIJ_SESSION_NAME` (`zellij-utils/src/envs.rs:19`) and
`ZELLIJ_PANE_ID`, so any process inside a pane can reach the right session socket.

**Cross-tab is already free for pane-addressed commands.** Pane ids are globally unique across ALL
tabs, and the screen resolves a pane by iterating every tab (`screen.rs:7373`,
`screen.rs:11163`) — so `write-chars --pane-id terminal_7`, `dump-screen --pane-id`, and
`subscribe --pane-id` all work when the pane lives in a different tab. Pipes are session-global
channels, so M3 listeners are cross-tab by design. What is NOT available today: addressing "the
active pane of tab N" without knowing its pane id, and subscribing to a whole tab at once — see
"Cross-tab addressing" in the deliverables.

**Conclusion: the write-path and read-path to/from arbitrary panes already exist. This spec adds
the missing pieces: (M2) pipe payloads delivered to a pane's stdin, (M3) a shell-side pipe
receiver, (M4) wait-for-output.** M5 (agent status + prompting) is fully designed but deferred
— see its section below for the build plan.

## 3. Architecture primer (read before touching anything)

- One server process per session; clients (TUI, web, and the CLI clients used by
  `zellij action`/`zellij pipe`/`zellij subscribe`) connect over a Unix socket in
  `ZELLIJ_SOCK_DIR/<session>`.
- **The wire protocol is PROTOBUF (prost), not JSON.** Domain enums (`ClientToServerMsg`,
  `ServerToClientMsg`, `Action`) are converted to generated `Proto*` types in
  `zellij-utils/src/ipc/protobuf_conversion.rs` + `enum_conversions.rs`, then
  `encode_to_vec()` (`zellij-utils/src/ipc.rs:387`). Generated code lives in
  `zellij-utils/assets/prost*` (CHECKED IN) and is produced from `zellij-utils/src/<contract>/*.proto`
  via `cargo x proto` (requires `protoc`; see CONTRIBUTING.md:97). `cargo x build` also
  auto-regenerates when a .proto is newer than the generated file (xtask/src/build.rs:248).
- Message flow, shell → server: `zellij` binary (`src/commands.rs`) → cli client
  (`zellij-client/src/cli_client.rs`) → `ClientToServerMsg::Action { action, terminal_id, client_id, is_cli_client }`
  → server loop (`zellij-server/src/lib.rs`) → `route_action` (`zellij-server/src/route.rs:218`)
  → `ScreenInstruction` / `PluginInstruction` → the screen event loop (`screen.rs`) → `Tab` → pane PTY.
- **The screen is a single-threaded event loop**. Never block inside it. If an action must return
  data, use a crossbeam response channel + `recv_timeout` (pattern: `Action::ListPanes` at
  `route.rs:2904`), or fire the per-action completion.
- **Two PaneId types.** User-facing: `zellij_utils::data::PaneId` (`Terminal(u32)` /
  `Plugin(u32)`; `FromStr` accepts `terminal_1`, `plugin_1`, or bare `1`). Server-internal:
  `zellij_server::panes::PaneId`. Convert with `.into()`; server APIs take the server type.
- Server state lives in `session_state` (see the pipe registry `get_pipe(&name) -> Option<ClientId>`
  used at `lib.rs:1266`).

## 4. Deliverables

### Design principle: on-demand only (pull, never broadcast)

All communication in this spec is **on the ask** — nothing is pushed to a pane, listener, or
subscriber unless it explicitly asked for it. This matches herdr: its `agent.prompt` literally
types text into the target pane, its `agent.wait` waits on the target's SELF-REPORTED status
(working/blocked/idle/done — small key/value tokens with TTLs, `herdr/src/metadata_tokens.rs`),
and reading output is a separate on-demand `pane.read`. Follow these rules in every milestone:

1. **No server-initiated broadcasts.** Every server→client message must be a response to an
   explicit client request: a one-shot action, a subscription, or a bounded wait. Do NOT copy the
   existing `CliPipeOutput` "send to all clients when the pipe is unassociated" fallback
   (lib.rs:1311) — it is legacy behavior, not a pattern to extend.
2. **Idle cost must be zero.** The pane-render pipeline already proves this: subscriber delivery
   and ANSI collection are gated on `!pane_render_subscribers.is_empty()` (screen.rs:4075-4078,
   4169) and only CHANGED viewport diffs are sent (screen.rs:7499-7503). New subscriptions must be
   equally gated and diff-based — never ship full buffers on a timer.
3. **Subscriptions are opt-in, scoped, and pruned.** A subscription names exactly what it wants
   (specific pane ids / pipe names), the client ends it by closing or via `--until`, and the
   server drops dead subscribers on send failure (screen.rs:7488-7549 pattern).
4. **Prefer one-shot + bounded wait over streams.** M4's `--until`/`--timeout` connects, matches,
   exits — the connection closes when the ask is answered. Long-lived tools (`subscribe`,
   `listen`) exist but are opened and closed by hand, like `tail -f`.
5. **"Check in on tab 2" is three asks**: (1) write — M2 `pipe --tab-id 2 --name prompt -- "... "`,
   (2) wait — M4 `subscribe --until PATTERN --timeout` or an M5 status-wait, (3) read —
   `dump-screen --pane-id` on demand. No transcript is shipped unless step 3 happens.
6. **Status is self-reported, never output-derived.** If M5 (agent status) is ever built, panes
   report working/blocked/idle/done as small metadata (herdr's model: tokens with TTLs reported by
   the process inside the pane), and waiters wait on that enum. The server must never parse or
   buffer pane output to infer state.

### Companion CLI (`zjw`) + hub protocol (the PRIMARY deliverable)

The zellij-native M2-M4 sections below are the OPTIONAL upstream variants. The DEFAULT
implementation is plugin + companion CLI, per §1.1. Milestone → artifact mapping:

**Runtime model (who runs when — READ THIS before designing anything):** the HUB is the only
always-on component. It loads with the session (layout `run_plugin`, keybind, or
`zellij action start-or-reload-plugin <url>`) and lives inside the zellij server for the whole
session, holding channels, waiters, and status tokens. `zjw` is NOT launched by zellij and does
NOT run in the background: every `zjw ...` invocation is a short-lived, stateless process (like
`zellij action` / `git`) that resolves the target, calls stock zellij commands
(`zellij action write-chars --pane-id ...`, `zellij pipe --plugin hub.wasm --name ask -- ...`,
`zellij subscribe ...`), and exits. `zjw ask`/`zjw wait` stay alive only as long as the blocking
pipe/subscribe they opened; `zjw listen` streams until Ctrl-C — both still on-demand. Agents call
`zjw` like any other tool; keybinds can invoke it via `Action::RunCommand`. **Do NOT build
`zjw` as a daemon** — the hub already holds the state; a daemon would duplicate it. (Analogy:
hub ≈ herdr's server core running inside zellij, `zjw` ≈ herdr's CLI, wrappers ≈ herdr's
integrations.)

| Milestone | Primary artifact (default) | Optional native variant |
|---|---|---|
| M1 baseline | harness helpers + tests | — |
| M2 pipe --pane-id | `zjw send` wraps `zellij action write-chars --pane-id`; hub handles the payload | `zellij pipe --pane-id` (upstream PR) |
| M3 listen | hub emulates listeners (writes payloads into subscriber panes via `write_chars_to_pane_id`); `zjw listen` opens the channel | `zellij listen` (upstream PR) |
| M4 wait | `zjw wait` = `zellij subscribe --format json` loop with a client-side matcher | `subscribe --until` (upstream PR) |
| M5 status | hub token registry + `zstatus` wrappers; `zjw status`/`zjw ask` | OSC status + `zellij agent` (fork/upstream) |

**Build order** (each step shippable): (1) integration-harness helpers + M1 tests, (2) hub
skeleton + pipe plumbing + `zjw send`/`zjw ask` (proves the channel end-to-end), (3) M4 matcher +
`zjw wait` (pure client-side), (4) M3 listeners in the hub, (5) M5 tokens + wrappers.

#### Repo layout

```
pane-comms/
├── hub/            # Rust wasm plugin (cdylib, wasm32-wasip1)
│   ├── src/lib.rs  # ZellijPlugin impl: pipe(), event(), render(), set_timeout tick
│   └── Cargo.toml  # deps: zellij-tile (path or crates.io), serde
├── zjw/             # companion CLI (any language; Rust keeps it one binary)
│   └── src/main.rs # subcommands: send, ask, status, wait, listen, targets
├── wrappers/       # zstatus + agent wrapper scripts (M5)
└── README.md
```

#### Hub wire protocol (zjw ⇄ hub over named pipes)

All payloads are JSON strings (pipes carry `Option<String>` only). Reserved pipe names:
`ask` (blocking request/response), `status` (one-shot), `report` (agents self-report), plus
user channel names. Envelopes:

```json
{"cmd":"send","target":"terminal_2","text":"hi"}              // zjw → hub
{"cmd":"wait","target":"terminal_2","until":"done","timeout_ms":60000}
{"cmd":"status","target":"tab-name:work"}
// hub → zjw (via cli_pipe_output(pipe_id, ...), then unblock_cli_pipe_input(pipe_id)):
{"ok":true,"status":"working","reply":"..."}
{"ok":false,"error":"pane terminal_2 not found"}
```

Target syntax (resolved in `zjw`, which sends a CONCRETE pane id to the hub): `terminal_2` |
`plugin_1` | `tab:3` | `tab-name:work` | `active`. `zjw` resolves `tab:*` via
`list-panes --json` / `list-tabs --json` before calling the hub.

Hub state: `HashMap<pane_id, AgentStatus>` + TTLs (M5), expired on a `set_timeout` tick.
Status is ephemeral by nature — the hub persists nothing across sessions; document that.

Hub permissions (config `allowed` KDL block; plugin calls `request_permission` at startup,
shim.rs:92): `ReadPaneContents`, `WriteToStdin`, `ReadCliPipes`, `MessageAndLaunchOtherPlugins`;
`RunCommands` only if the hub shells out. TEST the permission-denied path.

#### `zjw` CLI contract (exit codes are part of the contract — document them)

- `zjw send <target> <text>` — exit 0 delivered; 2 bad/missing target; 1 session not found.
- `zjw ask <target> <prompt> [--timeout N]` — blocking; exit 0 + reply on stdout; 3 timeout.
- `zjw status <target>` — JSON status on stdout; exit 4 unknown/expired.
- `zjw wait <target> --until X [--timeout N]` — exit 0 on match; 1 timeout.
- `zjw listen <channel> [--format raw|json]` — long-lived; Ctrl-C stops it.
- Session discovery mirrors `get_active_session()` (src/commands.rs:458): `ZELLIJ_SESSION_NAME`
  env, else `zellij ls`, else error listing active sessions.

#### Build & dev environment

- Hub target: `wasm32-wasip1` (the repo's own plugins build to `target/wasm32-wasip1/release`,
  xtask/src/build.rs:325). Build: `cargo build --target wasm32-wasip1 --release` in `hub/`.
- Load the hub: `zellij action start-or-reload-plugin file:///path/to/hub.wasm`
  (actions.rs:1854) or a layout `run_plugin`.
- Version floor: pipes (zellij ≥ 0.40), `write_chars_to_pane_id`, `get_pane_scrollback`,
  `subscribe` — verify against the running zellij (the CHANGELOG citations above) and pin that
  version in CI; run the §8 E2E suite against it.
- Debugging: `zellij --debug` server log; hub `log()`; since pipes carry only strings, errors
  are JSON-encoded into replies (see `ok:false` envelope above).

#### Risks & open questions (decide before coding)

- `send` into a pane running a full-screen app (vim/REPL) injects keystrokes into that app —
  define and document the behavior (herdr has the same limitation).
- Multiple sessions: hub state is per-session; `zjw` must target the right one.
- First-run UX when the hub requests permissions (approve prompt in the UI).
- Plugin API drift: pin the zellij version in CI (point above) so the E2E suite never runs
  against a moving target.

### Cross-tab addressing (design for --tab-id / --tab-name, applies to M2 and M4)

Recommended approach: **client-side resolution, no server/protocol changes**.

- `zellij action list-panes --json` returns `PaneListEntry` with `tab_id`, `tab_position`,
  `tab_name` (`zellij-utils/src/data.rs:2369-2378`) and, with `--state`, the focused pane
  (`PaneInfo.is_focused`, data.rs:2322). Resolution recipe for `--tab-id N`:
  1. run `zellij action list-panes --json --state` (via the same `start_cli_client`/CLI machinery, or a small helper);
  2. parse JSON, filter entries where `tab_id == N`;
  3. if targeting "the active pane of tab N", pick the entry with `is_focused: true`;
  4. if the tab has no panes (or doesn't exist), error out with exit 2 (mirror the existing
     "Pane {} not found" LogError convention).
- For `--tab-name "XYZ"` instead of `--tab-id N`: first run `zellij action list-tabs --json`
  and map `name` → `position` (`TabInfo.name` / `TabInfo.position`, data.rs:2256-2260), then
  proceed exactly as for `--tab-id`. `--tab-name` conflicts with `--tab-id` in clap.
- Then hand the resolved `PaneId`(s) to the existing pane-addressed paths
  (`WriteCharsToPaneId` for M2, `SubscribeToPaneRenders` for M4). **Zero proto/server work.**

**Traps:**
- **Resolution race**: the pane set / active pane of a tab can change between `list-panes` and
  the follow-up action. Acceptable for v1 — the follow-up fails loudly (exit 2 / LogError) if the
  pane is gone; document it. If a server-side, race-free variant is wanted later, add a
  `ScreenInstruction::GetFocusedPaneInfoForTab { tab_id, response_channel }` — the existing
  `GetFocusedPaneInfo` (screen.rs:8717) is per-CLIENT active tab only, so it needs a tab parameter.
- "Active pane of a tab" is ambiguous when the tab is empty or has only plugin panes — define the
  behavior (error) and test it.
- `--tab-id` and `--tab-name` must conflict with `--pane-id` and with each other in clap
  (you address a tab OR a specific pane, never both).
- **Tab names are NOT unique** — two tabs can share a name (e.g. two "main" tabs). Define the
  behavior for `--tab-name` (first match wins, or error on ambiguity) and test it. Unknown name
  → exit 2 with the list of valid tab names on stderr.

### M1 — Baseline verification (no production code)

Add tests proving the existing primitives work end-to-end, so later regressions are caught (full per-milestone test plan in §8):
- Integration test (harness: `zellij-integration-tests/`; look at existing tests for
  `zellij action write-chars` for the spawn pattern): two panes; write-chars from pane A to pane
  B by id; dump-screen B; `subscribe --pane-id B --format json` receives `pane_update` events.
- **Cross-tab variants of the same test**: move pane B to a different tab (`zellij action break-pane`
  or start a new tab and split) and repeat — write-chars, dump-screen, and subscribe must all still
  work because pane ids are global. Also verify `toggle-active-sync-tab --tab-id N` broadcasts only
  to tab N.

### M2 — `zellij pipe --pane-id <PANE_ID>`: deliver a pipe payload to a terminal pane's stdin

> PRIMARY IMPLEMENTATION: hub + `zjw send` (see "Companion CLI + hub protocol" above). This
> section describes the OPTIONAL native variant — the `zellij pipe --pane-id` flag you would
> submit upstream as a PR. Implement it only if you decide to contribute upstream.

Semantics: payload is delivered to the target pane's stdin (bracketed paste), the CLI call blocks
until delivered, exits 0; unknown/malformed pane exits 2 with an error on stderr. `--pane-id`
conflicts with `--plugin` (a payload goes to a pane OR a plugin, not both).

Ordered steps:

1. **CLI**: `zellij-utils/src/cli.rs` — add `#[clap(short, long, value_parser, conflicts_with("plugin"), conflicts_with("tab_id"))] pane_id: Option<String>`
   and `#[clap(short, long, value_parser, conflicts_with("plugin"), conflicts_with("pane_id"))] tab_id: Option<usize>`
   to BOTH `CliCommand::Pipe` (~line 666) and `CliAction::Pipe` (~line 1353, the `zellij action pipe` form).
   `--tab-id` means "the active pane of that tab" and is resolved per the Cross-tab addressing
   section (client-side via `list-panes --json --state`); pass the resolved pane down the same path
   as `--pane-id`. If resolution fails (tab empty/missing), exit 2 with an error on stderr.
2. **Domain**: `zellij-utils/src/input/actions.rs` — add `pane: Option<String>` to `Action::CliPipe`
   (~line 517); in `actions_from_cli` (~line 1941) copy the pane-id parsing + error string from the
   `WriteChars` arm (~lines 733-751: "Malformed pane id: expecting either a bare integer ...").
   `--tab-id` is resolved to a pane id BEFORE this conversion (in the cli client / commands.rs
   helper), so `Action::CliPipe` only ever carries an explicit pane id — keep tab resolution out of
   the domain enum to avoid proto churn.
3. **Proto**: `zellij-utils/src/client_server_contract/action.proto` — add an optional string field
   to the `CliPipe` message. Run `cargo x proto` (protoc required). Do NOT hand-edit generated files.
4. **Conversion**: `zellij-utils/src/ipc/protobuf_conversion.rs` — update BOTH directions of the
   Action↔ActionType match (ActionType→Action ~line 2782, Action→ActionType ~line 1881). Compile
   errors will force exhaustive coverage once the proto is regenerated.
5. **Server routing**: `zellij-server/src/route.rs`, `Action::CliPipe` arm (~line 1615). If
   `pane.is_some()`:
   - parse the PaneId (reuse `PaneId::from_str`);
   - send `ScreenInstruction::Paste { bytes: payload.into_bytes(), pane_id: Some(parsed), client_id: cli_client_id.unwrap_or(client_id), completion: Some(NotificationEnd::new(completion_tx)) }`;
   - do NOT `drop(completion_tx)` (that is plugin-path-only), do NOT send `PluginInstruction::CliPipe`, skip the "Message must have a name" check;
   - if the payload is `None` (stdin-less invocation), treat as empty string.
6. **Client**: `zellij-client/src/cli_client.rs` `pipe_client` (~line 83) — thread the pane
   through `create_msg`; mark the loop `pane_targeted` and add a match arm:
   `ServerToClientMsg::UnblockInputThread => { if pane_targeted { break; } }`.
7. **Tests**: `zellij-server/src/unit/screen_tests.rs` — mirror `send_cli_dump_screen_action`
   (~line 2823): `route_action` a pane-targeted CliPipe, assert the emitted ScreenInstruction and
   that completion is wired; `actions.rs` CLI parse tests (mirror ~line 3454); protobuf round-trip.
   Full test plan in §8.
8. **CHANGELOG.md** entry + clap doc comments (they ARE the user docs).

**Traps (M2):**
- **`ScreenInstruction::WriteToPaneId` IGNORES its completion** (screen.rs:11161 passes `None` to
  `tab.write_to_pane_id`). Routing the pipe there would make the cli client hang forever. Use
  `ScreenInstruction::Paste`: its handler (screen.rs:11172) forwards completion to
  `tab.paste_to_pane_id` (tab/mod.rs:4356), which fires it. If you insist on WriteToPaneId, fix the
  handler to pass the completion through and add a test.
- **`pipe_client` ignores `UnblockInputThread`** (`_ => {}` at cli_client.rs:206). Without the new
  arm the process hangs after delivery. Only break when pane-targeted — the plugin path never sends it.
- **`process::exit(0)` on `UnblockCliPipeInput` when stdin is not piped** (cli_client.rs:168) — with a
  multi-action CLI (`actions_from_cli` returns Vec<Action>; `start_cli_client` loops) an early exit
  skips remaining actions. Prefer `break`/return over `process::exit` in your new code.
- Completion goes back to `cli_client_id` — reuse `cli_client_id.unwrap_or(client_id)` exactly like
  the `ListClients` arm (route.rs:1719), or the response lands on the wrong client.

### M3 — `zellij listen --name X`: join a named pipe from a shell (receive side)

> PRIMARY IMPLEMENTATION: hub-emulated listeners + `zjw listen` (see "Companion CLI + hub
> protocol"). This section is the OPTIONAL native variant (upstream PR candidate).

A long-lived CLI client that prints everything delivered to pipe X to stdout (raw or JSON). This is
the missing receive half of "panes talk over a named channel". Model it on the subscribe client.
Pipes are session-global (a pipe name has no tab scope), so a listener in tab 1 receives traffic
from a sender in tab 3 with no extra work — no tab handling needed here.

1. **CLI**: `zellij-utils/src/cli.rs` — new top-level `CliCommand::Listen(ListenCli)` with
   `name` (repeatable), `format: raw|json`, plus the standard `--session` handling used by
   subscribe. Write a usage doc like the pipe one (~line 650).
2. **Proto**: `zellij-utils/src/client_server_contract/client_to_server.proto` — new message
   `ListenToPipe { repeated string pipe_names = 1; }` added to the `ClientToServerMsg` oneof.
   `cargo x proto`. **No new server→client message is needed — reuse `ServerToClientMsg::CliPipeOutput`.**
3. **Domain+conversion**: `zellij-utils/src/ipc.rs` (new `ClientToServerMsg::ListenToPipe { pipe_names }`),
   `protobuf_conversion.rs` + `enum_conversions.rs` (both directions).
4. **Server state**: session state gains `pipe_listeners: HashMap<String, Vec<ClientId>>`
   (look at how the pipe→client registry is stored, `get_pipe` at lib.rs:1266).
   New `ServerInstruction::ListenToPipe { client_id, pipe_names }`; prune on client exit.
5. **Server fan-out**: in the `ServerInstruction::CliPipeOutput` handler (lib.rs:1296) ALSO send
   the message to every listener registered for `pipe_name` (in addition to the associated client /
   all-clients broadcast). Drop listeners whose `send_to_client!` fails (mirror the dead-subscriber
   pruning in screen.rs:7488-7549 — collect sends first, then remove dead ids).
6. **Client**: `zellij-client/src/cli_client.rs` — `start_listen_client`: connect, send
   `ListenToPipe`, loop on `ServerToClientMsg::CliPipeOutput { pipe_name, output }`
   → raw: write the line; json: `{"event":"pipe_output","pipe_name":...,"output":...}`.
   Handle `Exit`/`LogError` like `start_subscribe_client` (cli_client.rs:302-362); send
   `ClientExited` at the end. Filter by your own pipe_names (see trap below).
7. **Dispatch**: `src/commands.rs` — add the `CliCommand::Listen` arm copying
   `subscribe_to_session` (~line 453) including the `get_active_session()` resolution logic.
8. Tests (§8) + CHANGELOG + docs.

**Traps (M3):**
- When a pipe has no associated client, `CliPipeOutput` is already broadcast to ALL clients
  (lib.rs:1311). A listener will therefore see traffic for pipes it didn't subscribe to — filter by
  `pipe_name` client-side. This is also why the server-side listener registry should be checked
  FIRST so you don't double-send to listeners via the fallback.
- A listener is just another client: it receives `Log`/`LogError`/`Exit` too. Don't let unknown
  variants crash the loop — the subscribe client's `_ => {}` arm is the model.
- If the session has no pipe activity, the listener blocks forever — that is the intended semantics
  (like `tail -f`); document it.

### M4 — `zellij subscribe --until <pattern> [--timeout <ms>]` (wait for output)

> PRIMARY IMPLEMENTATION: `zjw wait` (see "Companion CLI + hub protocol"). This section is the
> OPTIONAL native variant (upstream PR candidate); the matcher logic is identical either way.

Pure client-side, zero protocol/server changes. Highest value-per-effort; do this first if you want
a quick win.
- In `start_subscribe_client` (cli_client.rs:261): maintain a rolling buffer of received lines;
  on each `PaneRenderUpdate`, test the buffer against `--until` (substring, or regex if prefixed
  `/`); on match exit 0. On `--timeout` exit 1 (print a final JSON event / stderr note). Document
  exit codes in clap help.
- Add `--tab-id N` (conflicts with `--pane-id`): resolve to all pane ids of tab N via
  `list-panes --json` (Cross-tab addressing section) and pass the full list to
  `SubscribeToPaneRenders` — this gives "tail the whole tab" and "wait until ANY pane in tab N
  prints X". Note the resolution race trap (panes created/closed mid-subscription are not picked
  up; a re-subscribe or a future server-side tab subscription would fix it).
- **Trap**: `PaneRenderUpdate` carries the FULL current viewport every time, not deltas — "new"
  lines must be derived by diffing against the previous update's viewport (the server does this
  internally at screen.rs:7499; the client must re-derive it). A crude but correct-for-v1 approach:
  track `seen: HashSet<String>` of viewport lines; treat unseen lines as new. The initial
  `scrollback` payload lines should also be considered (they arrive once, in the first update).
- Tests: unit-test the matcher as a pure function (extract it); manual E2E below; full plan in §8.

### M5 — Agent status + prompting (design; the bigger project — do not start without sign-off)

Goal: per-pane agent state and prompt/wait primitives, so one agent can "check in" on another
("look at tab 2") by asking and blocking until the answer, WITHOUT streaming transcripts.
herdr's model, verified in its source: `AgentStatus` = Idle / Working / Blocked / Done
(`herdr/src/api/schema/common.rs:151-155`); status is SELF-REPORTED via tiny key/value tokens
with TTLs (`herdr/src/metadata_tokens.rs`); `agent.prompt` types the text into the target pane
and `agent.wait` waits on status transitions, anchored to a monotonic state-change sequence.
Design principle rule 6 governs: status is self-reported, NEVER output-derived.

#### Status model

- Per-pane `agent_status: Option<AgentStatus>` + monotonic `state_change_seq: u64` (bumped on
  every change). Waiters snapshot the seq and only accept events AFTER it — this avoids the
  "transition already happened before I subscribed" race (herdr's event-hub sequence pattern).
- TTL expiry: a status is stale after N seconds without refresh → report `unknown` (agent died
  or lost its wrapper), never a stale `working`. Expiry tick lives server-side (fork) or in the
  hub plugin (plugin route).

#### Report transport (choose one; least → most invasive)

1. **Pipes (no fork)** — a tiny `zstatus` wrapper inside each pane runs
   `zellij pipe --plugin status-hub.wasm --name status -- '{"status":"working"}'`; the hub keeps
   the TTL'd tokens. Works on unmodified zellij; needs the hub running + `ReadCliPipes`
   permission. Agent CLIs are wrapped, not modified (herdr's integration model).
2. **Files (no fork)** — wrappers write `<pane_id>.status` files into a session dir; the hub
   subscribes to the plugin filesystem events (`Event::FileSystemCreate/Update/Delete`,
   `zellij-server/src/plugins/watch_filesystem.rs:108`) and re-scans via `scan_host_folder`
   (`zellij_exports.rs:3954`). Verify which Permission covers host-folder access before
   relying on this (permission list: data.rs:1075-1093).
3. **OSC (fork — the real integration)** — zellij ALREADY parses one OSC from pane output: OSC 7
   (cwd). The precedent chain to mirror: `vte::Parser` in
   `zellij-server/src/panes/terminal_pane.rs:140` → `grid.rs:412 parse_osc7_path` →
   `PtyInstruction::NotifyCwdFromOsc7` (`pty.rs:159`) → `notify_cwd_from_osc7`
   (`pty.rs:2199`) → `Event::CwdChanged` (data.rs:1024). Add a zellij-private OSC (e.g.
   `OSC 1337;ZELLIJ_STATUS=working`) parsed in the same path, surfaced as
   `Event::PaneStatusChanged`. Then ANY process that can print an escape sequence reports status
   — no wrapper, no plugin, no pipe.

#### Wait / prompt semantics (mirror herdr)

- `zellij agent prompt <tab|pane> <text> [--wait [--until idle|working|blocked|done] [--timeout <ms>]]`:
  deliver text to the target pane's stdin (reuse the M2 pane-targeted write path), optionally wait.
- Wait loop: snapshot `state_change_seq`, subscribe to status updates, match `--until`, exit 0;
  timeout → exit 1. Model the client loop on `start_subscribe_client` (cli_client.rs:261) — it
  connects, waits, closes when answered (on-the-ask).
- **Prompt-stall detection** (herdr, cli/spec.rs:362): if the target was NOT working when prompted
  and no state change is observed within ~5s, return `prompt_stalled` instead of waiting forever;
  reject the prompt outright if the target is already `blocked`.

#### Fork-route milestones (rough order; separate design doc before coding)

1. **Status storage**: `agent_status`/`state_change_seq` on the pane struct
   (`zellij-server/src/panes/terminal_pane.rs`) + session state.
2. **OSC transport**: parse fn beside `grid.rs:412`; new `PtyInstruction` + server event
   mirroring `NotifyCwdFromOsc7` (pty.rs:159, 2199); TTL expiry tick.
3. **Protocol**: new `ClientToServerMsg` (`agent.wait`, `agent.prompt`) + `ServerToClientMsg`
   (`PaneStatusUpdate`) — same proto + protobuf_conversion.rs process as M2/M3.
4. **CLI**: `zellij agent` subcommands in cli.rs; dispatch in src/commands.rs; wait loop in
   cli_client.rs.
5. **UI**: status markers in pane frames / status bar (native) OR a status-hub plugin pane
   (plugin route).
6. **Integrations**: `zstatus`-style wrapper installation for real agent CLIs (herdr has
   `integration.install`; mirror as `zellij agent install <kind>`).

#### Effort & decision points

- Plumbing (OSC transport + status storage + wait loop) ≈ 1-3 weeks — it mirrors existing OSC7
  machinery. The hard, time-consuming parts are NOT plumbing: (a) wrapper/integration coverage
  for real agent CLIs (claude/codex/opencode/...), (b) semantics decisions — when is an agent
  "blocked"? what does "idle" mean mid-run? (c) the UI/product call. Budget accordingly.
- Plugin-first alternative: build the status-hub + `zstatus` wrapper now (days, no fork); keep
  the OSC path for later if/when the fork route is taken. Both satisfy the self-report rule.

### Alternative: build it all as a plugin (no fork needed)

Everything in M1-M4 (and most of M5) can be implemented as a "hub" WASM plugin on an UNMODIFIED
zellij, because the plugin API already exposes the primitives (all verified in this checkout):

| Need | Plugin API | Code |
|---|---|---|
| Write to any pane (cross-tab) | `write_chars_to_pane_id(chars, PaneId)` — permission `WriteToStdin` | `zellij-tile/src/shim.rs:1930` |
| Read any pane | `get_pane_scrollback(pane_id, full, max_lines)` — `ReadPaneContents` | `shim.rs:1890`, zellij_exports.rs:5618 |
| Named channels | `pipe()` export + `cli_pipe_output`/block/unblock — `ReadCliPipes` | `zellij-tile/src/lib.rs:43`, `shim.rs:1636-1658` |
| Synchronous wait-for-output | `set_timeout` poll + pipe reply (strider filepicker pattern) | `shim.rs:811`, default-plugins/strider/src/main.rs:155 |
| Cross-tab by tab NAME | `TabUpdate`/PaneUpdate events, `get_tab_info`, `get_focused_pane` | `shim.rs:308`, `shim.rs:2819` |
| Agent status (M5) | self-report via `zellij pipe --name status --plugin hub`, hub keeps TTL'd tokens (herdr model) | — |

The canonical shell-side flow with a hub: `zellij pipe --plugin hub.wasm --name <chan> -- <payload>`
(possibly with the target pane in `--plugin_configuration`); the hub receives it in `pipe()`,
acts (write/read/wait), and answers on the same pipe with `cli_pipe_output` +
`unblock_cli_pipe_input` — the invoking shell gets the reply on stdout and the CLI exits. That is
a blocking, on-the-ask channel (honors the design principle above).

**Why you might prefer the fork/upstream route (M2-M4 as specced) instead:**
- Native CLI flags (`--pane-id`/`--tab-name` on `zellij pipe`, a real `zellij listen` printing to
  a shell's stdout, `subscribe --until`) need the client changes in this doc — plugins can't add
  CLI surface; shell aliases can only approximate.
- Plugins can only inject keystrokes or reply down an open pipe — no out-of-band writes to a
  shell's stdout; pipes are text-only (`Option<String>` payloads).
- The hub occupies a pane (tiled/floating/tiny) and requires the user to grant permissions
  (`WriteToStdin`, `ReadPaneContents`, `ReadCliPipes`, `MessageAndLaunchOtherPlugins`, optionally
  `RunCommands`) via the permission request flow / config `allowed` list.
- API availability is version-gated: pipes exist since 0.40, `subscribe` and
  `write_chars_to_pane_id` are recent.

Recommended shape if you go plugin-first: hub plugin for all functionality (shipable as a .wasm,
no fork), plus the M2-M4 client work as the upstream polish on top — the two routes are
complementary, not either/or.

#### Maintenance risk: why plugin-first beats forking (decision rationale)

Everything below the CLI surface is plugin-buildable, and the plugin API is versioned and
compat-maintained upstream (old plugins keep working across releases) — so a plugin's breakage
risk is LOW. A fork is the opposite: M2-M4 as specced touch the CHURNIEST files in the repo —
`zellij-utils/src/cli.rs` (upstream adds flags constantly), `input/actions.rs` +
`protobuf_conversion.rs` + `action.proto` (the Action enum churns in nearly every release),
`cli_client.rs`, `route.rs`. Every upstream refactor of those lands in your diff.

Preferred path (zero fork):
1. Your repo = hub plugin (`.wasm`) + a small companion CLI/scripts (sugar over existing
   `zellij pipe` / `zellij action`) + wrapper install scripts. No zellij changes.
2. Submit the thin client features upstream as PRs (`subscribe --until/--timeout`,
   `--tab-name`, `pipe --listen`) — each is 2-3 files; precedent: `zellij subscribe` itself
   was added upstream via PR #4814 (CHANGELOG.md:155).
3. If a PR is rejected/slow: the remaining patch is 2-3 files (`cli.rs` + `cli_client.rs`),
   re-appliable across upstream versions in minutes — a patch, not a fork. Scope it to
   CLI-only flags resolved client-side so it never touches the protobuf or the Action enum.

IF a real fork is unavoidable (e.g. native OSC status from M5), keep it additive-only to make
upstream merges cheap: new enum variants only; new OPTIONAL proto fields with high field
numbers (never reuse/reorder/rename existing ones); new functions and new match arms appended,
never edits inside existing loops; `#[cfg(feature = "pane-comms")]` gates so upstream hunks
resolve as separate diffs; rebase upstream main weekly; keep the diff under ~15 files.

## 5. Cross-cutting pain points (read before starting)

1. **Protobuf regeneration is the #1 trip-up.** Steps: edit the .proto → `cargo x proto` (needs
   `protoc` on PATH) → generated files under `zellij-utils/assets/prost*` change → update the
   hand-written conversions in `zellij-utils/src/ipc/protobuf_conversion.rs` and
   `enum_conversions.rs`. If `cargo x proto` appears to do nothing, the generated files are newer
   than the .proto (force-skip) — the xtask only regenerates when the .proto is newer
   (xtask/src/build.rs:281-296); verify it actually rewrote files by checking git diff. NEVER
   hand-edit `zellij-utils/assets/prost*`.
2. **Always prefer extending an existing proto message with optional fields over adding a new
   message** — adding a field is one .proto edit + two match arms; a new message touches the oneof,
   the conversion glue, and every match site.
3. **Completion semantics (`NotificationEnd`)** — the per-action completion fires
   `ServerToClientMsg::UnblockInputThread` to the issuing client (this is what
   `individual_messages_client` at cli_client.rs:233 waits for). The plugin pipe path deliberately
   drops it (route.rs:1629). Pane delivery MUST fire it or the caller hangs — hence the `Paste`
   route in M2.
4. **Never block the screen loop.** Synchronous data returns use crossbeam channels +
   `recv_timeout` (route.rs:2904 pattern). Anything keyed by client must be pruned when
   `send_to_client` fails (dead-subscriber pattern, screen.rs:7488-7549), or the server leaks.
5. **Two PaneId types** — convert with `.into()`; the server's `has_pane_with_pid` (screen.rs:11164)
   takes the SERVER type. Pane ids are globally unique across tabs, so no tab disambiguation needed.
6. **New CLI commands dispatch in `src/commands.rs`** (root binary crate), NOT in zellij-client —
   top-level commands (`subscribe`, and your new `listen`) resolve the session there via
   `get_active_session()`, then call a `start_*_client` in `zellij-client/src/cli_client.rs` with
   `get_os_input(get_cli_client_os_input)` (no TUI). `zellij action ...` is a different path:
   `CliAction` → `actions_from_cli` → `start_cli_client`.
7. **New `ServerToClientMsg` variants must be added to the client match in
   `zellij-client/src/lib.rs` (~line 178-209)**; unknown variants are silently ignored there, but
   cli_client loops have their own matches — handle explicitly or ignore deliberately.
8. **What NOT to do**: don't add a generic "send message to pane" Action (reuse CliPipe /
   WriteCharsToPaneId); don't add JSON to the server (protobuf stays); don't reuse the plugin pipe
   machinery (PendingPipes, wasm_bridge pipe_messages) for pane delivery — pane delivery is
   fire-and-forget + completion and is much simpler; don't require `--name` for pane-targeted pipes.
9. **Cross-tab**: pane ids are global, so cross-tab costs nothing for pane-addressed commands —
   do NOT add tab scoping to pane lookups. The only cross-tab gap is "active pane of tab N" /
   "all panes of tab N" addressing; solve it client-side via `list-panes --json` (tab_id +
   is_focused) to avoid proto/server churn. Beware the resolution race and the empty-tab error case.

## 6. Testing checklist

- `cargo x check`, `cargo x test`, `cargo x build` (protoc installed; see CONTRIBUTING.md:25).
  CI runs the same via `cargo x ci` (which also regenerates protobufs).
- **Every milestone has a dedicated test section in §8 — read it before coding; do not stop at the
  checklist below.**
- Two infrastructure gaps to close FIRST (verified: neither exists today):
  - No protobuf round-trip tests anywhere in `zellij-utils/src/ipc/` — add encode→decode→assert
    tests for every new/changed message (L2).
  - The integration harness never spawns the real `zellij` binary — add `run_zellij_cli` /
    `spawn_zellij_cli_background` helpers (L5) before writing integration tests.
- Layers: L1 CLI-parse unit tests (`zellij-utils/src/input/actions.rs`, `cli.rs`) · L2 protobuf
  round-trip (`zellij-utils/src/ipc/protobuf_conversion.rs`) · L3 server unit tests
  (`zellij-server/src/unit/screen_tests.rs`, via `send_cli_action_to_server` /
  `route_arbitrary_action_to_server` + the captured client-message channel) · L4 pure-logic unit
  tests · L5 integration (`zellij-integration-tests/tests/pane_comms.rs`).
- Regression (every milestone): `zellij pipe --plugin ...` (plugin path untouched), `subscribe`,
  `write-chars --pane-id`, `dump-screen` all still pass.

## 7. Definition of done + manual E2E

```bash
# In a zellij session with two panes (pane ids from `zellij action list-panes --json`):
# M2 — pane 1 injects into pane 2 (blocks until delivered):
zellij pipe --pane-id terminal_2 --name demo -- 'hello from pane 1'
zellij action dump-screen --pane-id terminal_2        # contains 'hello from pane 1'
# M2 error path:
zellij pipe --pane-id terminal_999 --name demo -- hi   # exit 2, error on stderr
# Live read (baseline, keep working):
zellij subscribe --pane-id terminal_2 --format json &
# M3 — listener (background), then any plugin using `cli_pipe_output("demo", ...)` shows up:
zellij listen --name demo --format json &
# M4 — wait for output:
zellij subscribe --pane-id terminal_2 --until 'ready' --timeout 30000
# Cross-tab: pane in tab 1 reaches a pane in tab 3 (pane ids are global):
zellij action list-panes --json --state          # note tab_id per pane
zellij pipe --tab-id 3 --name demo -- 'hi tab 3' # resolves to the active pane of tab 3 (M2)
zellij subscribe --tab-id 3 --format json &      # tail the whole tab (M4)
# By tab NAME (list-tabs --json maps name -> position, then resolve as above):
zellij action list-tabs --json
zellij pipe --tab-name "work" --name demo -- 'hi work tab'
# "Check in on tab 2" = three asks, no continuous data flow (design principle):
zellij pipe --tab-name "reviewer" --name prompt -- 'what is your status?'
zellij subscribe --tab-name "reviewer" --until 'done' --timeout 60000   # ask + bounded wait
zellij action dump-screen --pane-id <resolved pane>                     # read on demand
```

Done = M2+M3+M4 implemented with tests (including the cross-tab M1 cases), `cargo x test`
green, changelog entry added, and the E2E script — including its cross-tab lines — passing manually
## 8. Test plan (per milestone)

### M1 tests (baseline regression — all L5, new file `zellij-integration-tests/tests/pane_comms.rs`)

Harness facts (verified): `start_zellij()`, `claim_first_terminal_and_wait_for_prompt`,
`split_right_and_wait_for_prompt`, `expect_pty_spawn()`, `pty_handle.output(b"...")`,
`zellij.wait_until(...)`, grid snapshots. Type into the OTHER pane via its fake pty handle.
CLI commands go through the new `run_zellij_cli` / `spawn_zellij_cli_background` helpers.

- `write_chars_to_other_pane`: two panes; `run_zellij_cli(["action","write-chars","--pane-id","terminal_2","marker-A"])`
  exits 0; pane B's grid contains `marker-A`.
- `dump_screen_round_trips_content`: pane B prints a unique marker via its pty handle;
  `run_zellij_cli(["action","dump-screen","--pane-id","terminal_2"])` stdout contains the marker.
- `subscribe_json_emits_pane_update`: background `subscribe --pane-id terminal_2 --format json`;
  pane B prints text; the handle's stdout produces a line that parses as JSON with
  `event == "pane_update"` and `pane_id == "terminal_2"`.
- `write_chars_across_tabs` / `dump_screen_across_tabs` / `subscribe_across_tabs`: move pane B to a
  new tab (BreakPane keybind — see keys.rs), repeat the three above; pane ids are global, all pass.
- `sync_tab_broadcasts_only_to_target_tab`: `toggle-active-sync-tab --tab-id N`; write once; every
  pane in tab N receives it, panes in other tabs do not.
- `write_chars_to_missing_pane_exits_nonzero`: `--pane-id terminal_999` → non-zero exit, stderr
  mentions the pane id.

### M2 tests (`zellij pipe --pane-id`)

- L1 (`actions.rs`): `pipe_with_pane_id_parses` — CliAction::Pipe{pane_id:"terminal_2"} →
  Action::CliPipe{pane:Some("terminal_2")}; `pipe_with_malformed_pane_id_errors` — error text
  matches the existing "Malformed pane id: expecting either a bare integer ..." string; in
  `cli.rs`: `pipe_pane_id_conflicts_with_plugin`, `pipe_tab_id_conflicts_with_pane_id` (clap
  conflict tests, mirror `subscribe_requires_pane_id` at cli.rs:1646).
- L2: round-trip `Action::CliPipe` with EVERY field set (incl. `pane`) and with `pane: None`;
  encode→decode→assert_eq. (No round-trip tests exist today — this is the first.)
- L3 (`screen_tests.rs`): `cli_pipe_to_pane_emits_paste_with_completion` —
  `route_arbitrary_action_to_server(Action::CliPipe{pane:Some(...)})`; assert a
  `ScreenInstruction::Paste` was sent with the right bytes + pane_id + `Some(NotificationEnd)`,
  and that firing the completion reaches the client (`UnblockInputThread`). `cli_pipe_to_missing_pane_errors`
  — LogError / non-zero path. `cli_pipe_without_pane_keeps_plugin_path` — regression:
  `pane: None` still sends `PluginInstruction::CliPipe`, never Paste.
- L4: `PaneId::from_str` coverage for bare-int / `terminal_N` / `plugin_N` / malformed.
- L5 (`pane_comms.rs`): `pipe_to_pane_delivers_and_blocks` (exit 0, target grid contains payload);
  `pipe_to_pane_multiline_payload`; `pipe_to_missing_pane_exits_2`; `pipe_to_pane_across_tabs`;
  `two_concurrent_pipes_same_name_both_delivered` (distinct pipe_ids — both payloads appear).

### M3 tests (`zellij listen`)

- L2: `ListenToPipe` round-trip (pipe_names vec).
- L3 (server): session-state registry unit tests — `register_listener_then_fan_out` (CliPipeOutput
  delivered to the associated pipe client AND the listener); `dead_listener_pruned_on_send_failure`
  (send fails → entry removed, mirrors screen.rs:7488-7549); `listener_filters_pipe_name` (only its
  own pipe_names).
- L5 (`pane_comms.rs`): `listen_streams_pipe_output` — background `listen --name X --format json`;
  drive CliPipeOutput on X (small fixture plugin that replies `cli_pipe_output(X, payload)` on pipe
  input — add the fixture under the integration-test plugin fixtures); assert a
  `{"event":"pipe_output","pipe_name":"X",...}` line; `listen_raw_prints_plain_lines`;
  `listen_exits_on_session_close`; `listen_ignores_other_pipes` — with an unassociated pipe name Y
  (broadcast fallback at lib.rs:1311), the listener on X prints nothing for Y.

### M4 tests (`subscribe --until/--timeout`)

- L4 (extract the matcher + viewport-differ as pure functions FIRST — then unit test them):
  `until_substring_match`, `until_regex_match` (`/^ready/`), `no_match_yet`, `timeout_elapses`;
  differ: `new_lines_detected`, `duplicate_lines_not_resent` (seen-set), `scrollback_lines_counted_once`.
- L5: `subscribe_until_exits_zero_on_match` (type the trigger into the target pane via its pty
  handle); `subscribe_until_times_out_exit_one` (short `--timeout`); `subscribe_until_ignores_pre_existing_text`
  (marker printed BEFORE subscribing must not match — only lines seen after the initial snapshot);
  `subscribe_tab_id_waits_for_any_pane` (cross-tab resolution path).

### M5 tests (design-phase — write when M5 is started)

- L4: status storage — `status_change_bumps_seq`, `no_change_keeps_seq`; TTL —
  `expired_status_becomes_unknown`; wait matching — `wait_accepts_only_events_after_snapshot`,
  `until_set_matches_status`, `prompt_stall_detected`, `prompt_rejected_when_blocked`.
- L4: OSC parser — `parse_zellij_status_osc_valid`, `malformed_osc_ignored`,
  `status_embedded_in_text`, `multiple_osc_in_one_burst` (mirror the `parse_osc7_path` tests,
  grid.rs:412).
- L3: `notify_pane_status_from_osc_emits_event` — mirror `osc7_emits_cwd_changed`
  (zellij-server/src/unit/pty_tests.rs:427).
- L5: fixture agent wrapper emits the status OSC → `zellij agent wait --until done` exits 0;
  `prompt_stalled_when_not_working`; `prompt_rejected_when_blocked`.
- Plugin-route variant: hub token TTL unit tests; status query round-trip over a pipe (shell →
  hub → reply) using the M3 fixture-plugin pattern.

### Definition of "tests green" for a milestone

L1+L2+L4 unit tests in the touched crates, L3 server tests where the server changed, L5
integration tests for the user-visible behavior, all under `cargo x test`; regression set from
§6 passes; `cargo x proto` produces no diff after a clean regen.

## Appendix: Glossary (terminal internals)

Short definitions of the terms used above, with why each matters for this spec. Read this before
the architecture primer if any term is unfamiliar.

- **PTY (pseudo-terminal)**: a fake terminal implemented by the OS. Each zellij pane runs its
  program attached to its own PTY, so the program believes it is talking to a real terminal while
  its bytes actually go to zellij. This is why zellij (and herdr, tmux, ...) can capture, replay,
  and redirect everything a pane's process writes and reads.

- **PTY master / slave**: the two ends of a PTY. The program's stdin/stdout/stderr are wired to
  the SLAVE end; zellij holds the MASTER end. The program writes to the slave and zellij reads the
  bytes on the master; conversely, bytes zellij writes to the master (e.g.
  `write_chars_to_pane_id`) appear on the slave exactly as if the user typed them. Nothing a
  pane's process outputs reaches any other pane or client except through zellij's master-side
  handling — which is what makes server-side routing (and pane comms) possible at all.

- **Escape sequence**: a run of bytes that starts with the ESC byte (0x1b) and means "this is a
  command, not displayable text". The two families that matter here: CSI (`ESC [ ...`, cursor
  moves, colors, modes) and OSC (`ESC ] ...`, see next). Terminals and emulators parse these
  instead of rendering them as garbage.

- **OSC (Operating System Command)**: the `ESC ] <number> ; <payload> ESC \` family of escape
  sequences used for structured metadata on the text stream: window title, clipboard, links,
  working directory. The canonical example is OSC 7 (cwd): shells emit it on `cd` so the
  terminal can track the directory. zellij ALREADY parses OSC 7 from every pane's output
  (`terminal_pane.rs` vte parser → `grid.rs:412` → `NotifyCwdFromOsc7` → `Event::CwdChanged`).
  **"Parsing OSC"** = recognizing these sequences in a pane's raw byte stream and turning them
  into state/events instead of screen text. It matters here because M5's native agent-status
  transport would be a private OSC (e.g. `ESC ]1337;ZELLIJ_STATUS=working ESC \`) parsed in the
  same path — and because plugins can NEVER see raw escape sequences: by the time a plugin reads
  pane contents (`get_pane_scrollback`) the parser has already consumed them.

- **Out-of-band**: data carried on a SEPARATE channel from the terminal's own text stream, as
  opposed to in-band (keystrokes/text on the terminal stream itself). In zellij, the session
  socket and named pipes are out-of-band channels relative to any pane. The plugin constraint
  this creates: a plugin can only deliver to a shell in-band (typed keystrokes via
  `write_chars_to_pane_id`) or out-of-band ON A CHANNEL THE SHELL ITSELF OPENED (the blocking
  `zellij pipe` RPC, `zellij subscribe`). There is no way to push data to a pane's process that
  never asked for it — no unsolicited out-of-band delivery. This is exactly the on-demand / pull
  model the design-principle section mandates.
