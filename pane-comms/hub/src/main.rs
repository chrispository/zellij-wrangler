//! pane-comms hub — a zellij plugin implementing cross-pane / cross-tab communication.
//!
//! The hub is the only always-on component of pane-comms. It loads inside a zellij session
//! (layout `run_plugin`, `zellij action start-or-reload-plugin`, or on demand the first time a
//! `zellij pipe --plugin hub.wasm ...` command runs) and holds all channel state: named-channel
//! subscribers and in-flight `ask` waits. It is deliberately stateless across sessions.
//!
//! Wire protocol: every request is a single-line JSON envelope delivered via `zellij pipe
//! --plugin hub.wasm --name <name> -- <payload>`. The hub answers on the *pipe id* (the UUID the
//! CLI client is registered under — `PipeMessage.source == PipeSource::Cli(pipe_id)`), never on
//! the human-readable pipe name: the server's pipe→client registry only contains the UUID.
//!
//! Requests:
//! ```json
//! {"cmd":"send","channel":"demo","text":"hi"}                  // fan out to channel subscribers
//! {"cmd":"listen","channel":"demo"}                            // subscribe THIS pipe to a channel
//! {"cmd":"unlisten","channel":"demo"}
//! {"cmd":"ask","target":"terminal_2","prompt":"...","timeout_ms":60000}
//! {"cmd":"status","target":"terminal_2"}
//! {"cmd":"channels"}                                           // debug: registry dump
//! ```
//!
//! Replies are `{"ok":true,...}` / `{"ok":false,"error":"..."}` JSON on the same pipe,
//! terminated by `unblock_cli_pipe_input` (the invoking CLI exits when it sees that).
//!
//! Channel fan-out sends `{"event":"channel","channel":"demo","payload":"hi"}` on each
//! subscriber's pipe WITHOUT unblocking — subscribers stay open and stream (zjw listen).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use zellij_tile::prelude::*;

/// Poll interval for pending `ask` waits (seconds).
const POLL_SECS: f64 = 0.3;
/// Maximum scrollback lines pulled per poll (bounds diff cost).
const MAX_POLL_LINES: usize = 500;

#[derive(Debug, Deserialize)]
struct Request {
    cmd: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize, Default)]
struct Reply {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane_id: Option<String>,
}

impl Reply {
    fn ok(reply: impl Into<String>, reply_type: &str, pane_id: Option<String>) -> Self {
        Reply {
            ok: true,
            reply: Some(reply.into()),
            reply_type: Some(reply_type.to_owned()),
            pane_id,
            ..Default::default()
        }
    }
    fn err(error: impl Into<String>) -> Self {
        Reply {
            ok: false,
            error: Some(error.into()),
            ..Default::default()
        }
    }
}

/// An in-flight `ask`: prompt delivered, waiting for fresh output on the target pane.
struct PendingAsk {
    target: PaneId,
    /// Content snapshot taken BEFORE the prompt was written, so the pane's echo of the prompt
    /// and its answer both count as new output.
    baseline: Vec<String>,
    /// Poll ticks remaining until the ask times out.
    ticks_left: u64,
}

#[derive(Default)]
struct Hub {
    /// channel name -> subscriber pipe ids (the UUIDs of `zellij pipe` CLIs).
    channels: HashMap<String, Vec<String>>,
    /// reply pipe id -> in-flight ask.
    pending_asks: HashMap<String, PendingAsk>,
}

