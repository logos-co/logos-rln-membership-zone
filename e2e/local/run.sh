#!/usr/bin/env bash
# e2e/local/run.sh: the RLN Membership Zone end to end on one machine, everything under this
# folder: a Bedrock dev node in docker, a LEZ sequencer built from source, the registration
# program, the Zone sequencer and an indexer, then every scenario (the flow of e2e/scenarios/
# and the failure cases of e2e/local/scenarios/). Nothing costs LGO, finality takes seconds.
# See e2e/local/README.md.
#
#   bash e2e/local/run.sh all              clean-logs, prepare, build, bedrock, up, zone, scenarios, logs (the stack stays up)
#   bash e2e/local/run.sh clean-logs       empty e2e/local/logs/ (the zone logs append across runs otherwise)
#   bash e2e/local/run.sh prepare          check the tools; clone LEZ v0.3.2 and lez-rln into e2e/local/
#   bash e2e/local/run.sh build            lez-rln guest (docker) + host bins, the Zone release bins
#   bash e2e/local/run.sh bedrock          the Bedrock dev node (docker), via stack.sh
#   bash e2e/local/run.sh up               stack.sh: payer, LEZ sequencer, program, zone.env
#   bash e2e/local/run.sh zone             start the Zone sequencer (:3000) and a Zone indexer (:3001)
#   bash e2e/local/run.sh scenarios [n..]  every scenario in order (or the ones named): the flow, then the failures
#   bash e2e/local/run.sh status           where everything stands
#   bash e2e/local/run.sh logs             collect every component's log under e2e/local/logs/
#   bash e2e/local/run.sh down             stop everything, wipe the chain and the state, keep e2e/local/logs/
#   bash e2e/local/run.sh purge            down, then remove the clones and their build output too (logs stay)
#
# Layout (gitignored): lez/ (LEZ v0.3.2 clone), lez-rln/ (the program and its bins), state/
# (payer wallet, state.json, LEZ sequencer home, zone data per role), logs/. The Zone builds in
# the repo this folder sits in. stack.sh, ctl.sh, local.env and the scenarios run with
# LEZ_CHECKOUT, LEZ_RLN_CHECKOUT and E2E_STATE pointed here (their defaults already).
#
# Knobs (environment): LEZ_SRC and LEZ_RLN_SRC (where to clone from: a URL or a local checkout;
# default GitHub), LEZ_RLN_BRANCH (default feat/rln-zone-events, the deployed program),
# RUSTUP_TOOLCHAIN (the LEZ repos pin rustc 1.98.1; set an installed toolchain here to build
# with it instead of letting rustup download the pin), BEDROCK_PAUSE_SEC (bedrock-pause
# scenario, default 90; 780 or more exercises the 10-minute silence alarm).
#
# Scenarios, in the order `scenarios` runs them (described in e2e/README.md and here):
#   register proof slash             the optimistic flow on the sequencer (e2e/scenarios/)
#   indexer-agrees                   a second reader judges every batch the same (e2e/local/scenarios/ from here)
#   sequencer-restart                sequencer down while LEZ and Bedrock move; reconcile, catch up
#   sequencer-kill                   SIGKILL right after a registration; one batch, nothing lost
#   indexer-rebuild                  REBUILD_TREE on the indexer ends where it was
#   publish-retry                    unfunded sequencer cannot publish; publishes once funded
#   lez-stop                         LEZ sequencer down: wait; back: catch up
#   bedrock-pause                    Bedrock node paused (BEDROCK_PAUSE_SEC); resume
#   wrong-batch-correction           a rogue sequencer publishes a wrong batch; correction

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E="$(cd "$HERE/.." && pwd)"
ZONE_REPO="$(cd "$E2E/.." && pwd)"
LEZ_SRC="${LEZ_SRC:-https://github.com/logos-blockchain/logos-execution-zone.git}"
LEZ_RLN_SRC="${LEZ_RLN_SRC:-https://github.com/logos-co/logos-lez-rln.git}"
LEZ_RLN_BRANCH="${LEZ_RLN_BRANCH:-feat/rln-zone-events}"

