#!/usr/bin/env bash
# End-to-end suite: the plugin against a real Herdr server and a fake Claude Code.
#
# Runs inside the image built by tests/e2e/Dockerfile and never touches a real Herdr
# session: every scenario gets its own throw-away HOME and its own headless server.
#
# Output: one PASS/FAIL line per check, one summary line per scenario, then either
# "ALL TESTS OK" or "<n> FAILURE(S)". The exit code is the number of unexpected failures.
# Scenarios listed in KNOWN_FAILING are expected to fail: they are reported in their own
# section and do not change the exit code (an unexpected pass is reported too).
#
# Usage: run.sh [scenario-id]    (for example: run.sh s3)

set -u

PLUGIN_ID="pepsit36.herdr-sleep"
PLUGIN_DIR="${PLUGIN_DIR:-/opt/herdr-sleep}"
HS="$PLUGIN_DIR/bin/herdr-sleep"
SLEEP_ICON="💤"
KEEP_ICON="🔒"
ONLY="${1:-}"

# Scenarios known to fail against a real Herdr: "id" -> one-line reason.
declare -A KNOWN_FAILING=()

# ---------------------------------------------------------------------------
# Bookkeeping
# ---------------------------------------------------------------------------

FAILURES=0
declare -a UNEXPECTED=() KNOWN_HIT=() KNOWN_PASSED=() PASSED=()
CURRENT_BAD=0

ok() { echo "  PASS  $*"; }
bad() {
  echo "  FAIL  $*"
  CURRENT_BAD=$((CURRENT_BAD + 1))
}
check() { # check "description" command...
  local what="$1"
  shift
  if "$@" >/dev/null 2>&1; then ok "$what"; else bad "$what"; fi
}
note() { echo "        $*"; }

# ---------------------------------------------------------------------------
# Environment of one scenario
# ---------------------------------------------------------------------------

AGENT_PREFIX="" # environment the fake agent is started with, for example "FAKE_AGENT_STATUS_LINES=16 "
ROOT=""
SERVER_LOG=""
STATE_DIR=""  # plugin state dir: the journal lives here
CONFIG_DIR=""
PANE_A=""     # pane of the agent "alpha"
PANE_OTHER=""
WS_A=""
WS_B=""
SID=""
QUICK=0 # 1 when the Watcher sleeps idle agents at once (sleep_after_minutes = 0)

hs() { # the plugin binary, with the environment Herdr gives to the plugin commands
  HERDR_BIN_PATH="$(command -v herdr)" \
    HERDR_PLUGIN_ID="$PLUGIN_ID" \
    HERDR_PLUGIN_ROOT="$PLUGIN_DIR" \
    HERDR_PLUGIN_STATE_DIR="$STATE_DIR" \
    HERDR_PLUGIN_CONFIG_DIR="$CONFIG_DIR" \
    "$HS" "$@"
}

# wait_for <seconds> <command...>: poll every half second until the command succeeds.
wait_for() {
  local limit="$1" deadline
  shift
  deadline=$((SECONDS + limit))
  while ((SECONDS < deadline)); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  "$@" >/dev/null 2>&1
}

# The agent of a pane as JSON ({} when there is none).
agent_json() {
  herdr agent list 2>/dev/null |
    jq -c --arg p "$1" '[.result.agents[]? | select(.pane_id == $p)][0] // {}'
}
agent_kind() { agent_json "$1" | jq -r '.agent // ""'; }
agent_name() { agent_json "$1" | jq -r '.name // ""'; }
agent_status() { agent_json "$1" | jq -r '.agent_status // ""'; }
agent_session() { agent_json "$1" | jq -r '.agent_session.value // ""'; }

is_asleep() { [ "$(agent_kind "$1")" = "sleeping-claude" ]; }
is_awake_idle() { [ "$(agent_kind "$1")" = "claude" ] && [ "$(agent_status "$1")" = "idle" ]; }
# Idle for three seconds in a row: right after it starts, Herdr's status of the agent can
# still flip between unknown and idle while its screen detection settles.
steady_idle() {
  local n=0
  while ((n < 6)); do
    is_awake_idle "$1" || return 1
    n=$((n + 1))
    sleep 0.5
  done
}
no_agent() { [ -z "$(agent_kind "$1")" ]; }
restore_ran_twice() { [ "$(grep -c RESTORE-START "$STATE_DIR/herdr-sleep.log" 2>/dev/null)" -ge 2 ]; }
is_named() { [ "$(agent_name "$1")" = "$2" ]; }

screen() { herdr pane read "$1" --source recent --lines 100 2>/dev/null; }
screen_has() { screen "$1" | grep -qF -- "$2"; }

# Pid of the Placeholder serving a pane, empty when none.
placeholder_pid() { pgrep -u "$(id -u)" -f "herdr-sleep-ph --pane $1 " | head -n 1; }
rss_kb() { awk '/^VmRSS:/ {print $2}' "/proc/$1/status" 2>/dev/null; }

tab_label() { herdr tab list 2>/dev/null | jq -r --arg t "$1" '.result.tabs[]? | select(.tab_id == $t) | .label'; }
tab_prefixed() { [[ "$(tab_label "$1")" == "$SLEEP_ICON"* ]]; }
tab_not_prefixed() { ! tab_prefixed "$1"; }
pane_tab() { herdr pane get "$1" 2>/dev/null | jq -r '.result.pane.tab_id'; }
watcher_running() { hs status --json 2>/dev/null | jq -e '.watcher.running' >/dev/null; }

journal_tail() {
  if [ -f "$STATE_DIR/herdr-sleep.log" ]; then
    echo "        --- last lines of herdr-sleep.log ---"
    tail -n 8 "$STATE_DIR/herdr-sleep.log" | sed 's/^/        /'
  else
    echo "        (no herdr-sleep.log in $STATE_DIR)"
  fi
}

server_up() { herdr status server 2>/dev/null | grep -q '^status: running'; }
server_down() { ! server_up; }

start_server() {
  (herdr server >>"$SERVER_LOG" 2>&1 &)
  wait_for 30 server_up
}

