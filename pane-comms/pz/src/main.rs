//! pz — pane-comms companion CLI.
//!
//! `pz` is a thin, stateless wrapper around stock zellij commands plus the pane-comms hub
//! plugin. Every invocation resolves the target, calls `zellij action` / `zellij pipe` /
//! `zellij subscribe`, and exits. Nothing runs in the background; the hub (inside the zellij
//! server) holds all state.
//!
//! Exit codes (part of the contract):
//!   0  success
//!   1  session resolution failure; `wait` timeout / pane closed; `listen` child died
//!   2  bad or missing target; zellij command failed
//!   3  `ask` timed out
//!   4  `status` target unknown/expired

use regex::Regex;
use serde::Deserialize;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const USAGE: &str = "\
pz — cross-pane / cross-tab communication for zellij (hub plugin + this CLI)

USAGE:
    pz [--session <SESSION>] [--hub <URL>] <COMMAND>

COMMANDS:
    send <target> <text...>             Write text into a pane's stdin (cross-tab; pane ids are global)
    send --channel <name> <text...>     Broadcast text to all listeners of a named channel
    ask <target> <prompt...> [--timeout N]
                                        Prompt a pane and block until it produces new output
    wait <target> --until <pattern> [--timeout N]
                                        Block until the pane's output matches pattern
                                        (pattern starting with / is a regex, e.g. /^ready/)
    listen <channel> [--format raw|json]
                                        Stream everything sent to a named channel (Ctrl-C stops)
    status <target>                     One-shot status of a pane (title, focused, exited)
    targets [--json]                    List panes with their tab ids / names

TARGETS (resolved client-side via `zellij action list-panes --json`):
    terminal_2 | plugin_1 | 3          explicit pane id (bare number == terminal_N)
    tab:3                              the active pane of tab 3 (focused, else first terminal)
    tab-name:work                      first tab named \"work\" (names are not unique)
    agent:opencode                     the unique pane running OpenCode (codex/claude too)
    active                             the single focused pane

EXIT CODES: 0 ok, 1 session/timeout, 2 bad target, 3 ask timeout, 4 status unknown.
SESSION: $ZELLIJ_SESSION_NAME, else the single active `zellij ls` session, else --session.
HUB: $PZ_HUB_URL, else <pz>/../hub/target/wasm32-wasip1/release/hub.wasm.
";

#[derive(Debug, Deserialize, serde::Serialize)]
struct PaneEntry {
    id: u32,
    is_plugin: bool,
    is_focused: bool,
    tab_id: u32,
    tab_name: String,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pane_command: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TabEntry {
    position: u32,
    name: String,
}

fn pane_id_string(e: &PaneEntry) -> String {
    format!(
        "{}{}",
        if e.is_plugin { "plugin_" } else { "terminal_" },
        e.id
    )
}

/// Remove ANSI CSI styling from `zellij ls` output. Zellij currently colorizes session
/// names even when stdout is a pipe, so parsing the raw first whitespace-delimited token can
/// accidentally produce a session name like "\\x1b[32;1mmarvellous-stegosaurus\\x1b[m".
fn strip_ansi_csi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() {
                let byte = bytes[i];
                i += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        } else {
            let rest = &input[i..];
            let ch = rest.chars().next().expect("valid UTF-8 boundary");
            output.push(ch);
            i += ch.len_utf8();
        }
    }
    output
}

fn fail(code: i32, msg: &str) -> ! {
    eprintln!("pz: {msg}");
    std::process::exit(code);
}

fn zellij_cmd(session: &str, args: &[&str]) -> Command {
    let mut c = Command::new("zellij");
    c.arg("--session").arg(session);
    c.args(args);
    c
}

fn run_zellij(session: &str, args: &[&str]) -> Result<std::process::Output, String> {
    zellij_cmd(session, args)
        .output()
        .map_err(|e| format!("failed to run zellij: {e}"))
}

