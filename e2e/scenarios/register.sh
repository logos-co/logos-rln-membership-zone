#!/usr/bin/env bash
# e2e/scenarios/register.sh: one registration, seen end to end.
#
#   bash e2e/scenarios/register.sh <network.env>
#
# Registers one membership from the payer wallet (lez-rln `register_member`), then watches
# the Zone sequencer: the membership appears at `/merkle_proof/<id_commitment>` with the
# leaf index the registration claimed, active once its LEZ block is adopted on Bedrock, with
# a proof path once a batch carries its root (the path is made at that root, so proof.sh can
# verify it against `/roots` at once; `bedrock_status` `finalized` hours later). The
# membership (leaf, id_commitment, identity secret) goes on record for slash.sh and proof.sh.

set -euo pipefail
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
identity_secret=$(printf '%s\n' "$out" | sed -n 's/^IDENTITY_SECRET_HASH=//p')
[ -n "$id_commitment" ] && [ -n "$leaf_index" ] || die "register_member printed no ID_COMMITMENT / LEAF_INDEX"
# On record for slash.sh and proof.sh.
membership_set "$leaf_index" "$id_commitment" "$identity_secret"

say "waiting for the zone to apply leaf $leaf_index and a batch to carry it: the LEZ block must be adopted on Bedrock first"
for i in $(seq 1 120); do
    answer=$(zone "/merkle_proof/$id_commitment" 2>/dev/null) || answer=""
    if [ "$(printf '%s' "$answer" | jq -r '.active and .proof != null' 2>/dev/null)" = "true" ]; then
        membership="$answer"
        break
    fi
    membership=""
    [ $((i % 6)) -eq 0 ] && say "  $((i / 6)) min: $(membership_status "$answer")"
    sleep 10
done
[ -n "$membership" ] || die "the zone did not apply and batch the registration within 20 minutes: $(membership_status "$answer")"
[ "$(printf '%s' "$membership" | jq -r '.leaf_index')" = "$leaf_index" ] \
    || die "leaf index mismatch: claimed $leaf_index, zone $(printf '%s' "$membership" | jq -r '.leaf_index')"
[ "$(printf '%s' "$membership" | jq -r '.proof.path_elements | length')" = "20" ] \
    || die "the proof path does not have 20 elements"
root=$(printf '%s' "$membership" | jq -r '.proof.root')
say "leaf $leaf_index active at LEZ block $(printf '%s' "$membership" | jq -r '.head.lez_block_id'), in a batch to LEZ block $(printf '%s' "$membership" | jq -r '.proof.to_lez_block_id'), root $root"

say "the batch carrying root $root on Bedrock"
published=$(root_wait_published '.root == $r' --arg r "$root") \
    || die "the zone's /roots does not list root $root within 30 minutes"
say "registration done: leaf $leaf_index is in a root a batch carries, proof.sh can verify against it"
echo "ID_COMMITMENT=$id_commitment LEAF_INDEX=$leaf_index ROOT=$root BATCH=$published"