stop_server() {
  herdr server stop >/dev/null 2>&1
  wait_for 20 server_down
}

# Herdr resumes restored agent panes only once a client attached and gave its terminal size,
# so the restart scenario attaches a real client (the TUI) in a pseudo terminal.
attach_client() {
  (sleep 600 | script -qfc "stty cols 120 rows 40; herdr" /dev/null >/dev/null 2>&1 &)
}

# start_agent <pane> <name>: run the fake claude there, wait until Herdr sees it idle, name it.
start_agent() {
  local pane="$1" name="$2"
  herdr pane run "$pane" "${AGENT_PREFIX}claude" >/dev/null 2>&1
  local ready=steady_idle
  # With sleep_after_minutes = 0 the Watcher would put the agent to sleep during the steady wait.
  [ "$QUICK" = 1 ] && ready=is_awake_idle
  if ! wait_for 60 "$ready" "$pane"; then
    bad "agent in $pane never became claude/idle (kind=$(agent_kind "$pane") status=$(agent_status "$pane"))"
    return 1
  fi
  herdr agent rename "$pane" "$name" >/dev/null 2>&1
  if ! wait_for 10 is_named "$pane" "$name"; then
    bad "agent in $pane could not be renamed to $name"
    return 1
  fi
}

# new_world <config.toml body>: a fresh HOME and config, the plugin linked, a running server
# with two workspaces (the second focused) and "alpha" idle in a pane of the first.
new_world() {
  ROOT="$(mktemp -d /tmp/e2e.XXXXXX)"
  export HOME="$ROOT/home"
  mkdir -p "$HOME/.config/herdr"
  SERVER_LOG="$ROOT/server.log"
  STATE_DIR="$HOME/.local/state/herdr/plugins/$PLUGIN_ID"
  CONFIG_DIR="$HOME/.config/herdr/plugins/config/$PLUGIN_ID"
  cat >"$HOME/.config/herdr/config.toml" <<'EOF'
[session]
resume_agents_on_restore = false
EOF
  mkdir -p "$CONFIG_DIR" "$STATE_DIR"
  printf '%s\n' "$1" >"$CONFIG_DIR/config.toml"
  QUICK=0
  [ "$1" = "$CONFIG_SWEEP" ] && QUICK=1
  # Linked before the server starts, so that the startup hook starts the Watcher.
  if ! herdr plugin link "$PLUGIN_DIR" >"$ROOT/link.log" 2>&1; then
    bad "herdr plugin link failed: $(head -c 300 "$ROOT/link.log")"
    return 1
  fi
  if ! start_server; then
    bad "the Herdr server did not start: $(tail -n 3 "$SERVER_LOG")"
    return 1
  fi
  local created
  created="$(herdr workspace create --label one --cwd "$HOME" --no-focus)"
  WS_A="$(jq -r '.result.workspace.workspace_id' <<<"$created")"
  PANE_A="$(jq -r '.result.root_pane.pane_id' <<<"$created")"
  created="$(herdr workspace create --label two --cwd "$HOME" --focus)"
  WS_B="$(jq -r '.result.workspace.workspace_id' <<<"$created")"
  PANE_OTHER="$(jq -r '.result.root_pane.pane_id' <<<"$created")"
  start_agent "$PANE_A" alpha || return 1
  SID="$(agent_session "$PANE_A")"
  [ -n "$SID" ] || bad "the fake agent reported no session id"
}

end_world() {
  stop_server
  pkill -u "$(id -u)" -x script 2>/dev/null
  pkill -u "$(id -u)" -x herdr-sleep-ph 2>/dev/null
  pkill -u "$(id -u)" -x claude 2>/dev/null
  pkill -u "$(id -u)" -f "herdr-sleep watch" 2>/dev/null
  pkill -u "$(id -u)" -x herdr 2>/dev/null
  sleep 0.3
  rm -rf "$ROOT"
}

# Manual sleep of alpha, then wait for the Sleeping agent and its Placeholder.
sleep_alpha() {
  local out
  out="$(hs sleep alpha --json 2>&1)"
  if ! wait_for 30 is_asleep "$PANE_A"; then
    bad "alpha did not go to sleep (kind=$(agent_kind "$PANE_A")); sleep said: $out"
    return 1
  fi
  wait_for 10 placeholder_pid "$PANE_A"
}

# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------

CONFIG_SWEEP='sleep_after_minutes = 0
sweep_interval_seconds = 5'
# An idle time that is never reached: only explicit commands put an agent to sleep.
CONFIG_MANUAL='sleep_after_minutes = 600
sweep_interval_seconds = 5'

s1_sleep_by_sweep() {
  new_world "$CONFIG_SWEEP" || return
  # The real guards run; the first sweeps may only start the idle clock.
  local i
  for i in 1 2 3 4 5 6; do
    hs sweep --json >/dev/null 2>&1
    is_asleep "$PANE_A" && break
    sleep 1
  done
  if wait_for 20 is_asleep "$PANE_A"; then ok "alpha is sleeping-claude after the sweep"; else bad "alpha is not sleeping-claude (kind=$(agent_kind "$PANE_A"))"; fi
  if wait_for 15 is_named "$PANE_A" alpha; then ok "the name alpha is kept"; else bad "the name is '$(agent_name "$PANE_A")', expected alpha"; fi
  if [ "$(agent_status "$PANE_A")" = "idle" ]; then ok "the Sleeping agent is idle"; else bad "status is '$(agent_status "$PANE_A")'"; fi
  local pid
  pid="$(placeholder_pid "$PANE_A")"
  if [ -n "$pid" ]; then
    ok "the Placeholder (pid $pid) is in the pane"
    local rss server shells
    rss="$(rss_kb "$pid")"
    note "Placeholder VmRSS: ${rss:-?} kB"
    if [ -n "$rss" ] && [ "$rss" -lt 3000 ]; then ok "VmRSS $rss kB is under 3000 kB"; else bad "VmRSS '${rss:-?}' kB is not under 3000 kB"; fi
    if [ "$(ps -o comm= -p "$pid")" = "herdr-sleep-ph" ]; then ok "ps shows herdr-sleep-ph as the pane process"; else bad "ps comm is '$(ps -o comm= -p "$pid")'"; fi
    if [ -z "$(pgrep -P "$pid")" ]; then ok "the Placeholder has no child process"; else bad "the Placeholder has children: $(pgrep -P "$pid" | tr '\n' ' ')"; fi
    server="$(pgrep -u "$(id -u)" -x herdr | head -n 1)"
    shells="$(ps -o comm= --ppid "$server" | grep -c '^bash$')"
    # Two panes: the other workspace keeps its shell, the sleeping pane has none left.
    if [ "$shells" = "1" ]; then ok "no shell is left in the sleeping pane"; else bad "$shells shells under the server, expected 1"; fi
  else
    bad "no Placeholder process found for $PANE_A"
  fi
  if pgrep -u "$(id -u)" -x claude >/dev/null; then bad "a claude process is still running"; else ok "the agent process is gone"; fi
  if watcher_running; then ok "the Watcher runs"; else bad "the Watcher is not running"; fi
}