/// Parse `zellij ls` output into `(name, is_current)` pairs, skipping EXITED sessions.
fn live_sessions(ls_output: &str) -> Vec<(String, bool)> {
    ls_output
        .lines()
        .map(strip_ansi_csi)
        .map(|line| line.trim().to_owned())
        .filter(|l| !l.is_empty() && !l.contains("EXITED"))
        .map(|l| {
            let name = l.split_whitespace().next().unwrap_or("").to_owned();
            (name, l.contains("(current)"))
        })
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

/// Choose the session to target: an env-derived name only if it is still live, else the
/// session zellij marks `(current)`, else the single live session.
fn choose_session(env_name: Option<&str>, live: &[(String, bool)]) -> Result<String, String> {
    if let Some(name) = env_name.filter(|s| !s.is_empty()) {
        if live.iter().any(|(n, _)| n == name) {
            return Ok(name.to_owned());
        }
        // env session is stale (e.g. inherited from a session that has since exited) — fall
        // through to the live-session logic instead of failing on a dead socket.
    }
    if let Some((name, true)) = live.iter().find(|(_, is_current)| *is_current) {
        return Ok(name.clone());
    }
    match live.len() {
        1 => Ok(live[0].0.clone()),
        0 => Err(
            "no active zellij session found — run from inside a session or pass --session <name>"
                .to_owned(),
        ),
        _ => Err(format!(
            "multiple active sessions ({}); pass --session <name>",
            live.iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Check a session by asking that exact server for pane JSON. This is deliberately stronger than
/// trusting `zellij ls`: when ZELLIJ_SESSION_NAME is stale, zellij treats that name as current and
/// changes the presentation of `zellij ls` as well.
fn session_is_usable(session: &str) -> bool {
    run_zellij(session, &["action", "list-panes", "--json"])
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| serde_json::from_slice::<Vec<PaneEntry>>(&out.stdout).ok())
        .is_some()
}

fn session_has_attached_client(session: &str) -> bool {
    run_zellij(session, &["action", "list-clients"])
        .ok()
        .filter(|out| out.status.success())
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .skip(1)
                .any(|line| !line.trim().is_empty())
        })
        .unwrap_or(false)
}

/// Find the session containing the pane id exported to the current process. Pane ids are only
/// unique within a session, so an ambiguous match must be reported instead of guessed.
fn session_for_pane_id(live: &[(String, bool)], pane_id: u32) -> Result<Option<String>, String> {
    let matches = live
        .iter()
        .filter_map(|(session, _)| {
            let out = run_zellij(session, &["action", "list-panes", "--json"]).ok()?;
            if !out.status.success() {
                return None;
            }
            let panes = serde_json::from_slice::<Vec<PaneEntry>>(&out.stdout).ok()?;
            panes
                .iter()
                .any(|pane| !pane.is_plugin && pane.id == pane_id)
                .then(|| session.clone())
        })
        .collect::<Vec<_>>();
    let attached_matches = matches
        .iter()
        .filter(|session| session_has_attached_client(session))
        .cloned()
        .collect::<Vec<_>>();
    let matches = if attached_matches.is_empty() {
        matches
    } else {
        attached_matches
    };
    match matches.as_slice() {
        [] => Ok(None),
        [session] => Ok(Some(session.clone())),
        many => Err(format!(
            "pane terminal_{pane_id} appears in multiple active sessions ({}); pass --session <name>",
            many.join(", ")
        )),
    }
}

fn resolve_session(explicit: Option<&str>) -> Result<String, String> {
    if let Some(s) = explicit {
        return Ok(s.to_owned());
    }

    let env_name = env::var("ZELLIJ_SESSION_NAME").ok();
    if let Some(name) = env_name.as_deref().filter(|s| !s.is_empty()) {
        if session_is_usable(name) {
            return Ok(name.to_owned());
        }
    }

    // Remove the possibly stale env var before listing. Otherwise zellij can label the stale
    // session as `(current)` and omit its EXITED marker, defeating fallback discovery.
    let out = Command::new("zellij")
        .arg("ls")
        .env_remove("ZELLIJ_SESSION_NAME")
        .output()
        .map_err(|e| format!("failed to run `zellij ls`: {e}"))?;
    let live = live_sessions(&String::from_utf8_lossy(&out.stdout));
    if let Ok(pane_id) = env::var("ZELLIJ_PANE_ID")
        .ok()
        .unwrap_or_default()
        .parse::<u32>()
    {
        if let Some(session) = session_for_pane_id(&live, pane_id)? {
            return Ok(session);
        }
    }
    choose_session(None, &live)
}

/// Parse a `zellij action <what>` JSON reply; if stdout is not JSON, prefer reporting the
/// command's stderr (zellij prints "Session not found" etc. there but can still exit 0).
fn parse_json_or_err<T: serde::de::DeserializeOwned>(
    session: &str,
    what: &str,
    out: &std::process::Output,
) -> Result<T, String> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str::<T>(&stdout).map_err(|e| {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        if !stderr.is_empty() {
            format!("`zellij action {what}` in session {session}: {stderr}")
        } else {
            format!("could not parse `{what}` output from session {session}: {e}")
        }
    })
}

fn list_panes(session: &str) -> Result<Vec<PaneEntry>, String> {
    let out = run_zellij(session, &["action", "list-panes", "--json"])?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    parse_json_or_err(session, "list-panes --json", &out)
}

fn list_tabs(session: &str) -> Result<Vec<TabEntry>, String> {
    let out = run_zellij(session, &["action", "list-tabs", "--json"])?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    parse_json_or_err(session, "list-tabs --json", &out)
}

/// Resolve a target spec to a concrete pane id string ("terminal_N" / "plugin_N").
fn resolve_target(session: &str, spec: &str) -> Result<String, String> {
    if let Some(role) = spec.strip_prefix("agent:") {
        return resolve_agent_target(session, role);
    }
    // Explicit pane ids pass through — but only if the pane actually exists. (0.44.3's
    // `write-chars` silently succeeds for missing panes, so pz validates client-side.)
    let explicit = Regex::new(r"^(terminal_\d+|plugin_\d+)$").unwrap();
    if explicit.is_match(spec) {
        let panes = list_panes(session)?;
        if panes.iter().any(|p| pane_id_string(p) == spec) {
            return Ok(spec.to_owned());
        }
        return Err(format!("pane {spec} not found"));
    }
    if spec.chars().all(|c| c.is_ascii_digit()) {
        let id = format!("terminal_{spec}");
        let panes = list_panes(session)?;
        if panes.iter().any(|p| pane_id_string(p) == id) {
            return Ok(id);
        }
        return Err(format!("pane {id} not found"));
    }
    if let Some(name) = spec.strip_prefix("tab-name:") {
        let tabs = list_tabs(session)?;
        let tab = tabs.iter().find(|t| t.name == name).ok_or_else(|| {
            let valid = tabs
                .iter()
                .map(|t| format!("{} ({})", t.name, t.position))
                .collect::<Vec<_>>()
                .join(", ");
            format!("no tab named '{name}' — valid tabs: {valid}")
        })?;
        return resolve_tab_pane(session, tab.position);
    }
    if let Some(n) = spec.strip_prefix("tab:") {
        let pos: u32 = n
            .parse()
            .map_err(|_| format!("malformed tab id '{n}' (expected a number)"))?;
        return resolve_tab_pane(session, pos);
    }
    if spec == "active" {
        let panes = list_panes(session)?;
        let focused: Vec<&PaneEntry> = panes
            .iter()
            .filter(|p| p.is_focused && !p.is_plugin)
            .collect();
        return match focused.len() {
            1 => Ok(pane_id_string(focused[0])),
            0 => Err("no focused terminal pane — pass an explicit pane id".to_owned()),
            _ => Err(format!(
                "multiple focused panes ({}); pass an explicit pane id",
                focused
                    .iter()
                    .map(|p| pane_id_string(p))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
    }
    Err(format!(
        "unknown target '{spec}' (expected terminal_N, plugin_N, N, tab:N, tab-name:NAME, agent:NAME, or active)"
    ))
}

/// Infer a stable agent role from the pane command. Pane titles are useful as a fallback (for
/// example `OC | ...`), but commands are the primary signal because titles often change while
/// an agent is running.
fn agent_kind(pane: &PaneEntry) -> Option<&'static str> {
    if pane.is_plugin {
        return None;
    }
    let command = pane
        .pane_command
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    let title = pane.title.to_ascii_lowercase();
    if command.contains("opencode") || title.starts_with("oc |") {
        Some("opencode")
    } else if command.contains("codex") {
        Some("codex")
    } else if command.contains("claude") {
        Some("claude")
    } else {
        None
    }
}

fn agent_role_matches(role: &str, kind: &str) -> bool {
    match role.trim().to_ascii_lowercase().as_str() {
        "opencode" | "open-code" | "oc" => kind == "opencode",
        "codex" => kind == "codex",
        "claude" => kind == "claude",
        _ => false,
    }
}

/// Resolve an agent role to a pane. Role targets intentionally require a unique match: silently
/// choosing one of two Codex panes would send a prompt to the wrong agent. The error lists the
/// candidates so the caller can choose a concrete pane id.
fn resolve_agent_target(session: &str, role: &str) -> Result<String, String> {
    let role = role.trim();
    if role.is_empty() {
        return Err(
            "empty agent role (use agent:opencode, agent:codex, or agent:claude)".to_owned(),
        );
    }
    let panes = list_panes(session)?;
    let candidates: Vec<&PaneEntry> = panes
        .iter()
        .filter(|pane| agent_kind(pane).is_some_and(|kind| agent_role_matches(role, kind)))
        .collect();
    match candidates.as_slice() {
        [pane] => Ok(pane_id_string(pane)),
        [] => {
            let available = panes
                .iter()
                .filter_map(|pane| {
                    agent_kind(pane).map(|kind| format!("agent:{kind}={}", pane_id_string(pane)))
                })
                .collect::<Vec<_>>();
            if available.is_empty() {
                Err(format!("no agent '{role}' found"))
            } else {
                Err(format!(
                    "no agent '{role}' found (available: {})",
                    available.join(", ")
                ))
            }
        },
        many => {
            let candidates = many
                .iter()
                .map(|pane| {
                    format!(
                        "{} (tab {}: {})",
                        pane_id_string(pane),
                        pane.tab_id,
                        pane.title
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "agent '{role}' is ambiguous: {candidates}; use a concrete pane id"
            ))
        },
    }
}

/// The active pane of a tab: the focused one, else the first non-plugin pane. Errors if the tab
/// has no usable panes.
fn resolve_tab_pane(session: &str, tab_position: u32) -> Result<String, String> {
    let panes = list_panes(session)?;
    let in_tab: Vec<&PaneEntry> = panes
        .iter()
        .filter(|p| p.tab_id == tab_position && !p.is_plugin)
        .collect();
    let pane = in_tab
        .iter()
        .find(|p| p.is_focused)
        .or_else(|| in_tab.first())
        .ok_or_else(|| format!("tab {tab_position} has no terminal panes"))?;
    Ok(pane_id_string(pane))
}

// --- hub RPC -------------------------------------------------------------------------------

fn default_hub_url() -> Result<String, String> {
    if let Ok(url) = env::var("PZ_HUB_URL") {
        return Ok(url);
    }
    let exe = env::current_exe().map_err(|e| format!("cannot locate pz binary: {e}"))?;
    let exe_dir = exe.parent().ok_or("cannot locate pz binary directory")?;
    // Candidate locations, in order:
    //   installed layout:      ~/.local/share/zellij-wrangler/hub.wasm
    //   standard workspace:    <ws>/pz/target/...  and <ws>/hub/target/...
    //   target-dir redirect:   <root>/target/...   (pz and hub share the root target dir)
    let mut candidates = Vec::new();
    if let Ok(home) = env::var("HOME") {
        candidates.push(
            PathBuf::from(&home)
                .join(".local")
                .join("share")
                .join("zellij-wrangler")
                .join("hub.wasm"),
        );
    }
    candidates.extend([
        exe_dir
            .join("..")
            .join("hub")
            .join("target")
            .join("wasm32-wasip1")
            .join("release")
            .join("hub.wasm"),
        exe_dir
            .join("..")
            .join("wasm32-wasip1")
            .join("release")
            .join("hub.wasm"),
        exe_dir
            .join("..")
            .join("..")
            .join("hub")
            .join("target")
            .join("wasm32-wasip1")
            .join("release")
            .join("hub.wasm"),
    ]);
    for candidate in &candidates {
        if let Ok(canonical) = candidate.canonicalize() {
            return Ok(format!("file://{}", canonical.display()));
        }
    }
    Err(format!(
        "hub plugin not found (looked at {}) — build it with `cargo build -p hub --target wasm32-wasip1 --release`, or set PZ_HUB_URL",
        candidates.iter().map(|c| c.display().to_string()).collect::<Vec<_>>().join(", ")
    ))
}

fn resolve_hub(hub_opt: Option<String>) -> String {
    hub_opt.unwrap_or_else(|| default_hub_url().unwrap_or_else(|e| fail(1, &e)))
}

enum HubError {
    Call(String),
}
fn hub_rpc(
    session: &str,
    hub_url: &str,
    name: &str,
    payload: &str,
    outer_timeout: Duration,
) -> Result<serde_json::Value, HubError> {
    let mut child = zellij_cmd(
        session,
        &["pipe", "--plugin", hub_url, "--name", name, "--", payload],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|e| HubError::Call(format!("failed to run `zellij pipe`: {e}")))?;

    let deadline = Instant::now() + outer_timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(HubError::Call("zellij pipe did not answer in time (is the hub running? are its permissions granted?)".to_owned()));
                }
                std::thread::sleep(Duration::from_millis(50));
            },
            Err(e) => {
                return Err(HubError::Call(format!(
                    "failed waiting for zellij pipe: {e}"
                )))
            },
        }
    };
    let out = child
        .wait_with_output()
        .map_err(|e| HubError::Call(e.to_string()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !status.success() {
        let stderr = stderr.trim().to_owned();
        let msg = if stderr.is_empty() {
            stdout.trim().to_owned()
        } else {
            stderr
        };
        return Err(HubError::Call(msg));
    }
    serde_json::from_str(stdout.trim())
        .map_err(|e| HubError::Call(format!("hub replied with non-JSON '{stdout}' ({e})")))
}

// --- wait matcher (pure, unit-tested) -----------------------------------------------------

/// A match pattern: substring, or a regex when written `/.../`.
enum Pattern {
    Substring(String),
    Regex(Regex),
}

impl Pattern {
    fn new(spec: &str) -> Result<Pattern, String> {
        if let Some(re) = spec.strip_prefix('/').and_then(|r| r.strip_suffix('/')) {
            Regex::new(re)
                .map(Pattern::Regex)
                .map_err(|e| format!("invalid regex '{re}': {e}"))
        } else {
            Ok(Pattern::Substring(spec.to_owned()))
        }
    }
    fn matches(&self, line: &str) -> bool {
        match self {
            Pattern::Substring(needle) => line.contains(needle),
            Pattern::Regex(rx) => rx.is_match(line),
        }
    }
}

/// Tracks which lines have been seen. The initial snapshot is noted as baseline (never
/// matched); only lines observed afterwards are reported as new (spec M4: pre-existing text
/// must not match).
struct LineTracker {
    seen: std::collections::HashSet<String>,
}

impl LineTracker {
    fn new() -> Self {
        LineTracker {
            seen: std::collections::HashSet::new(),
        }
    }
    fn note_initial(&mut self, lines: &[String]) {
        self.seen.extend(lines.iter().cloned());
    }
    /// Mark-and-return lines not seen before; duplicate lines are never reported twice.
    fn new_lines<'a>(&mut self, lines: &'a [String]) -> Vec<&'a str> {
        lines
            .iter()
            .filter(|l| self.seen.insert((*l).clone()))
            .map(|l| l.as_str())
            .collect()
    }
}

