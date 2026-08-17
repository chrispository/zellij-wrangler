#!/usr/bin/env bash
# pane-comms end-to-end test.
#
# Spawns a dedicated zellij session (never touches your other sessions), loads the hub,
# and exercises the full pz surface: send, ask, wait, listen, status, targets, cross-tab,
# tab-name resolution, and the documented exit codes.
#
# Requires: zellij, cargo, wasm32-wasip1 target, `script`.
set -u

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SESSION="${PZ_TEST_SESSION:-pztest}"
CACHE_DIR="${PZ_TEST_CACHE:-/tmp/pztest-cache}"
CONFIG="$REPO/layouts/e2e-config.kdl"
LAYOUT="$REPO/layouts/e2e.kdl"
# Pin the target dir: the parent zellij repo's .cargo/config.toml redirects artifacts, which
# would split pz/hub across directories.
export CARGO_TARGET_DIR="$REPO/target"
# zellij ignores a ZELLIJ_CACHE_DIR env var; its cache dir is the XDG cache dir (via the
# `directories` crate). Redirecting XDG_CACHE_HOME isolates this test session's permissions.kdl
# and plugin cache from the user's real zellij state.
export XDG_CACHE_HOME="$CACHE_DIR"
PZ="$CARGO_TARGET_DIR/debug/pz"
HUB_WASM="$CARGO_TARGET_DIR/wasm32-wasip1/release/hub.wasm"

PASS=0
FAIL=0
fail() { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }
ok() { PASS=$((PASS + 1)); }

check() { # check <desc> <expected-exit> cmd...
    local desc="$1" want="$2"
    shift 2
    "$@" >/tmp/pztest-out 2>/tmp/pztest-err
    local got=$?
    if [ "$got" -eq "$want" ]; then ok; echo "ok: $desc (exit $got)"; else
        fail "$desc: expected exit $want, got $got"; sed 's/^/    /' /tmp/pztest-err | head -3
    fi
}

echo "==> pane-comms E2E (session=$SESSION, repo=$REPO)"

# --- build -------------------------------------------------------------------------------
echo "==> building hub + pz"
cargo build --manifest-path "$REPO/Cargo.toml" -p hub --target wasm32-wasip1 --release || { echo "hub build failed (is the wasm32-wasip1 target installed?)"; exit 1; }
cargo build --manifest-path "$REPO/Cargo.toml" -p pz || { echo "pz build failed"; exit 1; }

HUB_URL="file://$HUB_WASM"
echo "==> hub: $HUB_URL"

# --- fresh session with a clean cache dir + pre-granted permissions ----------------------
# kill (if alive) then delete (removes the lingering dead session entry) so the name is reusable.
zellij kill-session "$SESSION" 2>/dev/null
zellij delete-session "$SESSION" 2>/dev/null
rm -rf "$CACHE_DIR"
mkdir -p "$CACHE_DIR"
# The `directories` crate appends /zellij to the XDG cache dir, so permissions.kdl lives at
# $XDG_CACHE_HOME/zellij/permissions.kdl. Keys are the plugin's stored location: a plain path
# for file:// URLs.
mkdir -p "$CACHE_DIR/zellij"
cat > "$CACHE_DIR/zellij/permissions.kdl" <<EOF
"$HUB_WASM" {
    ReadCliPipes
    WriteToStdin
    ReadPaneContents
    ReadApplicationState
}
EOF

echo "==> starting session"
( script -qec "zellij --session $SESSION --config $CONFIG --new-session-with-layout $LAYOUT" /dev/null >/tmp/pztest-client.log 2>&1 & )
for _ in $(seq 1 40); do
    zellij ls 2>/dev/null | grep -q "^$SESSION " && break
    sleep 0.25
done
sleep 1

zellij --session "$SESSION" action list-panes --json > /tmp/pztest-panes.json || { fail "session did not come up"; exit 1; }
PANE_A=$(jq -r '[.[] | select(.is_plugin == false and .tab_id == 0)][0] | "terminal_\(.id)"' /tmp/pztest-panes.json)
PANE_B=$(jq -r '[.[] | select(.is_plugin == false and .tab_id == 0)][1] | "terminal_\(.id)"' /tmp/pztest-panes.json)
PANE_C=$(jq -r '[.[] | select(.is_plugin == false and .tab_id == 1)][0] | "terminal_\(.id)"' /tmp/pztest-panes.json)
if [ "$PANE_A" = "null" ] || [ "$PANE_B" = "null" ] || [ "$PANE_C" = "null" ]; then
    fail "layout did not produce 2 panes in tab 0 + 1 in tab 1 (got $PANE_A / $PANE_B / $PANE_C)"
    exit 1
fi
echo "==> panes: $PANE_A (tab0) / $PANE_B (tab0) / $PANE_C (tab1 work)"

export PZ_HUB_URL="$HUB_URL"

# --- M1 baseline: write-chars + dump-screen + subscribe (cross-tab included) --------------
check "write-chars to $PANE_A" 0 "$PZ" --session "$SESSION" send "$PANE_A" "marker-A"
sleep 0.5
if zellij --session "$SESSION" action dump-screen --pane-id "$PANE_A" | grep -q "marker-A"; then ok; echo "ok: dump-screen round-trips marker-A"; else fail "dump-screen missing marker-A"; fi

check "write-chars cross-tab to $PANE_C" 0 "$PZ" --session "$SESSION" send "$PANE_C" "cross-tab-marker"
sleep 0.5
if zellij --session "$SESSION" action dump-screen --pane-id "$PANE_C" | grep -q "cross-tab-marker"; then ok; echo "ok: dump-screen cross-tab"; else fail "dump-screen cross-tab missing marker"; fi