s2_wake_by_enter() {
  new_world "$CONFIG_MANUAL" || return
  sleep_alpha || return
  herdr pane send-keys "$PANE_A" enter >/dev/null 2>&1
  if wait_for 40 is_awake_idle "$PANE_A"; then ok "the agent is back as claude"; else bad "no wake: kind=$(agent_kind "$PANE_A") status=$(agent_status "$PANE_A")"; fi
  if [ "$(agent_session "$PANE_A")" = "$SID" ]; then ok "same session id ($SID)"; else bad "session id is '$(agent_session "$PANE_A")', expected $SID"; fi
  if wait_for 20 is_named "$PANE_A" alpha; then ok "the name alpha is given back"; else bad "the name after the wake is '$(agent_name "$PANE_A")', expected alpha"; fi
  if screen_has "$PANE_A" "fake claude session $SID"; then ok "the screen shows the resumed session"; else bad "the screen does not show 'fake claude session $SID'"; fi
  if [ -z "$(placeholder_pid "$PANE_A")" ]; then ok "the Placeholder is gone"; else bad "the Placeholder still runs"; fi
}

s18_woken_pane_loses_its_marker() {
  new_world "$CONFIG_MANUAL" || return
  local tab
  tab="$(pane_tab "$PANE_A")"
  sleep_alpha || return
  if wait_for 15 tab_prefixed "$tab"; then ok "the tab of the sleeping agent is prefixed"; else bad "the tab label is '$(tab_label "$tab")'"; fi
  # Wake it the way a user does (Enter in the Placeholder), which leaves the registry through the Replay.
  herdr pane send-keys "$PANE_A" enter >/dev/null 2>&1
  if wait_for 40 is_awake_idle "$PANE_A"; then ok "the agent is back as claude"; else
    bad "no wake: kind=$(agent_kind "$PANE_A")"
    return
  fi
  if wait_for 15 tab_not_prefixed "$tab"; then ok "the tab loses its prefix once the agent is back"; else bad "the tab label is still '$(tab_label "$tab")'"; fi
  # The agent ends: nothing sleeps, nothing is left to restore.
  herdr pane send-text "$PANE_A" "/exit" >/dev/null 2>&1
  herdr pane send-keys "$PANE_A" enter >/dev/null 2>&1
  if wait_for 20 no_agent "$PANE_A"; then ok "the agent ended"; else bad "kind is still '$(agent_kind "$PANE_A")'"; fi
  hs sweep --now >/dev/null 2>&1
  if tab_not_prefixed "$tab"; then ok "the tab stays unprefixed after the sweep"; else bad "the tab label is '$(tab_label "$tab")'"; fi
  if jq -e '.panes | has("'"$PANE_A"'") | not' "$STATE_DIR/sleepers.json" >/dev/null 2>&1; then ok "the registry no longer holds the pane"; else bad "the registry still holds $PANE_A"; fi
  if stop_server; then ok "the server stopped"; else
    bad "the server did not stop"
    return
  fi
  if start_server; then ok "the server started again"; else
    bad "the server did not restart"
    return
  fi
  attach_client
  if wait_for 60 restore_ran_twice; then ok "restore ran again after the restart"; else bad "no second RESTORE-START after the restart"; fi
  if [ -z "$(agent_kind "$PANE_A")" ] && tab_not_prefixed "$(pane_tab "$PANE_A")"; then ok "the ended agent stays a plain shell, tab unprefixed"; else bad "kind='$(agent_kind "$PANE_A")' tab='$(tab_label "$(pane_tab "$PANE_A")")'"; fi
}

s3_wake_by_typing() {
  new_world "$CONFIG_MANUAL" || return
  sleep_alpha || return
  herdr pane send-text "$PANE_A" hello >/dev/null 2>&1
  herdr pane send-keys "$PANE_A" enter >/dev/null 2>&1
  if wait_for 40 is_awake_idle "$PANE_A"; then ok "the agent is back as claude"; else bad "no wake: kind=$(agent_kind "$PANE_A")"; fi
  if wait_for 30 screen_has "$PANE_A" "got: hello"; then ok "the typed text was replayed (got: hello)"; else
    bad "the screen never showed 'got: hello'"
    screen "$PANE_A" | tail -n 8 | sed 's/^/        | /'
  fi
  if [ "$(agent_session "$PANE_A")" = "$SID" ]; then ok "same session id"; else bad "session id is '$(agent_session "$PANE_A")'"; fi
}

s4_control_keys() {
  new_world "$CONFIG_MANUAL" || return
  sleep_alpha || return
  herdr pane send-keys "$PANE_A" ctrl+c ctrl+c ctrl+d >/dev/null 2>&1
  if wait_for 40 is_awake_idle "$PANE_A"; then ok "the agent still comes back"; else bad "no wake after ctrl+c ctrl+c ctrl+d: kind=$(agent_kind "$PANE_A") status=$(agent_status "$PANE_A")"; fi
  sleep 2
  if screen_has "$PANE_A" "got:"; then bad "something was replayed ('got:' on screen)"; else ok "nothing was replayed"; fi
  if [ "$(agent_session "$PANE_A")" = "$SID" ]; then ok "same session id"; else bad "session id is '$(agent_session "$PANE_A")'"; fi
  if pgrep -u "$(id -u)" -x claude >/dev/null; then ok "the agent process is running"; else bad "no claude process"; fi
  if wait_for 20 is_named "$PANE_A" alpha; then ok "the name alpha is given back"; else bad "the name after the wake is '$(agent_name "$PANE_A")', expected alpha"; fi
}

