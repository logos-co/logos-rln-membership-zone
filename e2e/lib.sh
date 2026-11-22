#!/usr/bin/env bash
# e2e/lib.sh: shared by the e2e scripts (bash only). Source it after reading a network env file.
#
# Needs: curl, jq, python3. State and secrets live under E2E_STATE (default
# e2e/state/<NETWORK>, gitignored): the payer wallet, state.json.

set -euo pipefail

say() { printf '%s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

# This directory and the repository root.
E2E_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
REPO_DIR="$(cd "$E2E_DIR/.." && pwd)"

E2E_STATE="${E2E_STATE:-$E2E_DIR/state/${NETWORK:?NETWORK unset}}"
STATE_JSON="$E2E_STATE/state.json"
PAYER_WALLET="$E2E_STATE/payer-wallet"
mkdir -p "$E2E_STATE"
chmod 700 "$E2E_STATE"
[ -f "$STATE_JSON" ] || echo '{}' > "$STATE_JSON"

# state.json key/value: state_get <k>, state_set <k> <v>, state_del <k>.
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
# The LEZ JSON-RPC, the Bedrock node's HTTP API, the zone's HTTP API.

lez_rpc() {
    curl -sS -m 30 -X POST -H 'content-type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":${2:-[]}}" "$LEZ_SEQUENCER_URL"
}
lez_balance() { lez_rpc getAccountBalance "[\"$1\"]" | jq -r '.result // empty'; }
bedrock() { curl -sS -m 60 "$@"; }
bedrock_info() { bedrock "http://$BEDROCK_HTTP/cryptarchia/info" | jq '.cryptarchia_info'; }
bedrock_wallet() { bedrock "http://$BEDROCK_HTTP/wallet/$1/balance"; }
zone() { curl -sS -m 30 "http://$ZONE_HTTP$1"; }

# Bedrock slots since the LEZ channel's newest inscription (its tip on the node): under ~200
# LEZ inscribes as usual; thousands means LEZ produces blocks the zone cannot see on Bedrock yet.
lez_l1_lag() {
    local now tip
    now=$(bedrock_info | jq -r '.slot')
    tip=$(bedrock "http://$BEDROCK_HTTP/channel/$LEZ_CHANNEL_ID" | jq -r '.tip_slot // 0')
    echo $((now - tip))
}

# One line on where a membership stands, from the zone's /merkle_proof answer: on Bedrock
# (active or removed), or only in a LEZ block the LEZ Sequencer produced (`lez_preview`), or
# unknown; always with how long LEZ has not inscribed.
membership_status() {
    local answer="$1" lag
    lag=$(lez_l1_lag)
    printf '%s' "$answer" | jq -r --argjson lag "$lag" '
        if .active == true and .proof != null then "on Bedrock: active, registered in LEZ block \(.registered_at_lez_block_id) (tx \(.registered_lez_tx_hash[0:8])..), in a batch (root \(.proof.root[0:8])..)"
        elif .active == true then "on Bedrock: active, registered in LEZ block \(.registered_at_lez_block_id) (tx \(.registered_lez_tx_hash[0:8])..), no batch carries it yet"
        elif .removed != null then "on Bedrock: removed (\(.removed)) in LEZ block \(.removed_at_lez_block_id)"
        elif .lez_preview != null then "in LEZ block \(.lez_preview.lez_block_id) (\(.lez_preview.event), tx \(.lez_preview.tx_hash[0:8])..), NOT on Bedrock yet; LEZ last inscribed \($lag) Bedrock slots ago"
        else "not seen by the zone; LEZ last inscribed \($lag) Bedrock slots ago" end' 2>/dev/null \
        || echo "not seen by the zone; LEZ last inscribed $lag Bedrock slots ago"
}

# ---- a root on Bedrock ---------------------------------------------------------------------
# A root the zone computes is at `head.root` until a batch carries it; `/roots` lists the
# roots batches carry, each batch with its `bedrock_status` (the Zone SDK's word): `pending`
# (published, no Bedrock block yet; `/` names it `in_flight`), `adopted` (in a block) or
# `finalized` (hours later). `verify_proof` trusts every root a batch carries,
# `--finalized-only` the finalized ones.
#
# root_wait_published <jq filter over a root entry> [jq --arg ...]: polls `/roots/5` for the
# entry the filter selects until a batch carries it, says the minutes spent, and prints the
# producing batch's `bedrock_status`. Fails after 30 minutes.
root_wait_published() {
    local filter="$1"; shift
    local status="" i
    for i in $(seq 1 180); do
        status=$(zone /roots/5 | jq -r "$@" \
            ".roots[] | select((.batches | length) > 0 and ($filter)) | .batches[0].bedrock_status" | head -1)
        case "$status" in
            finalized) say "  a batch carries it, finalized on Bedrock"; echo "$status"; return 0 ;;
            adopted|pending) say "  a batch carries it ($status on Bedrock, finality hours away)"; echo "$status"; return 0 ;;
        esac
        [ $((i % 6)) -eq 0 ] && say "  no batch yet after $((i / 6)) min: the sequencer publishes on its own schedule"
        sleep 10
    done
    return 1
}