impl Hub {
    fn reply(&self, pipe_id: &str, reply: Reply) {
        match serde_json::to_string(&reply) {
            Ok(json) => {
                cli_pipe_output(pipe_id, &format!("{json}\n"));
                unblock_cli_pipe_input(pipe_id);
            },
            Err(e) => {
                cli_pipe_output(
                    pipe_id,
                    &format!(r#"{{"ok":false,"error":"reply serialization failed: {e}"}}"#),
                );
                unblock_cli_pipe_input(pipe_id);
            },
        }
    }

    fn ack_without_unblock(&self, pipe_id: &str, reply: Reply) {
        if let Ok(json) = serde_json::to_string(&reply) {
            cli_pipe_output(pipe_id, &format!("{json}\n"));
        }
    }

    fn parse_pane_id(spec: &str) -> Result<PaneId, String> {
        spec.parse::<PaneId>().map_err(|_| {
            format!("malformed pane id '{spec}' (expected terminal_N, plugin_N, or a bare number)")
        })
    }

    fn handle(&mut self, pipe_id: &str, req: Request) {
        match req.cmd.as_str() {
            "send" => {
                let (channel, text) = match (req.channel, req.text) {
                    (Some(channel), Some(text)) => (channel, text),
                    _ => {
                        self.reply(
                            pipe_id,
                            Reply::err("'send' requires both 'channel' and 'text'"),
                        );
                        return;
                    },
                };
                let subscribers = self.channels.get(&channel).cloned().unwrap_or_default();
                let payload = format!(
                    "{}\n",
                    serde_json::json!({
                        "event": "channel",
                        "channel": channel,
                        "payload": text,
                    })
                );
                for sub in &subscribers {
                    cli_pipe_output(sub, &payload);
                }
                let reply = match subscribers.len() {
                    0 => Reply::ok("channel has no listeners", "channel", None),
                    n => Reply::ok(format!("delivered to {n} listener(s)"), "channel", None),
                };
                self.reply(pipe_id, reply);
            },
            "listen" => {
                let channel = match req.channel {
                    Some(c) => c,
                    None => {
                        self.reply(pipe_id, Reply::err("'listen' requires 'channel'"));
                        return;
                    },
                };
                let subscribers = self.channels.entry(channel.clone()).or_default();
                if !subscribers.iter().any(|s| s == pipe_id) {
                    subscribers.push(pipe_id.to_owned());
                }
                // Keep this pipe open after `pipe()` returns so the listener streams channel
                // events; without an explicit block the server auto-unblocks (pipes.rs
                // update_state_change) and the CLI exits before any payload arrives.
                block_cli_pipe_input(&pipe_id);
                let mut ack = Reply::ok("subscribed", "subscribed", None);
                ack.event = Some("ack".to_owned());
                self.ack_without_unblock(pipe_id, ack);
            },
            "unlisten" => {
                let channel = match req.channel {
                    Some(c) => c,
                    None => {
                        self.reply(pipe_id, Reply::err("'unlisten' requires 'channel'"));
                        return;
                    },
                };
                if let Some(subscribers) = self.channels.get_mut(&channel) {
                    subscribers.retain(|s| s != pipe_id);
                    if subscribers.is_empty() {
                        self.channels.remove(&channel);
                    }
                }
                self.reply(pipe_id, Reply::ok("unsubscribed", "unsubscribed", None));
            },
            "ask" => {
                let (target_spec, prompt) = match (req.target, req.prompt) {
                    (Some(t), Some(p)) => (t, p),
                    _ => {
                        self.reply(
                            pipe_id,
                            Reply::err("'ask' requires both 'target' and 'prompt'"),
                        );
                        return;
                    },
                };
                let target = match Self::parse_pane_id(&target_spec) {
                    Ok(id) => id,
                    Err(e) => {
                        self.reply(pipe_id, Reply::err(e));
                        return;
                    },
                };
                let timeout_ms = req.timeout_ms.unwrap_or(60_000);
                // Snapshot BEFORE delivering the prompt, so the echo + answer count as new
                // output; a baseline taken later would swallow fast replies and time out.
                let baseline = match get_pane_scrollback(target, true) {
                    Ok(contents) => pane_contents_to_lines(contents),
                    Err(_) => {
                        self.reply(pipe_id, Reply::err(format!("pane {target_spec} not found")));
                        return;
                    },
                };
                write_chars_to_pane_id(&prompt, target);
                let ticks = ((timeout_ms as f64) / (POLL_SECS * 1000.0)).ceil().max(1.0) as u64;
                self.pending_asks.insert(
                    pipe_id.to_owned(),
                    PendingAsk {
                        target,
                        baseline: baseline.clone(),
                        ticks_left: ticks,
                    },
                );
                // Keep the ask CLI's pipe open across the poll ticks; `reply()` (when the
                // answer or timeout lands) sends the explicit unblock that releases it.
                block_cli_pipe_input(&pipe_id);
                set_timeout(POLL_SECS);
            },
            "status" => {
                let target_spec = match req.target {
                    Some(t) => t,
                    None => {
                        self.reply(pipe_id, Reply::err("'status' requires 'target'"));
                        return;
                    },
                };
                match Self::parse_pane_id(&target_spec).and_then(|id| {
                    get_pane_info(id).ok_or_else(|| format!("pane {target_spec} not found"))
                }) {
                    Ok(info) => {
                        let reply = serde_json::json!({
                            "ok": true,
                            "reply_type": "status",
                            "pane_id": target_spec,
                            "title": info.title,
                            "is_focused": info.is_focused,
                            "is_floating": info.is_floating,
                            "exited": info.exited,
                        });
                        let _ = serde_json::to_string(&reply).map(|json| {
                            cli_pipe_output(pipe_id, &format!("{json}\n"));
                            unblock_cli_pipe_input(pipe_id);
                        });
                    },
                    Err(e) => self.reply(pipe_id, Reply::err(e)),
                }
            },
            "channels" => {
                let dump: Vec<serde_json::Value> = self
                    .channels
                    .iter()
                    .map(|(name, subs)| {
                        serde_json::json!({"channel": name, "subscribers": subs.len()})
                    })
                    .collect();
                let reply =
                    serde_json::json!({"ok": true, "reply_type": "channels", "channels": dump});
                let _ = serde_json::to_string(&reply).map(|json| {
                    cli_pipe_output(pipe_id, &format!("{json}\n"));
                    unblock_cli_pipe_input(pipe_id);
                });
            },
            other => {
                self.reply(pipe_id, Reply::err(format!("unknown command '{other}'")));
            },
        }
    }

    /// Poll tick: advance pending asks.
    fn tick(&mut self) {
        let mut done: Vec<(String, Reply)> = vec![];
        for (pipe_id, ask) in self.pending_asks.iter_mut() {
            match get_pane_scrollback(ask.target, true) {
                Err(_) => {
                    // Pane vanished mid-ask.
                    done.push((pipe_id.clone(), Reply::err("target pane no longer exists")));
                },
                Ok(contents) => {
                    let current = pane_contents_to_lines(contents);
                    let new_lines = new_lines_after(&ask.baseline, &current);
                    if !new_lines.is_empty() {
                        done.push((
                            pipe_id.clone(),
                            Reply::ok(new_lines.join("\n"), "output", None),
                        ));
                        continue;
                    }
                    if ask.ticks_left == 0 {
                        done.push((pipe_id.clone(), Reply::err("ask_timeout")));
                        continue;
                    }
                    ask.ticks_left -= 1;
                    // Slide the baseline so each poll only sees genuinely new output.
                    ask.baseline = current;
                },
            }
        }
        for (pipe_id, reply) in done {
            self.pending_asks.remove(&pipe_id);
            self.reply(&pipe_id, reply);
        }
        if !self.pending_asks.is_empty() {
            set_timeout(POLL_SECS);
        }
    }
}

fn pane_contents_to_lines(contents: PaneContents) -> Vec<String> {
    // History is truncated at the head; the newest output lives at the bottom of history or in
    // the viewport, so concatenating history + viewport keeps the tail stable across polls.
    let mut lines: Vec<String> = contents.lines_above_viewport;
    lines.extend(contents.viewport);
    if lines.len() > MAX_POLL_LINES {
        // Keep the NEWEST lines (the tail): history grows at the bottom.
        let start = lines.len() - MAX_POLL_LINES;
        lines.drain(..start);
    }
    lines
}

/// Return the lines that appear in `current` but not in `baseline`, assuming only
/// head-truncation (scrollback overflow) and append (new output) can happen between snapshots.
fn new_lines_after(baseline: &[String], current: &[String]) -> Vec<String> {
    // Longest suffix of `baseline` that is a prefix of `current` = the overlap.
    let max = baseline.len().min(current.len());
    let mut overlap = 0;
    for k in (0..=max).rev() {
        if baseline[baseline.len() - k..] == current[..k] {
            overlap = k;
            break;
        }
    }
    current[overlap..].to_vec()
}

impl ZellijPlugin for Hub {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::ReadCliPipes,
            PermissionType::WriteToStdin,
            PermissionType::ReadPaneContents,
            // get_pane_info (status) requires this; without it the shim panics and kills the
            // plugin instance (blocked ask/listen pipes then never unblock).
            PermissionType::ReadApplicationState,
        ]);
        // Timer events (from set_timeout) are only delivered to subscribed plugins
        // (wasm_bridge update_plugins gates on the subscription set); without this the
        // ask poll loop never ticks.
        subscribe(&[EventType::Timer]);
    }

    fn update(&mut self, event: Event) -> bool {
        if let Event::Timer(_) = event {
            self.tick();
        }
        false
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        let pipe_id = match &pipe_message.source {
            PipeSource::Cli(pipe_id) => pipe_id.clone(),
            // Only CLI pipes are part of the zjw protocol; plugin-to-plugin messages are ignored.
            _ => return false,
        };
        let payload = match &pipe_message.payload {
            Some(p) => p.clone(),
            None => {
                self.reply(
                    &pipe_id,
                    Reply::err("empty payload (expected a JSON request)"),
                );
                return false;
            },
        };
        let req: Request = match serde_json::from_str(&payload) {
            Ok(req) => req,
            Err(e) => {
                self.reply(&pipe_id, Reply::err(format!("malformed request: {e}")));
                return false;
            },
        };
        self.handle(&pipe_id, req);
        false
    }

    fn render(&mut self, _rows: usize, _cols: usize) {}
}

register_plugin!(Hub);
