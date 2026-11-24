#!/usr/bin/env bash
# e2e/local/stack.sh: the whole stack on one machine, the `local` e2e target.
#
#   bash e2e/local/stack.sh [check|bedrock|payer|lez|program|zone-env|all|status|down [-v]]
#
# The stack: a Bedrock dev node in docker (the LEZ checkout's bedrock/: one validator, 1 s
# slots, finality in seconds with security_param 5, the API on 18080, a funded wallet key),
# a LEZ sequencer built from the checkout and inscribing into that node (channel 0101..01,
# the debug config with the payer funded at genesis and a short block time), the lez-rln
# registration program deployed on it, and the Zone node reading all of it. Every step checks
# before it acts, so a re-run resumes; the scenarios (register.sh, slash.sh, proof.sh) then
# take e2e/local/local.env like any target.
#
# Steps, in order (`all` runs the first six):
#   check     the tools, the LEZ checkout (v0.3.2, bedrock/, tools/run_sequencer.sh), the
#             lez-rln bins and the guest binary run_setup deploys
#   bedrock   the dev node: resolve its binary, `docker compose up`, wait for `Following`
#   payer     mint the LEZ payer wallet (it is funded at the LEZ genesis by `lez`)
#   lez       the LEZ sequencer in the background (log under the state dir); the first run
#             builds it, long
#   program   deploy the registration program and initialize the registry from the payer
#             (lez-rln run_setup); records program, tree and first block in state.json
#   zone-env  write zone.env for the Zone node and print how to run it
#   status    what runs where
#   down      stop the LEZ sequencer and the dev node; `down -v` also wipes the chain, the
#             sequencer home, the payer wallet and state.json: a fresh stack next time

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
. "$ROOT/local.env"
# shellcheck source=/dev/null
. "$ROOT/../lib.sh"

STEP="${1:-all}"
shift || true

LEZ_HOME="$E2E_STATE/lez-home"                  # the sequencer's home: RocksDB, keys, ports, log
LEZ_CONFIG="$E2E_STATE/sequencer_config.json"   # the debug config, patched
LEZ_LOG="$E2E_STATE/lez-stack.log"
ZONE_ENV="$E2E_STATE/zone.env"
LEZ_BLOCK_TIME="${LEZ_BLOCK_TIME:-2s}"           # CLOCK_50 goes live after 50 blocks
LEZ_GENESIS_BALANCE="${LEZ_GENESIS_BALANCE:-10000000000000}"
# The dev node's funded wallet key (bedrock/node-config.yaml, `funding_pk`): pays the LEZ
# inscriptions and the Zone's batches.
NODE_WALLET_PK=2e03b2eff5a45478e7e79668d2a146cf2c5c7925bce927f2b1c67f2ab4fc0d26
GUEST_BIN="$LEZ_RLN_CHECKOUT/lez-rln/methods/guest/target/riscv32im-risc0-zkvm-elf/docker/rln_registration.bin"

lez_tag() {
    git -C "$LEZ_CHECKOUT" describe --tags --exact-match 2>/dev/null \
        || git -C "$LEZ_CHECKOUT" rev-parse --short HEAD
}
node_phase() { bedrock "http://$BEDROCK_HTTP/cryptarchia/info" 2>/dev/null | jq -r '.phase // empty'; }
lez_last_block() { lez_rpc getLastBlockId 2>/dev/null | jq -r '.result // empty'; }
lez_channel_tip() { bedrock "http://$BEDROCK_HTTP/channel/$LEZ_CHANNEL_ID" 2>/dev/null | jq -r '.tip_slot // empty'; }

