#!/usr/bin/env bash
# e2e/local/ctl.sh: start, stop and watch the processes of the local stack from a scenario.
# Source it after the network env file and lib.sh. Local stack only (e2e/local/local.env): it needs
# the Zone binary, the zone.env stack.sh wrote, the LEZ checkout (its bedrock/ compose
# runs the dev node, its tools/run_sequencer.sh the LEZ sequencer).
#
# Zone processes come in three roles: `sequencer` (ZONE_HTTP, data $E2E_STATE/zone-sequencer),
# `indexer` (ZONE_INDEXER_HTTP, $E2E_STATE/zone-indexer) and `rogue` (ZONE_ROGUE_HTTP,
# $E2E_STATE/zone-rogue: a second sequencer on a copy of the sequencer's data dir, so the same
# channel key, started with a wrong view to publish a wrong batch; see
# scenarios/wrong-batch-correction.sh). Each appends to $ZONE_LOG_DIR/zone-<role>.log
# across restarts, with a `==== start` line per start, so a scenario marks the log
# (`log_mark`) and waits for lines after the mark (`zone_wait_log`). The rogue runs as
# MODE=sequencer.

ZONE_BIN="${ZONE_BIN:-$REPO_DIR/target/release/zone}"
ZONE_ENV="${ZONE_ENV:-$E2E_STATE/zone.env}"
ZONE_LOG_DIR="${ZONE_LOG_DIR:-$E2E_STATE/logs}"
ZONE_INDEXER_HTTP="${ZONE_INDEXER_HTTP:-127.0.0.1:3001}"
ZONE_ROGUE_HTTP="${ZONE_ROGUE_HTTP:-127.0.0.1:3002}"
mkdir -p "$ZONE_LOG_DIR"

# ---- the Zone processes --------------------------------------------------------------------

zone_role_http() {
    case "$1" in
        sequencer) echo "$ZONE_HTTP" ;;
        indexer) echo "$ZONE_INDEXER_HTTP" ;;
        rogue) echo "$ZONE_ROGUE_HTTP" ;;
        *) die "unknown zone role $1" ;;
    esac
}
# zone_role_mode <role>: what MODE the role runs as.
zone_role_mode() { case "$1" in rogue) echo sequencer ;; *) echo "$1" ;; esac; }
zone_role_data() { echo "$E2E_STATE/zone-$1"; }
zone_role_log() { echo "$ZONE_LOG_DIR/zone-$1.log"; }
zone_role_pidfile() { echo "$E2E_STATE/zone-$1.pid"; }
# The pid: the one on record if it is alive, else whoever listens on the role's port.
zone_role_pid() {
    local pid http
    pid=$(cat "$(zone_role_pidfile "$1")" 2>/dev/null || true)
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then echo "$pid"; return 0; fi
    http=$(zone_role_http "$1")
    lsof -ti tcp:"${http##*:}" -sTCP:LISTEN 2>/dev/null | head -1 || true
}
# zone_at <role> <path>: the role's HTTP API.
zone_at() { curl -sS -m 30 "http://$(zone_role_http "$1")$2"; }
# zone_field <role> <jq filter>: one value out of the role's `/` summary.
zone_field() { zone_at "$1" / | jq -r "$2"; }

# zone_start <role> [VAR=value ...]: the role in the background with zone.env, its mode, data
# dir and bind, and the overrides given (REBUILD_TREE=true, MAX_TX_FEE=1, ...).
zone_start() {
    local role="$1" log pidfile kv
    shift
    [ -x "$ZONE_BIN" ] || die "no zone binary at $ZONE_BIN: cargo build --release --bins (ZONE_BIN)"
    [ -f "$ZONE_ENV" ] || die "no $ZONE_ENV: stack.sh zone-env writes it"
    if [ -n "$(zone_role_pid "$role")" ]; then
        say "zone $role: already running (pid $(zone_role_pid "$role"))"
        return 0
    fi
    log=$(zone_role_log "$role"); pidfile=$(zone_role_pidfile "$role")
    printf '\n==== start %s %s %s\n' "$(date -u +%FT%TZ)" "$role" "$*" >> "$log"
    (
        set -a
        # shellcheck source=/dev/null
        . "$ZONE_ENV"
        set +a
        export MODE="$(zone_role_mode "$role")" DATA_DIR="$(zone_role_data "$role")" HTTP_ADDR="$(zone_role_http "$role")"
        export RUST_LOG="${ZONE_RUST_LOG:-info}"
        for kv in "$@"; do export "${kv?}"; done
        nohup "$ZONE_BIN" >> "$log" 2>&1 < /dev/null &
        echo $! > "$pidfile"
    )
    say "zone $role: started (pid $(cat "$pidfile"), log $log${*:+, $*})"
}