// --- subcommands ---------------------------------------------------------------------------

fn cmd_send(session: &str, hub_opt: Option<String>, args: &[String]) -> i32 {
    if args.is_empty() {
        fail(
            2,
            "usage: pz send <target> <text...> | pz send --channel <name> <text...>",
        );
    }
    if args[0] == "--channel" {
        if args.len() < 3 {
            fail(2, "usage: pz send --channel <name> <text...>");
        }
        let (channel, text) = (args[1].clone(), args[2..].join(" "));
        let hub_url = resolve_hub(hub_opt);
        let payload =
            serde_json::json!({"cmd": "send", "channel": channel, "text": text}).to_string();
        return match hub_rpc(session, &hub_url, "send", &payload, Duration::from_secs(10)) {
            Ok(v) if v["ok"] == true => {
                if let Some(msg) = v["reply"].as_str() {
                    println!("{msg}");
                }
                0
            },
            Ok(v) => {
                eprintln!("pz: {}", v["error"].as_str().unwrap_or("hub error"));
                2
            },
            Err(HubError::Call(e)) => fail(2, &e),
        };
    }
    let target = &args[0];
    let text = args[1..].join(" ");
    if text.is_empty() {
        fail(2, "empty text — nothing to send");
    }
    let pane = match resolve_target(session, target) {
        Ok(p) => p,
        Err(e) => fail(2, &e),
    };
    match run_zellij(
        session,
        &["action", "write-chars", "--pane-id", &pane, &text],
    ) {
        Ok(out) if out.status.success() => 0,
        Ok(out) => {
            eprintln!("pz: {}", String::from_utf8_lossy(&out.stderr).trim());
            2
        },
        Err(e) => fail(2, &e),
    }
}