step_check() {
    local tool
    for tool in docker cargo jq python3 curl lsof; do
        command -v "$tool" >/dev/null || die "$tool is needed"
    done
    docker info >/dev/null 2>&1 || die "docker is not running"
    [ -f "$LEZ_CHECKOUT/bedrock/docker-compose.yml" ] && [ -f "$LEZ_CHECKOUT/tools/run_sequencer.sh" ] \
        || die "no LEZ checkout at $LEZ_CHECKOUT: git clone --branch v0.3.2 https://github.com/logos-blockchain/logos-execution-zone $LEZ_CHECKOUT (LEZ_CHECKOUT)"
    [ "$(lez_tag)" = v0.3.2 ] || say "warning: the LEZ checkout is at $(lez_tag), the Zone pins v0.3.2"
    lez_rln_bin mint_payer >/dev/null
    lez_rln_bin run_setup >/dev/null
    lez_rln_bin register_member >/dev/null
    lez_rln_bin slash_member >/dev/null
    [ -f "$GUEST_BIN" ] \
        || die "no guest binary at $GUEST_BIN: in $LEZ_RLN_CHECKOUT/lez-rln run \`cargo risczero build --manifest-path methods/guest/Cargo.toml\` (docker)"
    zone_bin generate_proof >/dev/null
    say "check: ok (LEZ $(lez_tag), lez-rln bins, guest binary, zone bins)"
}

step_bedrock() {
    if [ -n "$(node_phase)" ]; then
        say "bedrock: the dev node answers on $BEDROCK_HTTP"
    else
        say "bedrock: resolving the node binary and starting the dev node (docker)"
        (cd "$LEZ_CHECKOUT" && bash bedrock/tools/resolve_bedrock_node_in_docker.sh)
        (cd "$LEZ_CHECKOUT/bedrock" && docker compose up --build -d)
    fi
    local i phase
    for i in $(seq 1 120); do
        phase=$(node_phase)
        [ "$phase" = Following ] && break
        [ $((i % 12)) -eq 0 ] && say "  waiting for the dev node ($i x 5 s): phase ${phase:-no answer}"
        sleep 5
    done
    [ "$(node_phase)" = Following ] || die "the dev node is not Following after 10 minutes"
    say "bedrock: $(bedrock_info | jq -c '{slot, height, lib_slot, state}')"
}

step_payer() {
    payer_mint
}

step_lez() {
    local payer
    payer=$(state_get payer)
    [ -n "$payer" ] || die "no payer: run the payer step first, the LEZ genesis funds it"
    if [ -n "$(lez_last_block)" ]; then
        say "lez: a LEZ sequencer answers on $LEZ_SEQUENCER_URL (block $(lez_last_block))"
    else
        [ "$(node_phase)" = Following ] || die "the dev node is not Following: run the bedrock step"
        say "lez: writing the sequencer config (payer $payer funded at genesis, blocks every $LEZ_BLOCK_TIME)"
        python3 - "$LEZ_CHECKOUT/lez/sequencer/service/configs/debug/sequencer_config.json" "$LEZ_CONFIG" \
            "$payer" "$LEZ_GENESIS_BALANCE" "$LEZ_BLOCK_TIME" "http://$BEDROCK_HTTP" "$LEZ_CHANNEL_ID" <<'PY'
import json, sys
src, dst, payer, balance, block_time, node_url, channel = sys.argv[1:8]
cfg = json.load(open(src))
cfg["genesis"] = [e for e in cfg["genesis"] if e.get("supply_account", {}).get("account_id") != payer]
cfg["genesis"].append({"supply_account": {"account_id": payer, "balance": int(balance)}})
cfg["block_create_timeout"] = block_time
cfg["bedrock_config"]["node_url"] = node_url
cfg["bedrock_config"]["channel_id"] = channel
json.dump(cfg, open(dst, "w"), indent=2)
PY
        mkdir -p "$LEZ_HOME"
        # Default features only (as the LEZ devnet compose): `prove` pulls the risc0 Metal
        # kernels on macOS, which need Xcode; RISC0_DEV_MODE=1 skips proving anyway. `rpc`
        # is the default feature, named here because run_sequencer.sh expands an empty
        # features array, which bash 3.2 (macOS) rejects under `set -u`.
        say "lez: starting the sequencer from $LEZ_CHECKOUT (home $LEZ_HOME, log $LEZ_LOG); the first run builds it"
        # `;` not `&&` before nohup: `cd && cmd &` backgrounds the whole list, and that
        # list's shell would wait for the sequencer forever, holding this script's pipes.
        (
            cd "$LEZ_CHECKOUT" || exit 1
            LEZ_CONFIG="$LEZ_CONFIG" LEZ_NODE="http://$BEDROCK_HTTP" LEZ_SEQUENCER="$LEZ_SEQUENCER_URL" \
               LEZ_FEATURES="${LEZ_FEATURES:-rpc}" RISC0_DEV_MODE="${RISC0_DEV_MODE:-1}" \
               RUST_LOG="${LEZ_RUST_LOG:-warn,sequencer_service=info}" \
               nohup bash tools/run_sequencer.sh "$LEZ_HOME" >> "$LEZ_LOG" 2>&1 < /dev/null &
        )
    fi
    local i
    for i in $(seq 1 360); do
        [ -n "$(lez_last_block)" ] && break
        pgrep -f "tools/run_sequencer.sh $LEZ_HOME" >/dev/null || die "the LEZ sequencer exited: see $LEZ_LOG"
        [ $((i % 12)) -eq 0 ] && say "  waiting for the sequencer RPC ($i x 10 s; building on the first run): $(tail -n 1 "$LEZ_LOG" 2>/dev/null | cut -c1-100)"
        sleep 10
    done
    [ -n "$(lez_last_block)" ] || die "the LEZ sequencer does not answer after an hour: see $LEZ_LOG"
    for i in $(seq 1 60); do
        [ -n "$(lez_channel_tip)" ] && break
        [ $((i % 6)) -eq 0 ] && say "  waiting for the LEZ channel on the dev node ($i x 5 s)"
        sleep 5
    done
    [ -n "$(lez_channel_tip)" ] || die "the LEZ channel $LEZ_CHANNEL_ID is not on the dev node: see $LEZ_LOG"
    say "lez: block $(lez_last_block), channel tip at slot $(lez_channel_tip)"
}