s5_restart() {
  new_world "$CONFIG_MANUAL" || return
  sleep_alpha || return
  if stop_server; then ok "the server stopped"; else
    bad "the server did not stop"
    return
  fi
  if start_server; then ok "the server started again"; else
    bad "the server did not restart"
    return
  fi
  attach_client
  if wait_for 60 is_asleep "$PANE_A"; then ok "alpha's pane is sleeping-claude again, restored by the plugin"; else
    bad "after the restart the pane is kind='$(agent_kind "$PANE_A")' (expected sleeping-claude)"
    note "agents: $(herdr agent list 2>&1 | head -c 400)"
    note "processes: $(ps -u "$(id -u)" -o pid=,args= | cut -c1-110 | tr '\n' '|')"
    return
  fi
  if [ -n "$(placeholder_pid "$PANE_A")" ]; then ok "a Placeholder runs in the pane (started by the plugin, not on PATH)"; else bad "no Placeholder process in the pane"; fi
  if pgrep -u "$(id -u)" -x claude >/dev/null; then bad "a claude process runs after the restart"; else ok "no agent process was started"; fi
  if wait_for 15 is_named "$PANE_A" alpha; then ok "the name alpha is kept"; else bad "the name after the restart is '$(agent_name "$PANE_A")'"; fi
  restore_logged() { grep -q "RESTORE $PANE_A" "$STATE_DIR/herdr-sleep.log" 2>/dev/null; }
  # The line is written right after the Placeholder reports, which is what the wait above saw.
  if wait_for 10 restore_logged; then ok "the journal records the restore"; else bad "no RESTORE line in the journal"; fi
  if command -v herdr-sleep-ph >/dev/null; then bad "herdr-sleep-ph is on PATH"; else ok "herdr-sleep-ph is not on PATH"; fi
  herdr pane send-keys "$PANE_A" enter >/dev/null 2>&1
  if wait_for 40 is_awake_idle "$PANE_A"; then ok "the restored pane wakes"; else bad "the restored pane did not wake (kind=$(agent_kind "$PANE_A"))"; fi
  if [ "$(agent_session "$PANE_A")" = "$SID" ]; then ok "same session id after restart and wake"; else bad "session id is '$(agent_session "$PANE_A")', expected $SID"; fi
}

s6_two_panes_in_a_tab() {
  new_world "$CONFIG_MANUAL" || return
  local tab split second
  tab="$(pane_tab "$PANE_A")"
  split="$(herdr pane split "$PANE_A" --direction right --no-focus)"
  second="$(jq -r '.result.pane.pane_id // .result.root_pane.pane_id // empty' <<<"$split")"
  if [ -z "$second" ]; then
    bad "pane split gave no pane: $split"
    return
  fi
  start_agent "$second" beta || return
  sleep_alpha || return
  hs sleep beta --json >/dev/null 2>&1
  if ! wait_for 30 is_asleep "$second"; then
    bad "beta did not go to sleep"
    return
  fi
  ok "both panes sleep"
  if wait_for 15 tab_prefixed "$tab"; then ok "the tab label has the sleep prefix ($(tab_label "$tab"))"; else bad "the tab label '$(tab_label "$tab")' has no sleep prefix"; fi
  local token
  token="$(herdr workspace list | jq -r --arg w "$WS_A" '.result.workspaces[] | select(.workspace_id == $w) | (.tokens.sleep // "")')"
  if [ "$token" = "$SLEEP_ICON" ]; then ok "the workspace carries the sleep token"; else bad "the workspace token is '$token'"; fi
  herdr workspace focus "$WS_A" >/dev/null 2>&1
  if wait_for 40 is_awake_idle "$PANE_A"; then ok "the focused pane woke"; else bad "the focused pane did not wake (kind=$(agent_kind "$PANE_A"))"; fi
  sleep 2
  if is_asleep "$second"; then ok "the other pane stays asleep"; else bad "the other pane is '$(agent_kind "$second")', expected sleeping-claude"; fi
  if wait_for 15 tab_not_prefixed "$tab"; then ok "the tab prefix is removed ($(tab_label "$tab"))"; else bad "the tab label still has the prefix: '$(tab_label "$tab")'"; fi
}

s7_draft_guard() {
  # sleep_after_minutes = 0 so that the Watcher's sweeps evaluate the guards and remember
  # the refusal, which is what `status` shows.
  new_world "$CONFIG_SWEEP" || return
  herdr pane send-text "$PANE_A" "half written" >/dev/null 2>&1
  if ! wait_for 10 screen_has "$PANE_A" "half written"; then
    bad "the typed text did not reach the prompt"
    return
  fi
  # Shortly after the agent starts its status can still flip to unknown, which would make the
  # guard answer "not-settled" instead of the draft guard this scenario is about.
  wait_for 30 steady_idle "$PANE_A"
  local out rc
  out="$(hs sleep alpha --json 2>&1)"
  rc=$?
  if [ "$rc" = 1 ]; then ok "manual sleep exits 1"; else bad "manual sleep exited $rc ($out)"; fi
  if [ "$(jq -r '.outcome' <<<"$out" 2>/dev/null)" = "refused" ]; then ok "outcome is refused"; else bad "outcome: $out"; fi
  if [ "$(jq -r '.reason' <<<"$out" 2>/dev/null)" = "draft-in-prompt" ]; then ok "the reason is draft-in-prompt"; else bad "reason: $out"; fi
  if [ "$(agent_kind "$PANE_A")" = "claude" ]; then ok "the agent is still claude"; else bad "the agent is '$(agent_kind "$PANE_A")'"; fi
  status_shows_draft() { hs status 2>&1 | grep -q "draft-in-prompt"; }
  if wait_for 40 status_shows_draft; then ok "status shows the refusal (draft-in-prompt)"; else bad "status never showed the refusal: $(hs status 2>&1 | head -n 4 | tr '\n' '|')"; fi
  if [ "$(agent_kind "$PANE_A")" = "claude" ]; then ok "the Watcher did not put the draft to sleep"; else bad "the agent is '$(agent_kind "$PANE_A")'"; fi
}