fn cmd_ask(session: &str, hub_opt: Option<String>, args: &[String]) -> i32 {
    let mut timeout_ms: u64 = 60_000;
    let mut positional: Vec<&String> = vec![];
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--timeout" => {
                i += 1;
                if i >= args.len() {
                    fail(2, "--timeout requires a value (milliseconds)");
                }
                timeout_ms = args[i]
                    .parse()
                    .unwrap_or_else(|_| fail(2, "--timeout must be a number of milliseconds"));
            },
            a if a.starts_with("--") => fail(2, &format!("unknown flag '{a}' for ask")),
            _ => positional.push(&args[i]),
        }
        i += 1;
    }
    if positional.len() < 2 {
        fail(2, "usage: pz ask <target> <prompt...> [--timeout N]");
    }
    let target = positional[0];
    let prompt = positional[1..]
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let pane = match resolve_target(session, target) {
        Ok(p) => p,
        Err(e) => fail(2, &e),
    };
    let hub_url = resolve_hub(hub_opt);
    let payload = serde_json::json!({
        "cmd": "ask",
        "target": pane,
        "prompt": prompt,
        "timeout_ms": timeout_ms,
    })
    .to_string();
    let outer = Duration::from_millis(timeout_ms + 10_000);
    match hub_rpc(session, &hub_url, "ask", &payload, outer) {
        Ok(v) if v["ok"] == true => {
            if let Some(reply) = v["reply"].as_str() {
                println!("{reply}");
            }
            0
        },
        Ok(v) => {
            let err = v["error"].as_str().unwrap_or("hub error");
            if err == "ask_timeout" {
                eprintln!("pz: ask timed out after {timeout_ms} ms");
                3
            } else {
                eprintln!("pz: {err}");
                2
            }
        },
        Err(HubError::Call(e)) => {
            eprintln!("pz: {e}");
            3
        },
    }
}

