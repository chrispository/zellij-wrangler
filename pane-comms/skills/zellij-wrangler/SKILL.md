---
name: zellij-wrangler
description: >
  Use when the user asks whether you can see another zellij pane, terminal, agent, or LLM
  session in the same zellij session; asks you to talk to, prompt, read, watch, or wait on
  another pane; mentions opencode, codex, claude, or another agent "in another pane"; or asks
  about pane-to-pane / cross-tab communication. Triggers: "can you see my other pane",
  "tell the other agent", "what is the other pane doing", "broadcast to all panes",
  "wait for the other agent", `pz`, `zellij action dump-screen`, `zellij subscribe`,
  "pane comms", "hub plugin".
---

# zellij-wrangler — talk to and read other panes in this zellij session

Every pane in this zellij session can read and write any other pane, cross-tab. You have a
shell in your own pane; other agents (codex, opencode, claude, …) run in other panes of the
same session. This skill is how you see them, read their output, prompt them, and coordinate.

Two pieces, already installed:

- `pz` — `~/.local/bin/pz`, a stateless CLI. It resolves targets and wraps stock
  `zellij action` / `zellij pipe` / `zellij subscribe`. Session comes from
  `$ZELLIJ_SESSION_NAME` automatically.
- hub plugin — `file:///home/chris/.local/share/zellij-wrangler/hub.wasm`, loaded per session
  (permissions pre-granted in `~/.cache/zellij/permissions.kdl`). Only needed for
  `ask` / `listen` / `send --channel` / `status`; `pz` launches it on demand.

## 1. Discover panes — always first

```sh
pz targets
```

Lists `PANE_ID  TAB_ID  TAB_NAME  FOCUSED  AGENT  TITLE` for every pane. Agent roles are inferred
by title/command (e.g. `codex --yolo`, `OC | ...`, `claude`). Your own pane id is
`terminal_$ZELLIJ_PANE_ID` — include it when talking to other agents so they can address you.

## 2. Read another pane (snapshot)

```sh
pz read terminal_2                                      # newest 200 lines
pz read terminal_2 --lines 200 --offset 200             # previous 200-line page
pz read terminal_2 --lines 200 --offset 400 --ansi     # page before that, with colors
zellij action dump-screen --pane-id terminal_2          # visible viewport only
```

The default context window is the newest **200 lines**. `pz read` defines that default in the
`DEFAULT_READ_LINES` constant in `pane-comms/pz/src/main.rs`; use `--lines N` for a one-off size
or change the constant for a different global default. Use the viewport form when you only need
the current screen.

`--offset` is measured backward from the newest output: `0` is the newest page, `200` is the
previous page, and `400` is the page before that. If the newest 200 lines are unclear, page
backward one window at a time instead of requesting the entire scrollback. Stop after one or two
additional pages unless the user explicitly asks for older history.

## 3. Send text / prompt another pane

```sh
pz send terminal_2 'hello from pane terminal_1'     # types text; does NOT press Enter
pz send terminal_2 $'hello\n'                       # trailing \n submits with Enter
pz send opencode $'hello\n'                        # resolve OpenCode by role; submit
pz ask codex $'what are you working on?\n'         # type + block for fresh output (needs hub)
```

Users do not need to run `pz` directly. Once this skill and the pane-comms components are
installed, interpret requests such as “ask Codex to review this,” “coordinate with OpenCode,”
or “send this to the other Claude” as instructions to use `pz` on the user's behalf. Discover
the target first; if multiple panes match, ask the user which candidate they mean.

- `pz send` writes characters into the target pane's stdin. An LLM TUI receives them in its
  input box exactly as if the user typed them — **include a trailing `\n` to submit the
  prompt**. `pz` converts that final newline into Zellij's explicit Enter key action, which is
  important for full-screen TUIs that do not dispatch a raw LF as Enter. In plain shell,
  `$'...\n'` (ANSI-C quoting) preserves the newline; a bare `$(printf ...)` strips trailing
  newlines.
