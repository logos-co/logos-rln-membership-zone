#!/usr/bin/env bash
# e2e/testnet/provision.sh: give the testnet target a funded LEZ payer.
#
#   bash e2e/testnet/provision.sh [all|payer|fund|status]
#
# The testnet has no LEZ faucet. Native balance reaches a LEZ account one way only: a note
# the Bedrock node's wallet holds (mined PoW rewards on the PoWClaim key) is deposited into
# the LEZ channel with the payer's id as metadata, and LEZ credits it once the deposit is
# final on Bedrock (8 to 10 hours on the testnet). Every step checks before it acts, so a
# re-run resumes: the payer is minted once, a pending deposit is waited for, not repeated.
#
# Steps:
#   payer   mint the LEZ payer (a wallet under E2E_STATE); needs no chain
#   fund    split a DEPOSIT_AMOUNT note on the node, deposit it, wait for the credit
#   status  balances on both sides and the pending deposit

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
. "$ROOT/testnet.env"
# shellcheck source=/dev/null
. "$ROOT/../lib.sh"

STEP="${1:-all}"

# The node wallet key that holds the mined rewards, from the node keystore.
funding_key() {
    local key
    key=$(state_get funding_key)
    [ -n "$key" ] && { echo "$key"; return 0; }
    command -v yq >/dev/null || die "yq is needed to read ~/keystore.yaml (or set funding_key in $STATE_JSON)"
    key=$(yq .public_keys.PoWClaim ~/keystore.yaml) || die "cannot read PoWClaim from ~/keystore.yaml"
    state_set funding_key "$key"
    echo "$key"
}

step_payer() {
    payer_mint
}

# Waits until the node wallet shows a note of exactly $1.
wait_note() {
    local amount="$1" key
    key=$(funding_key)
    for _ in $(seq 1 60); do
        bedrock_wallet "$key" | jq -e --argjson a "$amount" '.notes | to_entries | any(.value == $a)' \
            >/dev/null 2>&1 && return 0
        sleep 10
    done
    return 1
}

# Has the node wallet make a note of exactly $1 for itself: a transfer to its own key, the
# wallet picks the largest notes first. A transaction spends at most 32 notes, so when the 30
# largest do not reach the amount plus fee the wallet first merges them into one note, as
# often as it takes (PoW rewards come as small notes).
# Whether the node knows a transaction: mined (`/cryptarchia/transaction/:hash`) or waiting in
# its mempool.
deposit_known() {
    local hash="$1"
    bedrock "http://$BEDROCK_HTTP/cryptarchia/transaction/$hash" | jq -e '.code != 404' >/dev/null 2>&1 \
        && return 0
    bedrock "http://$BEDROCK_HTTP/mempool/view" | jq -e --arg h "$hash" 'index($h) != null' >/dev/null 2>&1
}

make_note() {
    local amount="$1" key largest target
    key=$(funding_key)
    while ! bedrock_wallet "$key" | jq -e --argjson a "$amount" \
        '.notes | to_entries | any(.value == $a)' >/dev/null; do
        largest=$(bedrock_wallet "$key" | jq '[.notes[]] | sort | reverse | .[:30] | add // 0')
        if ge "$largest" "$((amount + SPLIT_FEE))"; then
            target=$amount
        else
            target=$((largest - SPLIT_FEE))
            ge "$target" 1000000 || die "the 30 largest notes hold $largest: too little to merge"
            say "merging the largest notes into one of $target first (at most 32 per transaction)"
        fi
        say "making a $target note"
        transfer_to_self "$key" "$target"
        wait_note "$target" || die "the $target note never appeared"
    done
}

# A transfer of $2 from key $1 to itself; the node answers {code, message} when it refuses.
transfer_to_self() {
    local key="$1" amount="$2" resp
    resp=$(bedrock -X POST -H 'content-type: application/json' \
        "http://$BEDROCK_HTTP/wallet/transactions/transfer-funds" \
        -d "{\"tip\":null,\"change_public_key\":\"$key\",\"funding_public_keys\":[\"$key\"],\"recipient_public_key\":\"$key\",\"amount\":$amount}") \
        || die "transfer-funds failed"
    if printf '%s' "$resp" | jq -e 'type == "object" and has("code")' >/dev/null 2>&1; then
        die "transfer-funds refused: $(printf '%s' "$resp" | jq -r '.message // .')"
    fi
}

# Fee room left when a note is made from many inputs (every input adds to the fee).
SPLIT_FEE=2000000