fn cmd_wait(session: &str, args: &[String]) -> i32 {
    let mut until: Option<String> = None;
    let mut timeout_ms: Option<u64> = None;
    let mut positional: Vec<&String> = vec![];
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--until" => {
                i += 1;
                if i >= args.len() {
                    fail(2, "--until requires a pattern");
                }
                until = Some(args[i].clone());
            },
            "--timeout" => {
                i += 1;
                if i >= args.len() {
                    fail(2, "--timeout requires a value (milliseconds)");
                }
                timeout_ms = Some(
                    args[i]
                        .parse()
                        .unwrap_or_else(|_| fail(2, "--timeout must be a number of milliseconds")),
                );
            },
            a if a.starts_with("--") => fail(2, &format!("unknown flag '{a}' for wait")),
            _ => positional.push(&args[i]),
        }
        i += 1;
    }
    if positional.len() != 1 {
        fail(2, "usage: pz wait <target> --until <pattern> [--timeout N]");
    }
    let until = until.unwrap_or_else(|| fail(2, "--until <pattern> is required"));
    let pattern = match Pattern::new(&until) {
        Ok(p) => p,
        Err(e) => fail(2, &e),
    };
    let pattern_desc = if until.starts_with('/') {
        format!("regex {until}")
    } else {
        format!("substring \"{until}\"")
    };

    let pane = match resolve_target(session, positional[0]) {
        Ok(p) => p,
        Err(e) => fail(2, &e),
    };
    let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));

    let mut child = match zellij_cmd(
        session,
        &["subscribe", "--pane-id", &pane, "--format", "json"],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    {
        Ok(c) => c,
        Err(e) => fail(1, &format!("failed to run `zellij subscribe`: {e}")),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                },
                Err(_) => break,
            }
        }
    });

    let mut tracker = LineTracker::new();
    loop {
        let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        match remaining {
            Some(rem) if rem.is_zero() => {
                let _ = child.kill();
                eprintln!(
                    "pz: wait for {pattern_desc} timed out after {} ms",
                    timeout_ms.unwrap_or(0)
                );
                return 1;
            },
            _ => {},
        }
        let line = match rx.recv_timeout(remaining.unwrap_or(Duration::from_secs(1))) {
            Ok(l) => l,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let event: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue, // non-JSON noise; ignore
        };
        match event["event"].as_str() {
            Some("pane_update") => {
                let is_initial = event["is_initial"].as_bool().unwrap_or(false);
                let mut lines: Vec<String> = vec![];
                if let Some(sb) = event["scrollback"].as_array() {
                    lines.extend(
                        sb.iter()
                            .filter_map(|v| v.as_str().map(str::trim_end).map(str::to_owned)),
                    );
                }
                if let Some(vp) = event["viewport"].as_array() {
                    lines.extend(
                        vp.iter()
                            .filter_map(|v| v.as_str().map(str::trim_end).map(str::to_owned)),
                    );
                }
                if is_initial {
                    tracker.note_initial(&lines);
                    continue;
                }
                for line in tracker.new_lines(&lines) {
                    if pattern.matches(line) {
                        let _ = child.kill();
                        return 0;
                    }
                }
            },
            Some("pane_closed") => {
                let _ = child.kill();
                eprintln!("pz: target pane closed before a match");
                return 1;
            },
            _ => {},
        }
    }
    let _ = reader.join();
    let _ = child.wait();
    eprintln!("pz: subscription ended before a match");
    1
}