# zone_stop <role> [-9]: SIGTERM (or SIGKILL) and wait until the process is gone.
zone_stop() {
    local role="$1" sig="${2:--TERM}" pid i
    pid=$(zone_role_pid "$role")
    [ -n "$pid" ] || { say "zone $role: not running"; return 0; }
    kill "$sig" "$pid" 2>/dev/null || true
    for i in $(seq 1 60); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.5
    done
    kill -0 "$pid" 2>/dev/null && die "zone $role (pid $pid) did not stop"
    say "zone $role: stopped (pid $pid, signal $sig)"
}

# zone_wait_http <role> [seconds]: the role's HTTP answers `/` with its mode.
zone_wait_http() {
    local role="$1" limit="${2:-120}" i
    for i in $(seq 1 "$limit"); do
        [ "$(zone_field "$role" .mode 2>/dev/null)" = "$(zone_role_mode "$role")" ] && return 0
        [ -n "$(zone_role_pid "$role")" ] || die "zone $role exited: see $(zone_role_log "$role")"
        sleep 1
    done
    die "zone $role: no HTTP answer after $limit s: see $(zone_role_log "$role")"
}

# ---- logs ----------------------------------------------------------------------------------

# log_mark <file>: the file's size now, for log_since / zone_wait_log.
log_mark() { wc -c < "$1" 2>/dev/null | tr -d ' ' || echo 0; }
# log_since <file> <mark>: what was appended after the mark.
log_since() { tail -c "+$(( $2 + 1 ))" "$1" 2>/dev/null || true; }
# zone_wait_log <role> <mark> <grep -E pattern> [seconds]: a matching line after the mark;
# prints the first match.
zone_wait_log() {
    local role="$1" mark="$2" pattern="$3" limit="${4:-300}" log i hit
    log=$(zone_role_log "$role")
    for i in $(seq 1 "$limit"); do
        hit=$(log_since "$log" "$mark" | grep -E -m1 -- "$pattern" || true)
        if [ -n "$hit" ]; then
            printf '%s\n' "$hit"
            return 0
        fi
        sleep 1
    done
    say "zone $role: no line matching '$pattern' within $limit s; the log since the mark:"
    log_since "$log" "$mark" | tail -n 20 >&2
    return 1
}
# zone_log_has <role> <mark> <pattern>: true when a line after the mark matches.
zone_log_has() { log_since "$(zone_role_log "$1")" "$2" | grep -E -q -- "$3"; }

# ---- the LEZ sequencer ---------------------------------------------------------------------

LEZ_HOME="${LEZ_HOME:-$E2E_STATE/lez-home}"

lez_seq_pid() { lsof -ti tcp:"${LEZ_SEQUENCER_URL##*:}" -sTCP:LISTEN 2>/dev/null | head -1 || true; }
# lez_seq_stop: the sequencer_service, its run_sequencer.sh wrapper and the tee behind it.
lez_seq_stop() {
    local pids i
    pids="$(pgrep -f "tools/run_sequencer.sh $LEZ_HOME" || true) $(lez_seq_pid)"
    [ -n "${pids// /}" ] || { say "lez sequencer: not running"; return 0; }
    # shellcheck disable=SC2086
    kill $pids 2>/dev/null || true
    for i in $(seq 1 60); do
        [ -z "$(lez_seq_pid)" ] && break
        sleep 0.5
    done
    [ -z "$(lez_seq_pid)" ] || die "the LEZ sequencer did not stop (pids $pids)"
    say "lez sequencer: stopped (pids ${pids// /, })"
}
# lez_seq_start: stack.sh lez resumes the existing home (built already, so quick).
lez_seq_start() { bash "$E2E_DIR/local/stack.sh" lez; }