s8_watcher() {
  new_world "$CONFIG_SWEEP" || return
  if wait_for 15 watcher_running; then ok "the Watcher was started by Herdr's startup hook"; else bad "the Watcher is not running"; fi
  # sweep_interval_seconds = 5: the first sweep starts the idle clock, the next one sleeps.
  if wait_for 40 is_asleep "$PANE_A"; then ok "alpha went to sleep by itself"; else bad "alpha did not sleep by itself within 40 s (kind=$(agent_kind "$PANE_A"))"; fi
  if wait_for 15 is_named "$PANE_A" alpha; then ok "the name alpha is kept"; else bad "the name is '$(agent_name "$PANE_A")', expected alpha"; fi
}

s9_restart_with_an_awake_agent() {
  new_world "$CONFIG_MANUAL" || return
  local split second sid_b
  split="$(herdr pane split "$PANE_A" --direction right --no-focus)"
  second="$(jq -r '.result.pane.pane_id // .result.root_pane.pane_id // empty' <<<"$split")"
  if [ -z "$second" ]; then
    bad "pane split gave no pane: $split"
    return
  fi
  start_agent "$second" beta || return
  sid_b="$(agent_session "$second")"
  # The Watcher records every live agent, so the restart gives beta its name back.
  if wait_for 30 grep -q '"name": "beta"' "$STATE_DIR/live-agents.json"; then ok "the live record holds beta"; else bad "live-agents.json has no beta: $(cat "$STATE_DIR/live-agents.json" 2>&1 | head -c 300)"; fi
  # alpha sleeps (it is in the registry); beta stays awake and idle, so only Herdr's Saved
  # session knows it after the restart.
  sleep_alpha || return
  if stop_server; then ok "the server stopped"; else
    bad "the server did not stop"
    return
  fi
  if start_server; then ok "the server started again"; else
    bad "the server did not restart"
    return
  fi
  attach_client
  if wait_for 90 is_asleep "$PANE_A"; then ok "alpha (registered) is sleeping-claude again"; else bad "alpha is kind='$(agent_kind "$PANE_A")' after the restart"; fi
  if wait_for 90 is_asleep "$second"; then ok "beta (never put to sleep) is sleeping-claude after the restart"; else
    bad "beta is kind='$(agent_kind "$second")' status='$(agent_status "$second")' after the restart"
    herdr pane read "$second" --lines 40 --format ansi 2>&1 | cat -v | sed "s/^/        screen| /"
    note "pane: $(herdr pane get "$second" 2>&1 | head -c 500)"
    note "journal: $(tail -n 6 "$STATE_DIR/herdr-sleep.log" 2>/dev/null | tr '\n' '|')"
    return
  fi
  if pgrep -u "$(id -u)" -x claude >/dev/null; then bad "a claude process runs after the restart"; else ok "no agent process was started"; fi
  # Herdr forgets the name of an agent that never slept: the live record gives it back.
  if wait_for 15 is_named "$second" beta; then ok "beta keeps its name"; else bad "beta's name is '$(agent_name "$second")', expected beta"; fi
  if wait_for 20 grep -q "RESTORE-DONE restored=2 skipped=0 missing_cwd=0" "$STATE_DIR/herdr-sleep.log"; then ok "the journal sums the restore up"; else bad "no RESTORE-DONE restored=2 line"; fi
  herdr pane send-keys "$second" enter >/dev/null 2>&1
  if wait_for 40 is_awake_idle "$second"; then ok "beta wakes"; else bad "beta did not wake (kind=$(agent_kind "$second"))"; fi
  if [ "$(agent_session "$second")" = "$sid_b" ]; then ok "beta resumes its own session"; else bad "beta's session is '$(agent_session "$second")', expected $sid_b"; fi
}

s10_sweep_now_and_sleep_errors() {
  new_world "$CONFIG_MANUAL" || return
  local out rc
  # The idle time (600 minutes) is far away: a plain sweep leaves alpha alone.
  hs sweep >/dev/null 2>&1
  if is_awake_idle "$PANE_A"; then ok "a plain sweep leaves alpha awake"; else bad "alpha is '$(agent_kind "$PANE_A")' after a plain sweep"; fi
  # No pane at all: an explicit error on stderr, exit code 1.
  out="$(hs sleep w999:p999 2>&1 >/dev/null)"
  rc=$?
  if [ "$rc" = 1 ] && [ -n "$out" ]; then ok "an unknown pane exits 1 with a message ($out)"; else bad "unknown pane: rc=$rc stderr='$out'"; fi
  # A sweep that does not wait for the idle time.
  hs sweep --now --json >/dev/null 2>&1
  if wait_for 40 is_asleep "$PANE_A"; then ok "sweep --now put alpha to sleep"; else bad "alpha is '$(agent_kind "$PANE_A")' after sweep --now"; fi
  # Refused sleep: asleep already; the reason is on stderr.
  out="$(hs sleep alpha 2>&1 >/dev/null)"
  rc=$?
  if [ "$rc" = 1 ] && [ -n "$out" ]; then ok "a refused sleep explains itself on stderr"; else bad "refused sleep: rc=$rc stderr='$out'"; fi
}

