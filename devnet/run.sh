#!/bin/bash
# Local single-node Logos Blockchain ("devnet" for this repo): the upstream standalone
# deployment, no peers, own block production, genesis-funded dev keys. Everything it
# writes stays under devnet/ (gitignored).
#
#   devnet/run.sh start    download the node for the submodule tag if needed, start it
#   devnet/run.sh fund     mine PoW tickets briefly and claim them to the funding key
#   devnet/run.sh status   consensus state + funding key balance
#   devnet/run.sh stop     stop the node
#   devnet/run.sh reset    stop and wipe chain data (keeps the binary)
#
# Upstream reference: logos-blockchain/README.md ("--deployment
# standalone-deployment-config.yaml nodes/node/standalone-node-config.yaml").

set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$DIR/.." && pwd)"
SUB="$REPO/logos-blockchain"
HTTP="127.0.0.1:8080"
# Genesis-funded wallet key of the standalone config (SDP funding key, 1e11 atomic units).
# The big staker key (e3635f...) is left alone so block production keeps its stake.
FUNDING_PK="9750fa86471fddc69749aa9f8568ef6635e64d9f9aa815e8cb932cea183c8e18"

RELEASE="$(git -C "$SUB" describe --tags --exact-match)"
BIN="$DIR/bin/logos-blockchain-node"

say() { echo -e "\033[36m$*\033[0m"; }
die() { echo -e "\033[31m$*\033[0m" >&2; exit 1; }
up() { curl -s -m 3 "http://$HTTP/cryptarchia/info" >/dev/null 2>&1; }
# Match on the arguments, not the absolute path, so a node started by hand from devnet/
# with a relative `bin/...` path is found too.
pid() { pgrep -f "bin/logos-blockchain-node --deployment deployment-config.yaml" || true; }

ensure_binary() {
    if [ -x "$BIN" ] && "$BIN" --version 2>/dev/null | grep -q "logos-blockchain-node $RELEASE$"; then
        return
    fi
    local os arch asset
    case "$(uname -s)" in Darwin) os=macos ;; Linux) os=linux ;; *) die "unsupported OS" ;; esac
    case "$(uname -m)" in arm64|aarch64) arch=aarch64 ;; x86_64) arch=x86_64 ;; *) die "unsupported arch" ;; esac
    asset="logos-blockchain-node-$os-$arch-$RELEASE.tar.gz"
    say "downloading $asset"
    mkdir -p "$DIR/bin"
    curl -fsSL -o "$DIR/bin/$asset" \
        "https://github.com/logos-blockchain/logos-blockchain/releases/download/$RELEASE/$asset" \
        || die "download failed: $asset"
    tar xzf "$DIR/bin/$asset" -C "$DIR/bin" && rm "$DIR/bin/$asset"
    [ -x "$BIN" ] || die "no logos-blockchain-node in $asset"
}

cmd_start() {
    ensure_binary
    if up; then say "node already up on $HTTP"; cmd_status; return; fi
    # Configs come from the submodule so they always match the pinned release.
    cp "$SUB/nodes/node/standalone-node-config.yaml" "$DIR/node-config.yaml"
    cp "$SUB/nodes/node/standalone-deployment-config.yaml" "$DIR/deployment-config.yaml"
    mkdir -p "$DIR/logs"
    say "starting $RELEASE standalone node (HTTP $HTTP, data in devnet/)"
    (cd "$DIR" && nohup "$BIN" --deployment deployment-config.yaml node-config.yaml \
        --log-backend file --log-dir ./logs --log-level info </dev/null > node.stdout 2>&1 &)
    local _t
    for _t in $(seq 1 30); do up && break; sleep 1; done
    up || die "node did not come up, see devnet/node.stdout"
    cmd_status
}

cmd_status() {
    if ! up; then echo "node: down"; return; fi
    echo "node:    $(curl -s -m 3 "http://$HTTP/cryptarchia/info" | jq -c '.cryptarchia_info // . | {state, height, slot}')"
    echo "funding: $FUNDING_PK"
    echo "balance: $(curl -s -m 3 "http://$HTTP/wallet/$FUNDING_PK/balance")"
}

api() { curl -s -m 20 -X "$1" -H 'Content-Type: application/json' "http://$HTTP$2" ${3:+-d "$3"}; }

# Genesis gives the dev keys almost nothing (the big genesis note belongs to a faucet key
# that is not in this keystore), so fees come from PoW: mine a few tickets, claim them.
cmd_fund() {
    up || die "node is down, run: devnet/run.sh start"
    api PUT /pow/mining/start >/dev/null
    say "mining until a ticket is claimable"
    local _t n=0
    for _t in $(seq 1 60); do
        n=$(api GET /pow/rewards/claimable | jq -r '.claimable_tickets // 0')
        [ "$n" -gt 0 ] && break
        sleep 2
    done
    api PUT /pow/mining/stop >/dev/null
    [ "$n" -gt 0 ] || die "no ticket after 120 s"
    say "claiming $n ticket(s) to $FUNDING_PK"
    api POST /pow/claim "{\"claim_address\":\"$FUNDING_PK\"}"; echo
    sleep 5
    cmd_status
}

cmd_stop() {
    local p; p="$(pid)"
    [ -n "$p" ] || { echo "node not running"; return; }
    kill $p; say "stopped ($p)"
}

cmd_reset() {
    cmd_stop
    sleep 1
    rm -r "$DIR/state" "$DIR/db" "$DIR/logs" 2>/dev/null || true
    rm "$DIR/node.stdout" "$DIR/node-config.yaml" "$DIR/deployment-config.yaml" 2>/dev/null || true
    say "chain data wiped"
}

case "${1:-start}" in
    start) cmd_start ;;
    fund) cmd_fund ;;
    status) cmd_status ;;
    stop) cmd_stop ;;
    reset) cmd_reset ;;
    *) die "usage: devnet/run.sh start|fund|status|stop|reset" ;;
esac