# ---- the Bedrock dev node ------------------------------------------------------------------

bedrock_compose() { (cd "$LEZ_CHECKOUT/bedrock" && docker compose "$@"); }
bedrock_pause() { bedrock_compose pause; say "bedrock: paused"; }
bedrock_unpause() { bedrock_compose unpause; say "bedrock: unpaused"; }
bedrock_slot() { bedrock_info 2>/dev/null | jq -r '.slot // empty'; }
bedrock_lib_slot() { bedrock_info 2>/dev/null | jq -r '.lib_slot // empty'; }
# bedrock_wait_moving [seconds]: the LIB slot advances again.
bedrock_wait_moving() {
    local limit="${1:-120}" a b i
    a=$(bedrock_lib_slot)
    for i in $(seq 1 "$limit"); do
        sleep 1
        b=$(bedrock_lib_slot)
        if [ -n "$a" ] && [ -n "$b" ] && [ "$b" -gt "$a" ]; then
            say "bedrock: LIB slot moves again ($a -> $b)"
            return 0
        fi
        [ -n "$b" ] && a="$b"
    done
    die "bedrock: the LIB slot does not move within $limit s"
}

# ---- what two roles say --------------------------------------------------------------------

# roots_signature <role>: the applied, non-superseded roots as "root to_block" lines, oldest
# first, out of /roots/50: the same on every reader once they have read the same chain.
roots_signature() {
    zone_at "$1" /roots/50 | jq -r '
        [.roots[] | . as $r | .batches[] | select(.superseded == false and .error == null)
         | "\($r.root) \(.to_lez_block_id)"] | sort | .[]'
}
# zone_wait_agree [seconds]: the indexer reaches the sequencer's batch count and newest
# published root, carries the same roots, judged nothing wrong (`stall` null) and excludes as
# many batches as the sequencer: `superseded_count + parked_count` on each, since a wrong batch
# stays parked on the indexer that judged it (its error, the evidence) and is superseded on the
# sequencer that applied it unjudged.
zone_wait_agree() {
    local limit="${1:-300}" i seq idx stall stalled="" seq_x idx_x
    for i in $(seq 1 "$limit"); do
        seq=$(zone_at sequencer / | jq -c '{batch_count, root: .newest_published.root}')
        idx=$(zone_at indexer / | jq -c '{batch_count, root: .newest_published.root}')
        stall=$(zone_field indexer .stall)
        # A stall may be on its way out (a wrong batch parked, its correction applied a tick
        # later; a rebuilt tree judging the parked batches again): wait, say so once.
        if [ "$stall" != "null" ]; then
            [ -n "$stalled" ] || say "  the indexer is stalled, waiting: $(jq -c . <<< "$stall")"
            stalled=1
        elif [ "$seq" = "$idx" ]; then
            [ "$(roots_signature sequencer)" = "$(roots_signature indexer)" ] \
                || die "the indexer lists other roots than the sequencer"
            seq_x=$(zone_field sequencer '.superseded_count + .parked_count')
            idx_x=$(zone_field indexer '.superseded_count + .parked_count')
            [ "$seq_x" = "$idx_x" ] \
                || die "the readers exclude different batches: sequencer superseded+parked $seq_x, indexer $idx_x"
            say "indexer agrees with the sequencer: $seq, superseded/parked $(zone_field sequencer '"\(.superseded_count)/\(.parked_count)"') vs $(zone_field indexer '"\(.superseded_count)/\(.parked_count)"')${stalled:+, after a stall that passed}"
            return 0
        fi
        [ $((i % 30)) -eq 0 ] && say "  waiting for the indexer: sequencer $seq, indexer $idx"
        sleep 1
    done
    [ "$stall" = "null" ] || die "the indexer is still stalled after $limit s: $stall"
    die "the indexer does not agree within $limit s: sequencer $seq, indexer $idx"
}