s11_exclusion_regexes() {
  new_world "$CONFIG_MANUAL" || return
  local out rc key
  set_config() { printf '%s\n' "$CONFIG_MANUAL" "$1" >"$CONFIG_DIR/config.toml"; }
  # Each exclusion key is checked on its own: the pane stays awake, `sleep` says why and
  # suggests --force, `config` shows the key.
  herdr pane send-text "$PANE_A" "waiting for approval" >/dev/null 2>&1
  herdr pane send-keys "$PANE_A" enter >/dev/null 2>&1
  wait_for 30 is_awake_idle "$PANE_A"
  for key in 'exclude_name_regex = ["^ALP"]|excluded-name-regex' \
    'exclude_screen_regex = ["Waiting for approval"]|excluded-screen-regex' \
    'exclude_cmdline_regex = ["claude"]|excluded-cmdline-regex'; do
    set_config "${key%%|*}"
    out="$(hs sleep alpha 2>&1 >/dev/null)"
    rc=$?
    if [ "$rc" = 1 ] && [[ "$out" == *"${key##*|}"* ]]; then ok "sleep refuses with ${key##*|}"; else bad "${key##*|}: rc=$rc stderr='$out'"; fi
    if [[ "$out" == *"--force"* ]]; then ok "the refusal suggests --force"; else bad "no --force hint: '$out'"; fi
    hs sweep --now >/dev/null 2>&1
    if is_awake_idle "$PANE_A"; then ok "sweep --now leaves alpha awake (${key##*|})"; else bad "alpha is '$(agent_kind "$PANE_A")' after sweep --now (${key##*|})"; fi
    if hs config 2>&1 | grep -q "^$(sed 's/ =.*//' <<<"${key%%|*}") = \\["; then ok "config shows ${key%%|*}"; else bad "config does not show ${key%%|*}"; fi
  done
  # An invalid regex is an error that names the key; the file is kept.
  set_config 'exclude_screen_regex = ["("]'
  out="$(hs sweep --now 2>&1)"
  if [[ "$out" == *exclude_screen_regex* ]]; then ok "an invalid regex names its key"; else bad "invalid regex message: '$out'"; fi
  # --force ignores the three patterns.
  set_config 'exclude_name_regex = ["^ALP"]
exclude_screen_regex = ["Waiting for approval"]
exclude_cmdline_regex = ["claude"]'
  hs sleep --force alpha >/dev/null 2>&1
  if wait_for 40 is_asleep "$PANE_A"; then ok "sleep --force ignores the exclusions"; else bad "alpha is '$(agent_kind "$PANE_A")' after sleep --force"; fi
}

s12_tall_status_bar() {
  AGENT_PREFIX="FAKE_AGENT_STATUS_LINES=16 "
  new_world "$CONFIG_MANUAL" || return
  AGENT_PREFIX=""
  local out rc
  # The prompt is 17 rows from the bottom: only its frame finds it. Without the frame
  # detection the fallback window misses it and the refusal names both settings.
  printf '%s\n' "$CONFIG_MANUAL" 'prompt_frame_regex = ""' >"$CONFIG_DIR/config.toml"
  out="$(hs sleep alpha 2>&1 >/dev/null)"
  rc=$?
  if [ "$rc" = 1 ] && [[ "$out" == *"prompt not detected"* && "$out" == *prompt_area_lines* && "$out" == *prompt_frame_regex* ]]; then ok "no frame detection: prompt-not-detected names both settings"; else bad "rc=$rc stderr='$out'"; fi
  printf '%s\n' "$CONFIG_MANUAL" >"$CONFIG_DIR/config.toml"
  hs sleep alpha >/dev/null 2>&1
  if wait_for 40 is_asleep "$PANE_A"; then ok "the framed prompt is found under a tall status bar"; else bad "alpha is '$(agent_kind "$PANE_A")' (stderr: $(hs sleep alpha 2>&1 >/dev/null))"; fi
  herdr pane send-keys "$PANE_A" enter >/dev/null 2>&1
  if wait_for 40 is_awake_idle "$PANE_A"; then ok "alpha wakes and is recognised as ready"; else bad "alpha is '$(agent_kind "$PANE_A")' after the wake"; fi
}

s13_preview_without_input_box() {
  AGENT_PREFIX="FAKE_AGENT_STATUS_LINES=3 "
  new_world "$CONFIG_MANUAL" || return
  AGENT_PREFIX=""
  hs sleep alpha >/dev/null 2>&1
  if ! wait_for 40 is_asleep "$PANE_A"; then bad "alpha is '$(agent_kind "$PANE_A")'"; return; fi
  local text
  text="$(screen "$PANE_A")"
  if [[ "$text" == *"is asleep"* ]]; then ok "the placeholder shows its message"; else bad "no message: $text"; fi
  if [[ "$text" != *"status 1"* && "$text" != *$'\u2500\u2500\u2500\u2500'* ]]; then ok "no input box rule and no status bar under the message"; else bad "input box or status bar left: $text"; fi
}

s14_action_refusal() {
  new_world "$CONFIG_MANUAL" || return
  local out rc
  printf '%s\n' "$CONFIG_MANUAL" 'exclude_name_regex = ["^ALP"]' >"$CONFIG_DIR/config.toml"
  # Started like the Herdr action: no target, the pane comes from the action context. The
  # refusal is also sent as a notification, which must not change the outcome.
  out="$(HERDR_PANE_ID="$PANE_A" hs sleep 2>&1 >/dev/null)"
  rc=$?
  if [ "$rc" = 1 ] && [[ "$out" == *excluded-name-regex* && "$out" == *"--force"* ]]; then ok "an action refusal still explains itself and exits 1"; else bad "rc=$rc stderr='$out'"; fi
  if is_awake_idle "$PANE_A"; then ok "alpha stays awake"; else bad "alpha is '$(agent_kind "$PANE_A")'"; fi
}