export LEZ_CHECKOUT="$HERE/lez"
export LEZ_RLN_CHECKOUT="$HERE/lez-rln"
export E2E_STATE="$HERE/state"
export CARGO_TERM_COLOR=never
LOGS="$HERE/logs"
export ZONE_LOG_DIR="$LOGS"
STACK="$HERE/stack.sh"
LOCAL_ENV="$HERE/local.env"
mkdir -p "$LOGS" "$E2E_STATE"

say() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
die() { say "error: $*"; exit 1; }

# The e2e library and the local process control, on this folder's state.
load_e2e() {
    # shellcheck source=/dev/null
    . "$LOCAL_ENV"
    # shellcheck source=/dev/null
    . "$E2E/lib.sh"
    # shellcheck source=/dev/null
    . "$HERE/ctl.sh"
}

step_check() {
    local tool
    for tool in git docker cargo jq python3 curl lsof; do
        command -v "$tool" >/dev/null || die "$tool is needed"
    done
    docker info >/dev/null 2>&1 || die "docker is not running"
    cargo risczero --version >/dev/null 2>&1 \
        || die "cargo risczero is needed for the lez-rln guest (rzup install cargo-risczero, https://dev.risczero.com/api/zkvm/install)"
    say "tools: git, docker, cargo, cargo risczero, jq, python3, curl, lsof${RUSTUP_TOOLCHAIN:+; RUSTUP_TOOLCHAIN=$RUSTUP_TOOLCHAIN}"
}

# The dev node's epoch is 100 slots as shipped (epoch_config 3+3+4, times security_param 5
# over the activation coefficient 1/2). Bedrock re-prices storage gas at every epoch
# rotation and a transaction must pay the price of the epoch that mines it, so every
# inscription pending across a rotation is dropped ("1 removed" in the node log); LEZ v0.3.2
# then waits stale_refund_slots (300) and rewinds. Every 100 s that stalls the stack for 5 min.
# 108+108+144 = 360 makes the epoch 3600 slots (one hour) at the same finality (5 s).
patch_dev_node_epoch() {
    local f="$LEZ_CHECKOUT/bedrock/deployment-settings.yaml"
    if grep -q 'epoch_stake_distribution_stabilization: 3$' "$f"; then
        sed -e 's/epoch_stake_distribution_stabilization: 3$/epoch_stake_distribution_stabilization: 108/' \
            -e 's/epoch_period_nonce_buffer: 3$/epoch_period_nonce_buffer: 108/' \
            -e 's/epoch_period_nonce_stabilization: 4$/epoch_period_nonce_stabilization: 144/' "$f" > "$f.tmp"
        mv "$f.tmp" "$f"
        say "lez: dev node epoch set to 3600 slots (bedrock/deployment-settings.yaml)"
    fi
}