# --- target resolution --------------------------------------------------------------------
check "send by tab:1 (active pane of tab 1)" 0 "$PZ" --session "$SESSION" send "tab:1" "tab-resolved-marker"
check "send by tab-name:work" 0 "$PZ" --session "$SESSION" send "tab-name:work" "tabname-resolved-marker"
sleep 0.5
if zellij --session "$SESSION" action dump-screen --pane-id "$PANE_C" | grep -q "tabname-resolved-marker"; then ok; echo "ok: tab-name:work resolved to $PANE_C"; else fail "tab-name:work marker missing"; fi

check "send to missing pane exits 2" 2 "$PZ" --session "$SESSION" send terminal_999 "nope"
check "send to unknown target exits 2" 2 "$PZ" --session "$SESSION" send "bogus" "nope"
check "send to unknown tab-name exits 2" 2 "$PZ" --session "$SESSION" send "tab-name:nope" "nope"
check "send to empty tab exits 2" 2 "$PZ" --session "$SESSION" send "tab:99" "nope"
check "wait missing pane exits 2" 2 "$PZ" --session "$SESSION" wait terminal_999 --until x --timeout 500

# --- channels (M3): listen + send --channel ----------------------------------------------
echo "==> starting listeners"
"$PZ" --session "$SESSION" listen demo > /tmp/pztest-listen-raw.out 2>/tmp/pztest-listen-raw.err &
LISTEN_RAW=$!
"$PZ" --session "$SESSION" listen demo --format json > /tmp/pztest-listen-json.out 2>/tmp/pztest-listen-json.err &
LISTEN_JSON=$!
sleep 2

check "send --channel demo" 0 "$PZ" --session "$SESSION" send --channel demo "hello listeners"
sleep 1.5
if grep -q "^hello listeners$" /tmp/pztest-listen-raw.out; then ok; echo "ok: raw listener received payload"; else fail "raw listener missing payload"; cat /tmp/pztest-listen-raw.err; fi
if grep -q '"event":"pipe_output"' /tmp/pztest-listen-json.out && grep -q '"output":"hello listeners"' /tmp/pztest-listen-json.out; then ok; echo "ok: json listener wrapped payload"; else fail "json listener payload"; cat /tmp/pztest-listen-json.out; fi

kill $LISTEN_RAW $LISTEN_JSON 2>/dev/null
wait $LISTEN_RAW $LISTEN_JSON 2>/dev/null

# --- wait (M4) ----------------------------------------------------------------------------
# The initial snapshot is the baseline (pre-existing text does not match); the marker must be
# sent AFTER the subscription is live, hence the backgrounded delayed send.
expect_wait_match() { # desc until-pattern pane marker
    local desc="$1" until="$2" pane="$3" marker="$4"
    ( sleep 1.5; "$PZ" --session "$SESSION" send "$pane" "$marker" >/dev/null 2>&1 ) &
    check "$desc" 0 "$PZ" --session "$SESSION" wait "$pane" --until "$until" --timeout 8000
}
expect_wait_match "wait until substring matches" READY-NOW "$PANE_A" $'\necho READY-NOW\n'
# A regex anchored at line start can only match real command output, not the prompt line, so
# the marker is delivered as `echo ...` and matched against its output. ANSI-C quoting keeps
# the newlines: $(printf '...\n') would strip trailing newlines (command substitution). The
# leading newline submits any partially-typed line left by earlier raw writes, so the echo
# command runs on a clean prompt instead of merging into a garbage command.
expect_wait_match "wait until regex matches" "/^READY-REGEX/" "$PANE_A" $'\necho READY-REGEX\n'
check "wait timeout exits 1" 1 "$PZ" --session "$SESSION" wait "$PANE_A" --until NEVER-MATCHES --timeout 1200
check "wait pre-existing text does not match (exit 1)" 1 "$PZ" --session "$SESSION" wait "$PANE_A" --until marker-A --timeout 1000

# --- ask (hub-mediated prompt + wait-for-output) ------------------------------------------
check "ask returns new output" 0 "$PZ" --session "$SESSION" ask "$PANE_A" "echo ASK-REPLY-$RANDOM
" --timeout 8000
if grep -q "ASK-REPLY" /tmp/pztest-out; then ok; echo "ok: ask reply contains output"; else fail "ask reply missing output"; cat /tmp/pztest-out; fi
check "ask to missing pane exits 2" 2 "$PZ" --session "$SESSION" ask terminal_999 "x
" --timeout 1000
# The tty driver echoes typed input even while a foreground job runs, so a plain busy pane
# still produces new output. `stty -echo` first, then sleep: typed input is not echoed ->
# no new output -> ask times out.
"$PZ" --session "$SESSION" send "$PANE_B" "stty -echo; sleep 8
" >/dev/null
sleep 0.7
check "ask timeout exits 3" 3 "$PZ" --session "$SESSION" ask "$PANE_B" "x
" --timeout 1200

# --- status -------------------------------------------------------------------------------
check "status ok" 0 "$PZ" --session "$SESSION" status "$PANE_A"
if grep -q '"ok": true' /tmp/pztest-out; then ok; else fail "status not ok: $(cat /tmp/pztest-out)"; fi
check "status unknown pane exits 4" 4 "$PZ" --session "$SESSION" status terminal_999

# --- targets ------------------------------------------------------------------------------
check "targets lists panes" 0 "$PZ" --session "$SESSION" targets
check "targets --json parses" 0 "$PZ" --session "$SESSION" targets --json

# --- cleanup ------------------------------------------------------------------------------
zellij kill-session "$SESSION" 2>/dev/null
echo
echo "==> PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ]
