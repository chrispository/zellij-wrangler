## Zellij Wrangler: agent-aware pane prompting

This fork adds `pane-comms`, a small companion CLI and Zellij hub plugin for communication
between terminal panes and the LLM agents running in them. It works across panes and tabs on
stock Zellij, without depending on fixed pane numbers or tab names.

`zjw` discovers agents from each pane's live command and title, so prompts can use a friendly
agent name:

```sh
zjw targets
zjw agents
zjw send codex $'Please review the failing test.\n'
zjw ask opencode $'What are you working on?\n'
zjw send other:codex $'Coordinate with the other Codex pane.\n'
```

You do not have to run `zjw` yourself. After the pane-comms components and the
`zellij-wrangler` skill are installed, just tell an agent what you want in plain language:
“ask Codex to review this,” “coordinate with OpenCode,” or “send this to the other Claude.” The
agent uses the skill to discover the right pane, invoke the communication tools, and ask you to
choose when multiple matching sessions exist.

The final newline is converted into an explicit Zellij `Enter` key action. This matters for
full-screen agent TUIs such as Codex, where writing a raw line feed may display the text without
submitting it. Without a trailing newline, `zjw send` types text but leaves submission to you.

Built-in profiles recognize `claude`, `codex`, `antigravity`, `opencode` (including `opencode2+`),
`crush`, `pi`, `omp`, `hermes`, `vibe`, and `z-code`/`zcode`. Profiles are matched by command or
title at send time; `Tab #1`, a renamed tab, and pane numbering do not matter.

If more than one pane matches an agent, `zjw` refuses to guess and reports each candidate's pane,
tab, working directory, and command. Ask which one to use, then send to its concrete pane id.
`other:NAME` excludes the calling pane, which is useful when two agents of the same type are
running.

### Defining additional agents

Add custom profiles in `$XDG_CONFIG_HOME/pane-comms/agents.toml` or
`~/.config/pane-comms/agents.toml`. Use `ZJW_AGENTS_CONFIG` to point to another file:

- `commands` is the executable name Zellij reports for the pane. It is usually the first word
  in the `command` field from `zjw agents --json`, such as `opencode2` or
  `codex`—not a shell alias.
- `aliases` are additional names you can use when asking an agent. The profile name itself also
  works, so this example can be addressed as `codex` or `backend-codex`.
- `titles` contains visible terminal-title text that identifies the agent when its command is
  hidden behind a shell or wrapper. It is optional; command matching is usually best.

```toml
[agents.codex]
# The executable shown in the `command` field from `zjw agents --json`.
commands = ["codex"]
# Names users can say in addition to the profile name `codex`.
aliases = ["backend-codex"]
# Optional visible terminal title used to recognize the pane.
titles = ["Backend Codex"]

[agents.opencode]
commands = ["opencode2"]
aliases = ["oc"]
```

Entries named after a built-in profile extend it; other entries create new profiles. Use
`zjw agents --json` to see the command and title Zellij is reporting for each discovered pane. The companion
CLI, hub, and agent skill are in [`pane-comms/`](./pane-comms/), including build instructions,
permissions, layouts, and the end-to-end test suite. The skill is
[`pane-comms/skills/zellij-wrangler/SKILL.md`](./pane-comms/skills/zellij-wrangler/SKILL.md).

For pane reading, `zjw read` defaults to the newest 200 lines so routine requests do not pull a
whole scrollback into an agent's context. The default is defined by `DEFAULT_READ_LINES` in
`pane-comms/zjw/src/main.rs`; use `zjw read <target> --lines N` for a one-off size and
`--offset 200` or `--offset 400` to page backward when recent context is unclear. Full history
should be reserved for an explicit request.

### Build and install

From a checkout of this repository, build the hub and companion CLI once:

```sh
cd pane-comms
rustup target add wasm32-wasip1
export CARGO_TARGET_DIR="$PWD/target"
cargo build -p hub --target wasm32-wasip1 --release
cargo build -p zjw

mkdir -p "$HOME/.local/bin" "$HOME/.local/share/zellij-wrangler"
install -m 755 target/debug/zjw "$HOME/.local/bin/zjw"
install -m 644 target/wasm32-wasip1/release/hub.wasm \
  "$HOME/.local/share/zellij-wrangler/hub.wasm"
```

Put `~/.local/bin` on your `PATH` if it is not already there (for example, add
`export PATH="$HOME/.local/bin:$PATH"` to `~/.bashrc` or `~/.zshrc`). No shell alias is needed:
the agent skill invokes `zjw` for you. Install the skill links as described in the full
[`pane-comms` README](./pane-comms/README.md), then restart already-running agents so they load
it.

## Original README below

<h1 align="center">
  <br>
  <img src="https://raw.githubusercontent.com/zellij-org/zellij/main/assets/logo.png" alt="logo" width="200">
  <br>
  Zellij
  <br>
  <br>
</h1>

<p align="center">
  <img src="https://raw.githubusercontent.com/zellij-org/zellij/main/assets/demo.gif" alt="demo">
</p>
<h4 align="center">
  [<a href="https://zellij.dev/documentation/installation">Installation</a>]
  [<a href="https://zellij.dev/screencasts/">Screencasts & Tutorials</a>]
  [<a href="https://zellij.dev/documentation/configuration">Configuration</a>]
  [<a href="https://zellij.dev/documentation/layouts">Layouts</a>]
  [<a href="https://zellij.dev/documentation/faq">FAQ</a>]