s15_sleep_with_draft() {
  new_world "$CONFIG_MANUAL" || return
  local out rc
  herdr pane send-text "$PANE_A" "my draft" >/dev/null 2>&1
  sleep 1
  # Off by default: the draft keeps the agent awake.
  out="$(hs sleep alpha 2>&1 >/dev/null)"
  rc=$?
  if [ "$rc" = 1 ] && [[ "$out" == *draft-in-prompt* || "$out" == *"typed in the prompt"* ]]; then ok "a draft keeps the agent awake by default"; else bad "rc=$rc stderr='$out'"; fi
  printf '%s\n' "$CONFIG_MANUAL" 'sleep_with_draft = true' >"$CONFIG_DIR/config.toml"
  hs sleep alpha >/dev/null 2>&1
  if wait_for 40 is_asleep "$PANE_A"; then ok "alpha sleeps with a draft in its prompt"; else bad "alpha is '$(agent_kind "$PANE_A")' (stderr: $(hs sleep alpha 2>&1 >/dev/null))"; return; fi
  herdr pane send-keys "$PANE_A" enter >/dev/null 2>&1
  if wait_for 40 is_awake_idle "$PANE_A"; then ok "alpha wakes"; else bad "alpha is '$(agent_kind "$PANE_A")' after the wake"; return; fi
  has_draft() { screen "$PANE_A" | grep -q "my draft"; }
  wait_for 20 has_draft
  in_registry() { grep -q '"w1:p1"' "$STATE_DIR/sleepers.json" 2>/dev/null; }
  not_in_registry() { ! in_registry; }
  if wait_for 20 not_in_registry; then ok "the woken pane leaves the registry of sleeping panes"; else bad "w1:p1 is still in sleepers.json"; fi
  local text
  text="$(screen "$PANE_A")"
  if [[ "$text" == *"my draft"* && "$text" != *"got: my draft"* ]]; then ok "the draft is back in the prompt, not submitted"; else bad "screen: $(tail -8 <<<"$text" | tr '\n' '|')"; fi
}

s16_remote_control() {
  AGENT_PREFIX="FAKE_AGENT_STATUS_LINES=2 FAKE_AGENT_BADGE='/rc active' "
  new_world "$CONFIG_MANUAL" || return
  AGENT_PREFIX=""
  local out rc
  out="$(hs sleep alpha 2>&1 >/dev/null)"
  rc=$?
  if [ "$rc" = 1 ] && [[ "$out" == *remote-control* && "$out" == *"--force"* ]]; then ok "sleep refuses an agent in Remote Control and says why"; else bad "rc=$rc stderr='$out'"; fi
  hs sweep --now >/dev/null 2>&1
  if is_awake_idle "$PANE_A"; then ok "sweep --now leaves it awake"; else bad "alpha is '$(agent_kind "$PANE_A")' after sweep --now"; fi
  hs sleep alpha --force >/dev/null 2>&1
  if wait_for 40 is_asleep "$PANE_A"; then ok "sleep --force ignores the guard"; else bad "alpha is '$(agent_kind "$PANE_A")' after sleep --force"; fi
}

s17_keep_tab_awake() {
  new_world "$CONFIG_MANUAL" || return
  local tab out rc label
  tab="$(pane_tab "$PANE_A")"
  label="$(tab_label "$tab")"
  hs keep-awake alpha >/dev/null 2>&1
  if [ "$(tab_label "$tab")" = "$KEEP_ICON $label" ]; then ok "the tab label has the lock icon ($(tab_label "$tab"))"; else bad "the tab label is '$(tab_label "$tab")'"; fi
  out="$(hs sleep alpha 2>&1 >/dev/null)"
  rc=$?
  if [ "$rc" = 1 ] && [[ "$out" == *tab-kept-awake* && "$out" == *"--force"* ]]; then ok "sleep refuses a pane of a tab kept awake"; else bad "rc=$rc stderr='$out'"; fi
  hs sweep --now >/dev/null 2>&1
  if is_awake_idle "$PANE_A"; then ok "sweep --now leaves it awake"; else bad "alpha is '$(agent_kind "$PANE_A")' after sweep --now"; fi
  hs keep-awake alpha >/dev/null 2>&1
  if [ "$(tab_label "$tab")" = "$label" ]; then ok "the same action removes the icon"; else bad "the tab label is '$(tab_label "$tab")', expected '$label'"; fi
  hs sleep alpha >/dev/null 2>&1
  if wait_for 40 is_asleep "$PANE_A"; then ok "alpha sleeps once the tab is released"; else bad "alpha is '$(agent_kind "$PANE_A")'"; fi
}

s19_overlapping_sleeps() {
  new_world "$CONFIG_MANUAL" || return
  # A Sweep of the Watcher, the "Sleep idle agents now" action and a manual
  # Sleep can reach the same pane together: only one may type into it.
  hs sleep alpha --json >/dev/null 2>&1 &
  hs sleep alpha --json >/dev/null 2>&1 &
  hs sleep alpha --json >/dev/null 2>&1 &
  wait
  if wait_for 30 is_asleep "$PANE_A"; then ok "alpha sleeps"; else bad "alpha is '$(agent_kind "$PANE_A")' after three sleeps"; return; fi
  wait_for 10 placeholder_pid "$PANE_A"
  local slept
  slept="$(grep -c " SLEEP .*($PANE_A)" "$STATE_DIR/herdr-sleep.log" 2>/dev/null)"
  if [ "$slept" = "1" ]; then ok "the pane was put to sleep once"; else bad "$slept SLEEP lines for $PANE_A, expected 1"; fi
  herdr pane send-keys "$PANE_A" enter >/dev/null 2>&1
  if wait_for 40 is_awake_idle "$PANE_A"; then ok "the agent is back as claude"; else bad "no wake: kind=$(agent_kind "$PANE_A")"; return; fi
  sleep 3
  if screen_has "$PANE_A" "herdr-sleep-ph"; then bad "the Placeholder command was typed into the agent"; else ok "nothing was typed into the agent"; fi
}