fn cmd_listen(session: &str, hub_opt: Option<String>, args: &[String]) -> i32 {
    let mut format = "raw";
    let mut positional: Vec<&String> = vec![];
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--format" => {
                i += 1;
                if i >= args.len() {
                    fail(2, "--format requires raw or json");
                }
                format = match args[i].as_str() {
                    "raw" | "json" => args[i].as_str(),
                    other => fail(2, &format!("unknown format '{other}' (raw|json)")),
                };
            },
            a if a.starts_with("--") => fail(2, &format!("unknown flag '{a}' for listen")),
            _ => positional.push(&args[i]),
        }
        i += 1;
    }
    if positional.len() != 1 {
        fail(2, "usage: pz listen <channel> [--format raw|json]");
    }
    let channel = positional[0].clone();
    let hub_url = resolve_hub(hub_opt);
    let payload = serde_json::json!({"cmd": "listen", "channel": channel}).to_string();
    let mut child = match zellij_cmd(
        session,
        &[
            "pipe", "--plugin", &hub_url, "--name", &channel, "--", &payload,
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    {
        Ok(c) => c,
        Err(e) => fail(1, &format!("failed to run `zellij pipe`: {e}")),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let out = std::io::stdout();
    let mut out = out.lock();
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(line) => {
                let trimmed = line.trim();
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    match event["event"].as_str() {
                        // ack/subscribed envelope — internal, not user data
                        Some("ack") | Some("subscribed") => continue,
                        Some("channel") => {
                            let payload = event["payload"].as_str().unwrap_or("");
                            if format == "json" {
                                let wrapped = serde_json::json!({
                                    "event": "pipe_output",
                                    "pipe_name": channel,
                                    "output": payload,
                                });
                                let _ = writeln!(out, "{wrapped}");
                            } else {
                                let _ = writeln!(out, "{payload}");
                            }
                        },
                        _ => {
                            // hub error envelope or unknown — surface it
                            let _ = writeln!(out, "{line}");
                        },
                    }
                } else if format == "raw" {
                    let _ = writeln!(out, "{line}");
                }
            },
            Err(_) => break,
        }
    }
    let _ = child.wait();
    eprintln!("pz: channel listener ended");
    1
}