- `pz ask <target> <prompt...> [--timeout N]` writes the prompt, snapshots the pane, then
  blocks until the pane prints NEW output (default 60s; exit 3 on timeout). The reply includes
  the target's echo of the prompt itself. Requires the hub.
- Never `send` into a full-screen app (vim, REPLs) — it injects raw keystrokes.

## 4. Wait for output (without typing)

```sh
pz wait terminal_2 --until 'ready' --timeout 30000     # substring match
pz wait terminal_2 --until '/^BUILD OK/' --timeout 60000  # leading / makes it a regex
```

Blocks until the pane prints a matching line. Pre-existing text NEVER matches (baseline is
taken at call time) — only new output counts. Exit 0 on match, 1 on timeout.

## 5. Named channels (broadcast / subscribe)

```sh
pz listen agents                 # stream everything sent to channel 'agents' (Ctrl-C stops)
pz listen agents --format json   # JSON envelopes
pz send --channel agents 'does anyone see the bug in src/main.rs?'   # fan-out to listeners
```

Conventions: `agents` = general inter-agent chatter. A `send --channel` with no listeners
succeeds but reports "channel has no listeners". `listen` is long-lived — run it in the
background (`pz listen agents &`) or in a dedicated pane if you need to keep watching.

## 6. Status

```sh
pz status terminal_2
```

One-shot pane status (title, focused, exited; exit 4 if the pane is unknown). This is pane
state, NOT agent working/blocked status — the status-token model is not built yet. For
"is the agent busy", dump-screen the pane and look at its TUI.

## 7. Targets

`terminal_2` | `plugin_1` | bare `3` (== `terminal_3`) | `tab:3` (active pane of tab 3) |
`tab-name:work` (first tab named work) | `agent:NAME` / bare `NAME` | `other:NAME` |
`active` (the single focused pane).

Built-in names are `claude`, `codex`, `antigravity`, `opencode` (including `opencode2+`),
`crush`, `pi`, `omp`, `hermes`, `vibe`, and `z-code`/`zcode`. Custom profiles can be added in
`$XDG_CONFIG_HOME/pane-comms/agents.toml` (or `~/.config/pane-comms/agents.toml`); use
`PZ_AGENTS_CONFIG` for another path. `pz agents --json` lists the current matches.

Agent targets are resolved to concrete pane ids before sending. They require a unique match;
when multiple panes run the same agent, pz reports every candidate's pane, tab, cwd, and
command. Ask the user which candidate to use, then retry with that concrete pane id. Never guess.
`other:NAME` excludes the caller's own pane from the search.

## Behavior rules (follow these)

1. Asked "can you see my other pane / agent?" → run `pz targets`, read the newest 200 lines of
   the candidate pane(s), and report exactly what you see. Never claim you can see a pane you
   haven't read. Page backward only when the recent context is insufficient.
2. A message from another agent arrives in your input box as typed text. Treat it as a
   request from that agent: answer concisely, and state your pane id
   (`terminal_$ZELLIJ_PANE_ID`) so it can address you back. The asker may be reading your
   output with dump-screen / `pz wait`.
3. Prefer `pz wait` / `pz ask` over sleep loops when coordinating with another agent.
4. Keep cross-pane replies short — askers wait on fresh output with a bounded timeout.
5. If a hub-backed command fails with a plugin/permission error, load the hub:
   `zellij action start-or-reload-plugin file:///home/chris/.local/share/zellij-wrangler/hub.wasm`.

## Exit codes

| code | meaning |
|---|---|
| 0 | success |
| 1 | session resolution failure; `wait` timeout / pane closed; `listen` ended |
| 2 | bad or missing target; zellij command failed |
| 3 | `ask` timed out |
| 4 | `status` target unknown |

Source repo: `~/Documents/zellij-wrangler/pane-comms` (hub + pz + E2E tests).