</h4>
<p align="center">
  <a href="https://discord.gg/CrUAFH3"><img alt="Discord Chat" src="https://img.shields.io/discord/771367133715628073?color=5865F2&label=discord&style=flat-square"></a>
  <a href="https://matrix.to/#/#zellij_general:matrix.org"><img alt="Matrix Chat" src="https://img.shields.io/matrix/zellij_general:matrix.org?color=1d7e64&label=matrix%20chat&style=flat-square&logo=matrix"></a>
  <a href="https://zellij.dev/documentation/"><img alt="Zellij documentation" src="https://img.shields.io/badge/zellij-documentation-fc0060?style=flat-square"></a>
</p>

<br>
    <p align="center">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://github.com/user-attachments/assets/bc5daac4-140a-4b83-8729-71c944ee1100">
      <img src="https://github.com/user-attachments/assets/55156624-a71a-46b5-939e-f562e3b2dd7f" alt="Sponsored by ">
    </picture>
    &nbsp;
    &nbsp;
    <a href="https://www.gresearch.com/">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="https://github.com/user-attachments/assets/d609936a-abf8-4406-8cfc-889f76a09d74">
          <img src="https://github.com/user-attachments/assets/742ae902-fe9d-41c6-baf2-4bc143061da3" alt="gresearch logo">
        </picture>
    </a>
</p>

# What is this?

[Zellij](#origin-of-the-name) is a workspace aimed at developers, ops-oriented people and anyone who loves the terminal. Similar programs are sometimes called "Terminal Multiplexers".

Zellij is designed around the philosophy that one must not sacrifice simplicity for power, taking pride in its great experience out of the box as well as the advanced features it places at its users' fingertips.

Zellij is geared toward beginner and power users alike - allowing deep customizability, personal automation through [layouts](https://zellij.dev/documentation/layouts.html), true multiplayer collaboration, unique UX features such as floating and stacked panes, and a [plugin system](https://zellij.dev/documentation/plugins.html) allowing one to create plugins in any language that compiles to WebAssembly.

Zellij includes a built-in [web-client](https://zellij.dev/tutorials/web-client/), making a terminal optional.

You can get started by [installing](https://zellij.dev/documentation/installation.html) Zellij and checking out the [Screencasts & Tutorials](https://zellij.dev/screencasts/).

For more details about our future plans, read about upcoming features in our [roadmap](#roadmap).

## How do I install it?

The easiest way to install Zellij is through a [package for your OS](./docs/THIRD_PARTY_INSTALL.md).

If one is not available for your OS, you could download a prebuilt binary from the [latest release](https://github.com/zellij-org/zellij/releases/latest) and place it in your `$PATH`. If you'd like, we could [automatically choose one for you](#try-zellij-without-installing).

You can also install (compile) with `cargo`:

```
cargo install --locked zellij
```

#### Try Zellij without installing

bash/zsh:
```bash
bash <(curl -L https://zellij.dev/launch)
```
fish/xonsh:
```bash
bash -c 'bash <(curl -L https://zellij.dev/launch)'
```

#### Installing from `main`
Installing Zellij from the `main` branch is not recommended. This branch represents pre-release code, is constantly being worked on and may contain broken or unusable features. In addition, using it may corrupt the cache for future versions, forcing users to clear it before they can use the officially released version.

That being said - no-one will stop you from using it (and bug reports involving new features are greatly appreciated), but please consider using the latest release instead as detailed at the top of this section.

## How do I start a development environment?

* Clone the project
* In the project folder, for debug builds run: `cargo xtask run`
* To run all tests: `cargo xtask test`

For more build commands, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Configuration
For configuring Zellij, please see the [Configuration Documentation](https://zellij.dev/documentation/configuration.html).

## About issues in this repository
Issues in this repository, whether open or closed, do not necessarily indicate a problem or a bug in the software. They only indicate that the reporter wanted to communicate their experiences or thoughts to the maintainers. The Zellij maintainers do their best to go over and reply to all issue reports, but unfortunately cannot promise these will always be dealt with or even read. Your understanding is appreciated.

## Roadmap
Presented here is the project roadmap, divided into three main sections.

These are issues that are either being actively worked on or are planned for the near future.

***If you'll click on the image, you'll be led to an SVG version of it on the website where you can directly click on every issue***

[![roadmap](https://github.com/user-attachments/assets/bb55d213-4a68-4c84-ae72-7db5c9bf94fb)](https://zellij.dev/roadmap)

## Origin of the Name
[From Wikipedia, the free encyclopedia](https://en.wikipedia.org/wiki/Zellij)

Zellij (Arabic: الزليج, romanized: zillīj; also spelled zillij or zellige) is a style of mosaic tilework made from individually hand-chiseled tile pieces. The pieces were typically of different colours and fitted together to form various patterns on the basis of tessellations, most notably elaborate Islamic geometric motifs such as radiating star patterns composed of various polygons. This form of Islamic art is one of the main characteristics of architecture in the western Islamic world. It is found in the architecture of Morocco, the architecture of Algeria, early Islamic sites in Tunisia, and in the historic monuments of al-Andalus (in the Iberian Peninsula).

## License

MIT

## Sponsored by
<a href="https://terminaltrove.com/"><img src="https://avatars.githubusercontent.com/u/121595180?s=200&v=4" width="80px"></a>