fn cmd_status(session: &str, hub_opt: Option<String>, args: &[String]) -> i32 {
    if args.len() != 1 {
        fail(2, "usage: pz status <target>");
    }
    let pane = match resolve_target(session, &args[0]) {
        Ok(p) => p,
        // spec: `pz status` reports unknown/expired targets as exit 4 (distinct from the
        // exit-2 "bad target" contract of send/wait).
        Err(e) => fail(4, &e),
    };
    let hub_url = resolve_hub(hub_opt);
    let payload = serde_json::json!({"cmd": "status", "target": pane}).to_string();
    match hub_rpc(
        session,
        &hub_url,
        "status",
        &payload,
        Duration::from_secs(10),
    ) {
        Ok(v) if v["ok"] == true => {
            println!(
                "{}",
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            );
            0
        },
        Ok(v) => {
            eprintln!("pz: {}", v["error"].as_str().unwrap_or("unknown status"));
            4
        },
        Err(HubError::Call(e)) => fail(4, &e),
    }
}

fn cmd_targets(session: &str, args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let panes = match list_panes(session) {
        Ok(p) => p,
        Err(e) => fail(1, &e),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&panes).unwrap_or_default()
        );
        return 0;
    }
    println!("PANE_ID      TAB_ID  TAB_NAME   FOCUSED  AGENT      TITLE");
    for p in &panes {
        println!(
            "{:<12} {:<7} {:<10} {:<8} {:<10} {}",
            pane_id_string(p),
            p.tab_id,
            p.tab_name,
            p.is_focused,
            agent_kind(p).unwrap_or("-"),
            p.title
        );
    }
    0
}