step_program() {
    local payer run_setup tree first out program
    if [ -n "$(state_get program_account_id)" ]; then
        say "program: $(state_get program_account_id) (tree $(state_get tree_id), first LEZ block $(state_get first_lez_block_id))"
        return 0
    fi
    payer=$(state_get payer)
    [ -n "$payer" ] || die "no payer: run the payer step first"
    [ -n "$(lez_last_block)" ] || die "no LEZ sequencer on $LEZ_SEQUENCER_URL: run the lez step"
    ge "$(lez_balance "$payer")" "$PAYER_MIN" || die "payer $payer holds $(lez_balance "$payer"): not in this chain's genesis (down -v, then all)"
    run_setup=$(lez_rln_bin run_setup)
    tree=$(state_get tree_id)
    if [ -z "$tree" ]; then
        tree=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
        state_set tree_id "$tree"
    fi
    # The block before the deploy: nothing of the registry is earlier.
    first=$(lez_last_block)
    write_wallet_config
    say "program: deploying and initializing the registry (tree $tree) from $payer"
    # run_setup reads the guest binary by a path relative to lez-rln/.
    out=$(cd "$LEZ_RLN_CHECKOUT/lez-rln" \
        && HOME="$PAYER_WALLET" LEE_WALLET_HOME_DIR="$PAYER_WALLET" LEZ_RLN_PAYER="$payer" \
           LEZ_RLN_TREE_ID_HEX="$tree" LEZ_RLN_GAS_LIMIT="$LEZ_RLN_GAS_LIMIT" \
           LEZ_RLN_ACTIVE_DURATION_SECS="${LEZ_RLN_ACTIVE_DURATION_SECS:-3600}" \
           LEZ_RLN_GRACE_PERIOD_DURATION_SECS="${LEZ_RLN_GRACE_PERIOD_DURATION_SECS:-600}" \
           "$run_setup") || die "run_setup failed"
    printf '%s\n' "$out" >&2
    program=$(printf '%s\n' "$out" | sed -n 's/^Registration program: //p' | tail -n 1)
    [ -n "$program" ] || die "run_setup printed no 'Registration program:' line"
    state_set program_account_id "$program"
    state_set first_lez_block_id "$first"
    say "program: $program, tree $tree, first LEZ block $first"
}

