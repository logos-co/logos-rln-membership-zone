#!/usr/bin/env bash
# e2e/provision-testnet.sh: give the testnet target a funded LEZ payer.
#
#   bash e2e/provision-testnet.sh [all|payer|fund|status]
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
. "$ROOT/lib.sh"

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
    if [ -n "$(state_get payer)" ] && [ -f "$PAYER_WALLET/storage.json" ]; then
        say "payer $(state_get payer) (wallet $PAYER_WALLET)"
        return 0
    fi
    local mint_payer payer
    mint_payer=$(lez_rln_bin mint_payer)
    mkdir -p "$PAYER_WALLET"
    jq -n --arg s "$LEZ_SEQUENCER_URL/" '{sequencers:[{sequencer_addr:$s}], seq_poll_timeout:"30s",
        seq_tx_poll_max_blocks:15, seq_poll_max_retries:10, seq_block_poll_max_amount:100,
        multi_sequencer_client_config:{distribution_limit:1, calibration_limit:3}}' \
        > "$PAYER_WALLET/wallet_config.json"
    payer=$(HOME="$PAYER_WALLET" LEE_WALLET_HOME_DIR="$PAYER_WALLET" "$mint_payer") || die "mint_payer failed"
    chmod 600 "$PAYER_WALLET/storage.json"
    state_set payer "$payer"
    say "minted payer $payer (wallet $PAYER_WALLET; back it up, it will hold real funds)"
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

    if [ -z "$(state_get pending_deposit)" ]; then
        local total
        total=$(bedrock_wallet "$key" | jq -r '.balance // 0')
        ge "$total" "$((DEPOSIT_AMOUNT + 1000000))" \
            || die "the node wallet holds $total, below the deposit plus fee ($DEPOSIT_AMOUNT + 1e6)"

        # A deposit spends whole notes: make one of exactly DEPOSIT_AMOUNT.
        if ! bedrock_wallet "$key" | jq -e --argjson a "$DEPOSIT_AMOUNT" \
            '.notes | to_entries | any(.value == $a)' >/dev/null; then
            say "splitting a $DEPOSIT_AMOUNT note"
            bedrock -X POST -H 'content-type: application/json' \
                "http://$BEDROCK_HTTP/wallet/transactions/transfer-funds" \
                -d "{\"tip\":null,\"change_public_key\":\"$key\",\"funding_public_keys\":[\"$key\"],\"recipient_public_key\":\"$key\",\"amount\":$DEPOSIT_AMOUNT}" \
                >/dev/null || die "transfer-funds failed"
            wait_note "$DEPOSIT_AMOUNT" || die "the $DEPOSIT_AMOUNT note never appeared"
        fi

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
    say "zone:    $(zone / | jq -c '{lez_block_id, covered_to_lez_block_id, published_batches, newest_adopted}' 2>/dev/null || echo 'not running')"
}

case "$STEP" in
    all) step_payer; step_fund; step_status ;;
    payer) step_payer ;;
    fund) step_fund ;;
    status) step_status ;;
    *) die "unknown step $STEP (all|payer|fund|status)" ;;
esac