fn main() {
    let argv: Vec<String> = env::args().skip(1).collect();
    let mut session_opt: Option<String> = None;
    let mut hub_opt: Option<String> = None;
    let mut i = 0;
    // Global flags (--session/--hub) are only parsed BEFORE the subcommand; anything after the
    // subcommand belongs to the subcommand's own parser.
    while i < argv.len() {
        match argv[i].as_str() {
            "--session" => {
                i += 1;
                if i >= argv.len() {
                    fail(2, "--session requires a value");
                }
                session_opt = Some(argv[i].clone());
            },
            "--hub" => {
                i += 1;
                if i >= argv.len() {
                    fail(2, "--hub requires a URL");
                }
                hub_opt = Some(argv[i].clone());
            },
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            },
            a if a.starts_with("--") => fail(
                2,
                &format!("unknown flag '{a}' before subcommand (use --help)"),
            ),
            _ => break, // subcommand starts here; pass the rest through untouched
        }
        i += 1;
    }
    let rest: Vec<String> = argv[i..].to_vec();
    if rest.is_empty() {
        fail(2, "no command given (use --help)");
    }
    let cmd = rest[0].clone();
    let cmd_args = &rest[1..];
    let session = match resolve_session(session_opt.as_deref()) {
        Ok(s) => s,
        Err(e) => fail(1, &e),
    };
    let code = match cmd.as_str() {
        "send" => cmd_send(&session, hub_opt.clone(), cmd_args),
        "ask" => cmd_ask(&session, hub_opt.clone(), cmd_args),
        "wait" => cmd_wait(&session, cmd_args),
        "listen" => cmd_listen(&session, hub_opt.clone(), cmd_args),
        "status" => cmd_status(&session, hub_opt.clone(), cmd_args),
        "targets" => cmd_targets(&session, cmd_args),
        other => fail(2, &format!("unknown command '{other}' (use --help)")),
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn until_substring_match() {
        let p = Pattern::new("ready").unwrap();
        assert!(p.matches("shell ready now"));
        assert!(!p.matches("readey now"));
    }

    #[test]
    fn until_regex_match() {
        let p = Pattern::new("/^ready/").unwrap();
        assert!(p.matches("ready NOW"));
        assert!(!p.matches("not ready"));
    }

    #[test]
    fn invalid_regex_errors() {
        assert!(Pattern::new("/[unclosed/").is_err());
    }

    #[test]
    fn new_lines_detected() {
        let mut t = LineTracker::new();
        t.note_initial(&["old".to_owned()]);
        let input = ["old".to_owned(), "fresh".to_owned()];
        let new = t.new_lines(&input);
        assert_eq!(new, vec!["fresh"]);
    }

    #[test]
    fn duplicate_lines_not_resent() {
        let mut t = LineTracker::new();
        assert_eq!(t.new_lines(&["a".to_owned(), "a".to_owned()]), vec!["a"]);
        assert!(t.new_lines(&["a".to_owned()]).is_empty());
    }

    #[test]
    fn initial_snapshot_never_matches() {
        // Lines in the initial snapshot are baseline: they are never reported as new.
        let mut t = LineTracker::new();
        t.note_initial(&["ready".to_owned()]);
        let input = ["ready".to_owned(), "go".to_owned()];
        let new = t.new_lines(&input);
        assert_eq!(new, vec!["go"]);
    }

    #[test]
    fn trailing_whitespace_is_stripped_before_matching() {
        // Viewport lines are padded to the pane width; the caller trims before tracking.
        let mut t = LineTracker::new();
        t.note_initial(&["sh-5.3$".to_owned()]);
        let input = ["sh-5.3$ ready                                     ".to_owned()];
        let trimmed: Vec<String> = input.iter().map(|l| l.trim_end().to_owned()).collect();
        let new = t.new_lines(&trimmed);
        assert_eq!(new, vec!["sh-5.3$ ready"]);
        assert!(Pattern::new("ready").unwrap().matches(new[0]));
    }

    const LS_OUTPUT: &str = "\
session-a [Created 1h 0m 0s ago]
marvellous-stegosaurus [Created 12m 34s ago] (current)
pzdbg [Created 1h 11m 29s ago]
quadratic-mountain [Created 2days 5h 52m 40s ago] (EXITED - attach to resurrect)
";

    #[test]
    fn live_sessions_skips_exited_and_marks_current() {
        let live = live_sessions(LS_OUTPUT);
        assert_eq!(
            live,
            vec![
                ("session-a".to_owned(), false),
                ("marvellous-stegosaurus".to_owned(), true),
                ("pzdbg".to_owned(), false),
            ]
        );
    }

    #[test]
    fn live_sessions_strips_zellij_ansi_colors() {
        let colored = "\x1b[32;1mmarvellous-stegosaurus\x1b[m [Created 1m ago] (current)\n";
        assert_eq!(
            live_sessions(colored),
            vec![("marvellous-stegosaurus".to_owned(), true)]
        );
    }

    #[test]
    fn agent_kind_uses_command_and_title() {
        let opencode = PaneEntry {
            id: 1,
            is_plugin: false,
            is_focused: true,
            tab_id: 0,
            tab_name: "work".to_owned(),
            title: "OC | task".to_owned(),
            pane_command: Some("opencode2".to_owned()),
        };
        assert_eq!(agent_kind(&opencode), Some("opencode"));
        assert!(agent_role_matches("oc", "opencode"));
    }

    #[test]
    fn choose_session_prefers_live_env_name() {
        let live = live_sessions(LS_OUTPUT);
        assert_eq!(choose_session(Some("pzdbg"), &live).unwrap(), "pzdbg");
    }

    #[test]
    fn choose_session_falls_back_from_stale_env_to_current() {
        // opencode regression: env froze ZELLIJ_SESSION_NAME from a session that exited.
        let live = live_sessions(LS_OUTPUT);
        assert_eq!(
            choose_session(Some("quadratic-mountain"), &live).unwrap(),
            "marvellous-stegosaurus"
        );
    }

    #[test]
    fn choose_session_errors_on_multiple_live_without_current() {
        let live = live_sessions("a [Created 1h 0m 0s ago]\nb [Created 1h 0m 0s ago]\n");
        let err = choose_session(None, &live).unwrap_err();
        assert!(err.contains("multiple active sessions"), "{err}");
    }
}