# clone_url <url or local path>: a local checkout as file://, so `--depth 1` holds (git ignores
# it for a plain path and copies the whole history).
clone_url() {
    case "$1" in
        /*|.*) echo "file://$(cd "$1" && pwd)" ;;
        *) echo "$1" ;;
    esac
}

step_prepare() {
    step_check
    if [ -d "$LEZ_CHECKOUT/.git" ]; then
        say "lez: $(git -C "$LEZ_CHECKOUT" describe --tags --always) at $LEZ_CHECKOUT"
    else
        say "lez: cloning v0.3.2 from $LEZ_SRC"
        git clone -q --depth 1 --branch v0.3.2 --single-branch "$(clone_url "$LEZ_SRC")" "$LEZ_CHECKOUT"
    fi
    patch_dev_node_epoch
    if [ -d "$LEZ_RLN_CHECKOUT/.git" ]; then
        say "lez-rln: $(git -C "$LEZ_RLN_CHECKOUT" log --oneline -1) at $LEZ_RLN_CHECKOUT"
    else
        say "lez-rln: cloning $LEZ_RLN_BRANCH from $LEZ_RLN_SRC"
        git clone -q --depth 1 --branch "$LEZ_RLN_BRANCH" --single-branch "$(clone_url "$LEZ_RLN_SRC")" "$LEZ_RLN_CHECKOUT"
    fi
}

step_build() {
    local guest="$LEZ_RLN_CHECKOUT/lez-rln/methods/guest/target/riscv32im-risc0-zkvm-elf/docker/rln_registration.bin"
    [ -d "$LEZ_RLN_CHECKOUT/lez-rln" ] || die "no lez-rln checkout: run prepare first"
    cd "$LEZ_RLN_CHECKOUT/lez-rln"
    if [ -f "$guest" ]; then
        say "lez-rln guest: present ($(wc -c < "$guest" | tr -d ' ') bytes)"
    else
        say "lez-rln guest: cargo risczero build (docker, reproducible, long), log $LOGS/build-lez-rln-guest.log"
        cargo risczero build --manifest-path methods/guest/Cargo.toml > "$LOGS/build-lez-rln-guest.log" 2>&1 \
            || die "guest build failed: see $LOGS/build-lez-rln-guest.log"
        [ -f "$guest" ] || die "guest build left no $guest"
        say "lez-rln guest: built"
    fi
    say "lez-rln host bins: cargo build --release, log $LOGS/build-lez-rln-host.log"
    cargo build --release --bins > "$LOGS/build-lez-rln-host.log" 2>&1 \
        || die "lez-rln host build failed: see $LOGS/build-lez-rln-host.log"
    ls target/release/{mint_payer,run_setup,register_member,slash_member} >/dev/null
    say "lez-rln host bins: built"
    cd "$ZONE_REPO"
    say "zone: cargo build --release --bins, log $LOGS/build-zone.log"
    cargo build --release --bins > "$LOGS/build-zone.log" 2>&1 || die "zone build failed: see $LOGS/build-zone.log"
    ls target/release/{zone,generate_proof,verify_proof} >/dev/null
    say "zone: built"
}

stack() { bash "$STACK" "$@" 2>&1 | tee -a "$LOGS/stack.log"; }

# The LEZ sequencer builds from source on the first `up` (long); its log is state/lez-home/.
step_bedrock() { stack bedrock; }

step_up() {
    stack check
    stack payer
    stack lez
    stack program
    stack zone-env
}

zone_summary() { zone_at "$1" / 2>/dev/null | jq -c '{mode, batch_count, in_flight, stall, lez: .lez.adopted_block_id, newest: .newest_published}' || echo 'no answer'; }

step_zone() {
    load_e2e
    zone_start sequencer
    zone_start indexer
    zone_wait_http sequencer 300
    zone_wait_http indexer 300
    say "zone sequencer /: $(zone_summary sequencer)"
    say "zone indexer /:   $(zone_summary indexer)"
}

SCENARIOS="register proof slash indexer-agrees sequencer-restart sequencer-kill indexer-rebuild \
publish-retry lez-stop bedrock-pause wrong-batch-correction"

# scenario_script <name>: the flow scenarios live in e2e/scenarios/, the failure cases here.
scenario_script() {
    if [ -f "$E2E/scenarios/$1.sh" ]; then echo "$E2E/scenarios/$1.sh"
    elif [ -f "$HERE/scenarios/$1.sh" ]; then echo "$HERE/scenarios/$1.sh"
    else die "no scenario $1 in $E2E/scenarios or $HERE/scenarios"
    fi
}

step_scenarios() {
    local list="${*:-$SCENARIOS}" s script log rc passed=0 failed="" skipped="" start
    for s in $list; do
        script=$(scenario_script "$s")
        log="$LOGS/scenario-$s.log"
        say "scenario $s, log $log"
        start=$(date +%s)
        rc=0
        bash "$script" "$LOCAL_ENV" > "$log" 2>&1 || rc=$?
        case "$rc" in
            0) passed=$((passed + 1)); say "  PASS in $(( $(date +%s) - start )) s: $(tail -n 1 "$log")" ;;
            3) skipped="$skipped $s"; say "  SKIP: $(grep -m1 skipped "$log" || tail -n 1 "$log")" ;;
            *) failed="$failed $s"; say "  FAIL (exit $rc) in $(( $(date +%s) - start )) s: $(grep -m1 'error:' "$log" || tail -n 1 "$log")" ;;
        esac
    done
    say "scenarios: $passed passed${failed:+, failed:$failed}${skipped:+, skipped:$skipped}"
    [ -z "$failed" ]
}

step_status() {
    stack status
    load_e2e
    say "zone sequencer: pid $(zone_role_pid sequencer) $(zone_summary sequencer)"
    say "zone indexer:   pid $(zone_role_pid indexer) $(zone_summary indexer)"
}

step_logs() {
    load_e2e
    (cd "$LEZ_CHECKOUT/bedrock" && docker compose logs --no-color > "$LOGS/bedrock-node.log" 2>&1) || true
    cp -f "$E2E_STATE/lez-stack.log" "$LOGS/lez-sequencer-stack.log" 2>/dev/null || true
    cp -f "$E2E_STATE/lez-home/sequencer.log" "$LOGS/lez-sequencer.log" 2>/dev/null || true
    cp -f "$E2E_STATE/state.json" "$LOGS/state.json" 2>/dev/null || true
    cp -f "$E2E_STATE/zone.env" "$LOGS/zone.env" 2>/dev/null || true
    zone_at sequencer / > "$LOGS/zone-sequencer-summary.json" 2>/dev/null || true
    zone_at indexer / > "$LOGS/zone-indexer-summary.json" 2>/dev/null || true
    zone_at sequencer /roots/50 > "$LOGS/zone-sequencer-roots.json" 2>/dev/null || true
    zone_at indexer /roots/50 > "$LOGS/zone-indexer-roots.json" 2>/dev/null || true
    say "logs: $(ls "$LOGS" | tr '\n' ' ')"
}

step_down() {
    load_e2e
    zone_stop sequencer || true
    zone_stop indexer || true
    zone_stop rogue -KILL || true
    stack down -v || true
    rm -rf "$E2E_STATE"
    say "down: the stack is stopped and its state wiped; logs stay in $LOGS"
}

step_purge() {
    step_down
    rm -rf "$LEZ_CHECKOUT" "$LEZ_RLN_CHECKOUT"
    say "purge: clones removed; $LOGS kept"
}

# Every log of the previous run goes: the zone logs append across restarts and runs otherwise.
step_clean_logs() {
    find "$LOGS" -mindepth 1 -delete
    say "clean-logs: $LOGS emptied"
}

# `all` writes its own output to logs/run-all.log too, after the clean.
step_all() {
    step_clean_logs
    exec > >(tee -a "$LOGS/run-all.log") 2>&1
    step_prepare
    step_build
    step_bedrock
    step_up
    step_zone
    local rc=0
    step_scenarios || rc=$?
    step_logs
    say "all: done, the stack is still up (status / down / purge); scenarios exit $rc"
    return "$rc"
}

cmd="${1:-}"
shift || true
case "$cmd" in
    all) step_all ;;
    clean-logs) step_clean_logs ;;
    prepare) step_prepare ;;
    build) step_build ;;
    bedrock) step_bedrock ;;
    up) step_up ;;
    zone) step_zone ;;
    scenarios) step_scenarios "$@" ;;
    status) step_status ;;
    logs) step_logs ;;
    down) step_down ;;
    purge) step_purge ;;
    *) sed -n 2,19p "$0"; exit 2 ;;
esac
