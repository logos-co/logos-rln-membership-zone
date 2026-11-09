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

# ---- memberships -----------------------------------------------------------------------------
# Every registration this harness made, by leaf index: `memberships.<leaf>` =
# {id_commitment, identity_secret, slashed}. The secret is born in `register_member` and
# cannot be read back from anywhere, so it lives here.

membership_set() {
    jq --arg leaf "$1" --arg id "$2" --arg secret "$3" \
        '.memberships[$leaf] = {id_commitment: $id, identity_secret: $secret, slashed: false}' \
        "$STATE_JSON" > "$STATE_JSON.tmp" && mv "$STATE_JSON.tmp" "$STATE_JSON"
}
membership_mark_slashed() {
    jq --arg leaf "$1" '.memberships[$leaf].slashed = true' "$STATE_JSON" > "$STATE_JSON.tmp" \
        && mv "$STATE_JSON.tmp" "$STATE_JSON"
}
# "<leaf> <id_commitment> <identity_secret>" of the leaf given, or of the newest one not slashed.
membership_pick() {
    if [ -n "${1:-}" ]; then
        jq -r --arg leaf "$1" '.memberships[$leaf] | select(. != null) | "\($leaf) \(.id_commitment) \(.identity_secret)"' "$STATE_JSON"
    else
        jq -r '.memberships // {} | to_entries | map(select(.value.slashed == false)) | sort_by(.key | tonumber) | last | select(. != null) | "\(.key) \(.value.id_commitment) \(.value.identity_secret)"' "$STATE_JSON"
    fi
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

# A zone binary (`generate_proof`, `verify_proof`), debug build first, release second.
zone_bin() {
    local name="$1" repo candidate
    repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    for candidate in "$repo/target/debug/$name" "$repo/target/release/$name"; do
        [ -x "$candidate" ] && { echo "$candidate"; return 0; }
    done
    die "no $name under $repo/target: cargo build --bins"
}

# The environment the lez-rln binaries read: wallet, payer, registry.
lez_rln_env() {
    export LEE_WALLET_HOME_DIR="$PAYER_WALLET"
    LEZ_RLN_PAYER="$(state_get payer)"
    export LEZ_RLN_PAYER
    export LEZ_RLN_TREE_ID_HEX="$TREE_ID"
    export LEZ_RLN_REGISTRATION_PROGRAM_ID="$PROGRAM_ACCOUNT_ID"
}
