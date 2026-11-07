#!/usr/bin/env bash
# e2e/lib.sh: shared by the e2e scripts. Source it after reading a networks env file.
#
# Needs: curl, jq, python3. State and secrets live under E2E_STATE (default
# e2e/state/<NETWORK>, gitignored): the payer wallet, state.json.

set -euo pipefail

say() { printf '%s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

E2E_STATE="${E2E_STATE:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/state/${NETWORK:?NETWORK unset}}"
STATE_JSON="$E2E_STATE/state.json"
PAYER_WALLET="$E2E_STATE/payer-wallet"
mkdir -p "$E2E_STATE"
chmod 700 "$E2E_STATE"
[ -f "$STATE_JSON" ] || echo '{}' > "$STATE_JSON"

state_get() { jq -r --arg k "$1" '.[$k] // empty' "$STATE_JSON"; }
state_set() {
    jq --arg k "$1" --arg v "$2" '.[$k] = $v' "$STATE_JSON" > "$STATE_JSON.tmp" \
        && mv "$STATE_JSON.tmp" "$STATE_JSON"
}
state_del() {
    jq --arg k "$1" 'del(.[$k])' "$STATE_JSON" > "$STATE_JSON.tmp" && mv "$STATE_JSON.tmp" "$STATE_JSON"
}

# ---- chain reads ---------------------------------------------------------------------------

lez_rpc() {
    curl -sS -m 30 -X POST -H 'content-type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":${2:-[]}}" "$LEZ_SEQUENCER_URL"
}
lez_balance() { lez_rpc getAccountBalance "[\"$1\"]" | jq -r '.result // empty'; }
bedrock() { curl -sS -m 60 "$@"; }
bedrock_info() { bedrock "http://$BEDROCK_HTTP/cryptarchia/info" | jq '.cryptarchia_info'; }
bedrock_wallet() { bedrock "http://$BEDROCK_HTTP/wallet/$1/balance"; }
zone() { curl -sS -m 30 "http://$ZONE_HTTP$1"; }

# A base58 account id as the JSON array of its 32 bytes: the deposit metadata is
# borsh(DepositMetadata { recipient_id }), which is the raw id.
account_bytes_json() {
    python3 - "$1" <<'EOF'
import json, sys
alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
n = 0
for c in sys.argv[1]:
    n = n * 58 + alphabet.index(c)
print(json.dumps(list(n.to_bytes(32, "big"))))
EOF
}

# Integer compare beyond what bash carries (balances are u128).
ge() { python3 -c 'import sys; sys.exit(0 if int(sys.argv[1]) >= int(sys.argv[2]) else 1)' "$1" "$2"; }

# A lez-rln binary, debug build first, release second.
lez_rln_bin() {
    local name="$1" candidate
    for candidate in "$LEZ_RLN_CHECKOUT/lez-rln/target/debug/$name" \
                     "$LEZ_RLN_CHECKOUT/lez-rln/target/release/$name"; do
        [ -x "$candidate" ] && { echo "$candidate"; return 0; }
    done
    die "no $name under $LEZ_RLN_CHECKOUT/lez-rln/target: cargo build --bin $name there (LEZ_RLN_CHECKOUT)"
}

# The environment the lez-rln binaries read: wallet, payer, registry.
lez_rln_env() {
    export LEE_WALLET_HOME_DIR="$PAYER_WALLET"
    LEZ_RLN_PAYER="$(state_get payer)"
    export LEZ_RLN_PAYER
    export LEZ_RLN_TREE_ID_HEX="$TREE_ID"
    export LEZ_RLN_REGISTRATION_PROGRAM_ID="$PROGRAM_ACCOUNT_ID"
}