step_fund() {
    local payer key bal state
    payer=$(state_get payer)
    [ -n "$payer" ] || die "no payer yet: run the payer step"
    key=$(funding_key)

    state=$(bedrock_info | jq -r .state)
    [ "$state" = "Online" ] || die "the node is $state, not Online: wait for it to sync"

    bal=$(lez_balance "$payer")
    [ -n "$bal" ] || die "cannot read the payer's balance from $LEZ_SEQUENCER_URL"
    if ge "$bal" "$PAYER_MIN"; then
        say "payer holds $bal (>= $PAYER_MIN): funded"
        return 0
    fi

    # A pending deposit the node does not know: `/cryptarchia/transaction/:id` answers 404 for
    # a mined tx too once it left the node's index, so this alone does not mean dropped. The
    # note it spent tells: gone from the wallet, it was mined (keep waiting); still there, the
    # node dropped the tx. Depositing again is the operator's call (DEPOSIT_RESEND=1).
    local pending
    pending=$(state_get pending_deposit)
    if [ -n "$pending" ] && ! deposit_known "$pending"; then
        if [ "${DEPOSIT_RESEND:-0}" = "1" ]; then
            say "deposit $pending forgotten on request (DEPOSIT_RESEND=1): depositing again"
            state_del pending_deposit
            state_del pending_from_balance
        else
            say "deposit $pending: not in the node's tx index nor in its mempool. If the wallet still" \
                "holds a note of $DEPOSIT_AMOUNT the node dropped it: re-run with DEPOSIT_RESEND=1." \
                "If that note is gone it was mined: keep waiting for the credit."
        fi
    fi

    if [ -z "$(state_get pending_deposit)" ]; then
        local total
        total=$(bedrock_wallet "$key" | jq -r '.balance // 0')
        ge "$total" "$((DEPOSIT_AMOUNT + 1000000))" \
            || die "the node wallet holds $total, below the deposit plus fee ($DEPOSIT_AMOUNT + 1e6)"

        # A deposit spends whole notes: make one of exactly DEPOSIT_AMOUNT.
        make_note "$DEPOSIT_AMOUNT"

        local note body resp
        note=$(bedrock_wallet "$key" | jq -r --argjson a "$DEPOSIT_AMOUNT" \
            '[.notes | to_entries[] | select(.value == $a) | .key][0]')
        body=$(jq -cn --arg ch "$LEZ_CHANNEL_ID" --arg note "$note" --arg key "$key" \
            --argjson meta "$(account_bytes_json "$payer")" \
            '{tip:null, deposit:{channel_id:$ch, inputs:[$note], metadata:$meta},
              change_public_key:$key, funding_public_keys:[$key], max_tx_fee:100000}')
        resp=$(bedrock -X POST -H 'content-type: application/json' "http://$BEDROCK_HTTP/channel/deposit" \
            -d "$body") || die "channel deposit failed"
        state_set pending_deposit "$(printf '%s' "$resp" | jq -r '.hash')"
        state_set pending_from_balance "$bal"
        say "deposited $DEPOSIT_AMOUNT for $payer (bedrock tx $(state_get pending_deposit)); credited once final"
    else
        say "deposit $(state_get pending_deposit) already pending"
    fi

    local from budget
    from=$(state_get pending_from_balance)
    budget="${DEPOSIT_WAIT_S:-43200}"
    say "waiting up to ${budget}s for the credit (Ctrl-C and re-run to keep waiting)"
    for _ in $(seq 1 $((budget / 60))); do
        bal=$(lez_balance "$payer")
        if [ -n "$bal" ] && ge "$bal" "$(python3 -c "print(${from:-0} + $DEPOSIT_AMOUNT // 2)")"; then
            state_del pending_deposit
            state_del pending_from_balance
            say "credited: payer holds $bal"
            return 0
        fi
        sleep 60
    done
    die "deposit not credited after ${budget}s (payer holds ${bal:-?}); re-run to keep waiting"
}

step_status() {
    local payer key
    payer=$(state_get payer)
    key=$(funding_key)
    say "node:    $(bedrock_info | jq -c '{slot, height, lib_slot, state}')"
    say "L1 $key: $(bedrock_wallet "$key" | jq -c '{balance, notes: (.notes | length)}')"
    [ -n "$payer" ] && say "payer $payer: L2 balance $(lez_balance "$payer")"
    [ -n "$(state_get pending_deposit)" ] && say "pending deposit $(state_get pending_deposit)"
    say "zone:    $(zone / | jq -c '{head, newest_published, newest_finalized}' 2>/dev/null || echo 'not running')"
}

case "$STEP" in
    all) step_payer; step_fund; step_status ;;
    payer) step_payer ;;
    fund) step_fund ;;
    status) step_status ;;
    *) die "unknown step $STEP (all|payer|fund|status)" ;;
esac