step_zone_env() {
    local program tree first
    program=$(state_get program_account_id); tree=$(state_get tree_id); first=$(state_get first_lez_block_id)
    [ -n "$program" ] && [ -n "$tree" ] && [ -n "$first" ] || die "no program on record: run the program step"
    cat > "$ZONE_ENV" <<EOF
# The Zone node on the local stack, written by e2e/local/stack.sh zone-env.
MODE=sequencer
NODE_URL=http://$BEDROCK_HTTP
FUNDING_PK=$NODE_WALLET_PK
MAX_TX_FEE=1000000
PRIORITY_FEE_PERCENT=12
CHANNEL_NAME=rln-membership-zone:local
LEZ_CHANNEL_ID=$LEZ_CHANNEL_ID
LEZ_SEQUENCER_URL=$LEZ_SEQUENCER_URL
PROGRAM_ACCOUNT_ID=$program
TREE_ID=$tree
FIRST_LEZ_BLOCK_ID=$first
POLL_SEC=5
HEARTBEAT_SEC=300
DATA_DIR=data-local
HTTP_ADDR=127.0.0.1:3000
EOF
    say "zone-env: $ZONE_ENV"
    say "run the Zone sequencer:  set -a; . $ZONE_ENV; set +a; cargo run"
    say "and an indexer beside it: MODE=indexer DATA_DIR=data-local-indexer HTTP_ADDR=127.0.0.1:3001 cargo run"
    say "then the scenarios:       bash e2e/scenarios/register.sh e2e/local/local.env (or bash e2e/local/run.sh scenarios)"
}

step_status() {
    say "bedrock $BEDROCK_HTTP: $(bedrock_info 2>/dev/null | jq -c '{slot, height, lib_slot, state}' || echo down)"
    say "LEZ channel tip slot: $(lez_channel_tip || true)"
    say "LEZ sequencer $LEZ_SEQUENCER_URL: block $(lez_last_block || echo down)"
    say "payer: $(state_get payer) balance $(lez_balance "$(state_get payer)" 2>/dev/null || echo ?)"
    say "program: $(state_get program_account_id) tree $(state_get tree_id) first block $(state_get first_lez_block_id)"
    say "zone $ZONE_HTTP: $(zone / 2>/dev/null | jq -c '{mode, batch_count, in_flight, lez: .lez.adopted_block_id, head: .head.lez_block_id}' || echo down)"
}

step_down() {
    local pids
    pids="$(pgrep -f "tools/run_sequencer.sh $LEZ_HOME" || true) $(lsof -ti tcp:3040 2>/dev/null || true)"
    pids="${pids## }"; pids="${pids%% }"
    if [ -n "$pids" ]; then
        say "down: stopping the LEZ sequencer (pids $pids)"
        # shellcheck disable=SC2086
        kill $pids 2>/dev/null || true
    fi
    [ -d "$LEZ_CHECKOUT/bedrock" ] || { say "down: no LEZ checkout at $LEZ_CHECKOUT, no dev node to stop"; return 0; }
    if [ "${1:-}" = "-v" ]; then
        (cd "$LEZ_CHECKOUT/bedrock" && docker compose down -v)
        rm -rf "$LEZ_HOME" "$LEZ_CONFIG" "$PAYER_WALLET" "$STATE_JSON" "$ZONE_ENV"
        say "down: the chain, the sequencer home, the payer wallet and state.json are gone; the Zone's data-local must go too (rm -rf data-local data-local-indexer)"
    else
        (cd "$LEZ_CHECKOUT/bedrock" && docker compose down)
        say "down: stopped; the chain and the state stay (down -v wipes them)"
    fi
}

case "$STEP" in
    check) step_check ;;
    bedrock) step_bedrock ;;
    payer) step_payer ;;
    lez) step_lez ;;
    program) step_program ;;
    zone-env) step_zone_env ;;
    all) step_check; step_bedrock; step_payer; step_lez; step_program; step_zone_env ;;
    status) step_status ;;
    down) step_down "$@" ;;
    *) die "unknown step $STEP (check|bedrock|payer|lez|program|zone-env|all|status|down [-v])" ;;
esac