s20_a_quit_agent_is_not_restored() {
  new_world "$CONFIG_MANUAL" || return
  local split second
  split="$(herdr pane split "$PANE_A" --direction right --no-focus)"
  second="$(jq -r '.result.pane.pane_id // .result.root_pane.pane_id // empty' <<<"$split")"
  [ -n "$second" ] || { bad "pane split gave no pane: $split"; return; }
  start_agent "$second" gamma || return
  if wait_for 30 grep -q '"name": "gamma"' "$STATE_DIR/live-agents.json"; then ok "the live record holds gamma"; else bad "gamma is not in live-agents.json"; return; fi
  # The user quits gamma: the Watcher sees it go and forgets it.
  herdr pane send-text "$second" "/exit" >/dev/null 2>&1
  herdr pane send-keys "$second" enter >/dev/null 2>&1
  no_agent() { [ -z "$(agent_kind "$1")" ]; }
  if wait_for 30 no_agent "$second"; then ok "gamma quit"; else bad "gamma is still '$(agent_kind "$second")'"; return; fi
  no_record() { ! grep -q '"name": "gamma"' "$STATE_DIR/live-agents.json"; }
  if wait_for 30 no_record; then ok "the record of gamma is gone"; else bad "gamma is still recorded"; fi
  sleep_alpha || return
  stop_server || { bad "the server did not stop"; return; }
  start_server || { bad "the server did not restart"; return; }
  attach_client
  if wait_for 90 is_asleep "$PANE_A"; then ok "alpha is sleeping-claude again"; else bad "alpha is '$(agent_kind "$PANE_A")'"; fi
  sleep 5
  if no_agent "$second" && ! grep -q "RESTORE-RESUME $second" "$STATE_DIR/herdr-sleep.log"; then ok "gamma, quit before the restart, stays a shell"; else bad "gamma was brought back (kind=$(agent_kind "$second"))"; fi
}

s21_unsubmitted_text_stays_in_the_prompt() {
  new_world "$CONFIG_MANUAL" || return
  sleep_alpha || return
  # No Enter: the user did not submit this, the agent must not receive it as a prompt.
  herdr pane send-text "$PANE_A" "half typed" >/dev/null 2>&1
  if wait_for 40 is_awake_idle "$PANE_A"; then ok "the agent is back as claude"; else bad "no wake: kind=$(agent_kind "$PANE_A")"; return; fi
  if wait_for 30 screen_has "$PANE_A" "half typed"; then ok "the text is in the prompt"; else bad "the text never reached the prompt"; fi
  sleep 2
  if screen_has "$PANE_A" "got: half typed"; then bad "the text was submitted without Enter"; else ok "the text was not submitted"; fi
}

SCENARIOS=(
  "s1|sleep by sweep|s1_sleep_by_sweep"
  "s2|wake by Enter|s2_wake_by_enter"
  "s3|wake by typing (replay)|s3_wake_by_typing"
  "s4|Ctrl+C, Ctrl+C, Ctrl+D wake without replay|s4_control_keys"
  "s5|server restart|s5_restart"
  "s6|two sleeping panes in one tab|s6_two_panes_in_a_tab"
  "s7|draft guard on manual sleep|s7_draft_guard"
  "s8|the Watcher sleeps an idle agent|s8_watcher"
  "s9|restart with an awake agent (Saved session)|s9_restart_with_an_awake_agent"
  "s10|sweep --now and explicit sleep errors|s10_sweep_now_and_sleep_errors"
  "s11|exclusion regexes keep an agent awake|s11_exclusion_regexes"
  "s12|prompt found under a tall status bar|s12_tall_status_bar"
  "s13|preview leaves out the input box and status bar|s13_preview_without_input_box"
  "s14|a refused action sleep explains itself|s14_action_refusal"
  "s15|sleep with a draft saves and restores it|s15_sleep_with_draft"
  "s16|an agent in Remote Control stays awake|s16_remote_control"
  "s17|a tab kept awake does not sleep|s17_keep_tab_awake"
  "s18|a woken pane loses its marker|s18_woken_pane_loses_its_marker"
  "s19|overlapping sleeps type once|s19_overlapping_sleeps"
  "s20|an agent the user quit is not restored|s20_a_quit_agent_is_not_restored"
  "s21|unsubmitted text stays in the prompt|s21_unsubmitted_text_stays_in_the_prompt"
)

# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

echo "herdr:  $(herdr --version 2>&1)"
echo "plugin: $("$HS" --version 2>&1)"
echo

run_scenario() {
  local id="$1" title="$2" fn="$3" known
  known="${KNOWN_FAILING[$id]:-}"
  CURRENT_BAD=0
  echo "== $id: $title"
  "$fn"
  if [ "$CURRENT_BAD" -gt 0 ]; then
    journal_tail
    [ -n "$known" ] && note "(known failing: $known)"
  fi
  end_world
  if [ "$CURRENT_BAD" -eq 0 ]; then
    if [ -n "$known" ]; then
      echo "-- $id: PASS (listed as known-failing, remove it from KNOWN_FAILING)"
      KNOWN_PASSED+=("$id $title")
    else
      echo "-- $id: PASS"
      PASSED+=("$id")
    fi
  elif [ -n "$known" ]; then
    echo "-- $id: KNOWN-FAILING ($CURRENT_BAD check(s) failed)"
    KNOWN_HIT+=("$id $title: $known")
  else
    echo "-- $id: FAIL ($CURRENT_BAD check(s) failed)"
    UNEXPECTED+=("$id $title")
    FAILURES=$((FAILURES + 1))
  fi
  echo
}

for entry in "${SCENARIOS[@]}"; do
  IFS='|' read -r id title fn <<<"$entry"
  if [ -z "$ONLY" ] || [ "$ONLY" = "$id" ]; then
    run_scenario "$id" "$title" "$fn"
  fi
done

echo "== Summary"
echo "passed: ${#PASSED[@]} (${PASSED[*]:-none})"
if [ "${#KNOWN_HIT[@]}" -gt 0 ] || [ "${#KNOWN_PASSED[@]}" -gt 0 ]; then
  echo
  echo "Known-failing scenarios:"
  for line in "${KNOWN_HIT[@]}"; do echo "  - $line"; done
  for line in "${KNOWN_PASSED[@]}"; do echo "  - $line: now passes, remove it from KNOWN_FAILING"; done
fi
echo
if [ "$FAILURES" -eq 0 ]; then
  echo "ALL TESTS OK"
else
  for line in "${UNEXPECTED[@]}"; do echo "  FAILED: $line"; done
  echo "$FAILURES FAILURE(S)"
fi
exit "$FAILURES"