# ---- the LEZ payer -------------------------------------------------------------------------
# The wallet config in every shape a LEZ wallet has read it: `sequencer` (v0.3.1, the lez-rln
# pin), `sequencers` (v0.3.0), flat `sequencer_addr` (the module). No struct denies unknown
# fields, so one file serves all (as lez-rln's own tools/deployments do).
write_wallet_config() {
    jq -n --arg s "$LEZ_SEQUENCER_URL/" '{sequencer_addr:$s, sequencer:{sequencer_addr:$s},
        sequencers:[{sequencer_addr:$s}], seq_poll_timeout:"30s", seq_tx_poll_max_blocks:15,
        seq_poll_max_retries:10, seq_block_poll_max_amount:100, gas_limit:10000000,
        multi_sequencer_client_config:{distribution_limit:1, calibration_limit:3}}' \
        > "$PAYER_WALLET/wallet_config.json"
}

# The LEZ payer: minted once into PAYER_WALLET (lez-rln `mint_payer`) and recorded as `payer`;
# an existing one is kept, its wallet config refreshed.
payer_mint() {
    if [ -n "$(state_get payer)" ] && [ -f "$PAYER_WALLET/storage.json" ]; then
        write_wallet_config
        say "payer $(state_get payer) (wallet $PAYER_WALLET)"
        return 0
    fi
    local mint_payer payer
    mint_payer=$(lez_rln_bin mint_payer)
    mkdir -p "$PAYER_WALLET"
    write_wallet_config
    payer=$(HOME="$PAYER_WALLET" LEE_WALLET_HOME_DIR="$PAYER_WALLET" "$mint_payer") || die "mint_payer failed"
    chmod 600 "$PAYER_WALLET/storage.json"
    state_set payer "$payer"
    say "minted payer $payer (wallet $PAYER_WALLET; back it up once it holds funds)"
}

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

# A lez-rln binary, release build first, debug second.
lez_rln_bin() {
    local name="$1" candidate
    for candidate in "$LEZ_RLN_CHECKOUT/lez-rln/target/release/$name" \
                     "$LEZ_RLN_CHECKOUT/lez-rln/target/debug/$name"; do
        [ -x "$candidate" ] && { echo "$candidate"; return 0; }
    done
    die "no $name under $LEZ_RLN_CHECKOUT/lez-rln/target: cargo build --release --bin $name there (LEZ_RLN_CHECKOUT)"
}

# A zone binary (`generate_proof`, `verify_proof`), release build first, debug second.
zone_bin() {
    local name="$1" candidate
    for candidate in "$REPO_DIR/target/release/$name" "$REPO_DIR/target/debug/$name"; do
        [ -x "$candidate" ] && { echo "$candidate"; return 0; }
    done
    die "no $name under $REPO_DIR/target: cargo build --release --bins"
}

# The environment the lez-rln binaries read: wallet, payer, registry.
lez_rln_env() {
    export LEE_WALLET_HOME_DIR="$PAYER_WALLET"
    LEZ_RLN_PAYER="$(state_get payer)"
    export LEZ_RLN_PAYER
    export LEZ_RLN_TREE_ID_HEX="$TREE_ID"
    export LEZ_RLN_REGISTRATION_PROGRAM_ID="$PROGRAM_ACCOUNT_ID"
    # The execution gas a membership transaction declares (the fee reserve follows it).
    export LEZ_RLN_GAS_LIMIT="${LEZ_RLN_GAS_LIMIT:-2000000}"
}
