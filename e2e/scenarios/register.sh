#!/usr/bin/env bash
# e2e/scenarios/register.sh: one registration, seen end to end.
#
#   bash e2e/scenarios/register.sh <network.env>
#
# Registers one membership from the payer wallet (lez-rln `register_member`), then watches
# the Zone sequencer: the membership appears at `/merkle_proof/<id_commitment>` with the
# leaf index the registration claimed and a proof path, and the root that includes it is
# published in a batch (`/roots`), pending first, adopted once Bedrock holds it.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${1:?usage: register.sh <network.env>}"
# shellcheck source=/dev/null
. "$ENV_FILE"
# shellcheck source=/dev/null
. "$ROOT/lib.sh"

register_member=$(lez_rln_bin register_member)
lez_rln_env
[ -n "$LEZ_RLN_PAYER" ] || die "no payer: run provision-$NETWORK.sh first"
ge "$(lez_balance "$LEZ_RLN_PAYER")" "$PAYER_MIN" || die "payer $LEZ_RLN_PAYER holds less than $PAYER_MIN"

before=$(lez_balance "$LEZ_RLN_PAYER")
say "registering from $LEZ_RLN_PAYER (balance $before)"
out=$("$register_member") || die "register_member failed"
after=$(lez_balance "$LEZ_RLN_PAYER")
say "payer balance $before -> $after: the registration cost $((before - after)) atomic (price + fee)"
printf '%s\n' "$out" >&2
id_commitment=$(printf '%s\n' "$out" | sed -n 's/^ID_COMMITMENT=//p')
leaf_index=$(printf '%s\n' "$out" | sed -n 's/^LEAF_INDEX=//p')
[ -n "$id_commitment" ] && [ -n "$leaf_index" ] || die "register_member printed no ID_COMMITMENT / LEAF_INDEX"

say "waiting for the zone to apply leaf $leaf_index"
for _ in $(seq 1 60); do
    if membership=$(zone "/merkle_proof/$id_commitment" 2>/dev/null) \
        && [ "$(printf '%s' "$membership" | jq -r '.active')" = "true" ]; then
        break
    fi
    membership=""
    sleep 10
done
[ -n "$membership" ] || die "the zone did not apply the registration within 10 minutes"
[ "$(printf '%s' "$membership" | jq -r '.leaf_index')" = "$leaf_index" ] \
    || die "leaf index mismatch: claimed $leaf_index, zone $(printf '%s' "$membership" | jq -r '.leaf_index')"
[ "$(printf '%s' "$membership" | jq -r '.proof.path_elements | length')" = "20" ] \
    || die "the proof path does not have 20 elements"
root=$(printf '%s' "$membership" | jq -r '.root')
say "leaf $leaf_index active, root $root"

say "waiting for a batch carrying that root"
for _ in $(seq 1 90); do
    status=$(zone /roots/5 | jq -r --arg r "$root" '.roots[] | select(.root == $r) | .status' | head -1)
    [ -n "$status" ] && break
    sleep 10
done
[ -n "$status" ] || die "no batch with root $root within 15 minutes"
say "batch with root $root is $status"
echo "ID_COMMITMENT=$id_commitment LEAF_INDEX=$leaf_index ROOT=$root STATUS=$status"
