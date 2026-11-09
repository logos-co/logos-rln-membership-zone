#!/usr/bin/env bash
# e2e/scenarios/slash.sh: slash the membership register.sh made, seen end to end.
#
#   bash e2e/scenarios/slash.sh <network.env> [leaf_index]
#
# Reveals the identity secret of a registration register.sh made (the leaf given, else the
# newest not slashed yet; lez-rln `slash_member`, paid by the payer), then watches the Zone
# sequencer: the membership turns `active: false`, `removed: slashed`, its proof goes away,
# and a batch with the new root is published (`Remove leaf N`).

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="${1:?usage: slash.sh <network.env> [leaf_index]}"
# shellcheck source=/dev/null
. "$ENV_FILE"
# shellcheck source=/dev/null
. "$ROOT/lib.sh"

slash_member=$(lez_rln_bin slash_member)
lez_rln_env
read -r leaf_index id_commitment identity_secret <<< "$(membership_pick "${2:-}")"
[ -n "${identity_secret:-}" ] || die "no registration to slash on record${2:+ at leaf $2}: run register.sh first"
membership=$(zone "/merkle_proof/$id_commitment") || die "the zone does not know $id_commitment"
[ "$(printf '%s' "$membership" | jq -r '.active')" = "true" ] || die "leaf $leaf_index is not active, nothing to slash"

before=$(lez_balance "$LEZ_RLN_PAYER")
slashed_at=$(date +%s)
say "slashing leaf $leaf_index from $LEZ_RLN_PAYER (balance $before)"
LEZ_RLN_ID_COMMITMENT="$id_commitment" LEZ_RLN_IDENTITY_SECRET="$identity_secret" "$slash_member" >&2 \
    || die "slash_member failed"
after=$(lez_balance "$LEZ_RLN_PAYER")
say "payer balance $before -> $after: the slash cost $((before - after)) atomic (fee)"

say "waiting for the zone to remove leaf $leaf_index"
for _ in $(seq 1 60); do
    membership=$(zone "/merkle_proof/$id_commitment") || die "the zone lost $id_commitment"
    [ "$(printf '%s' "$membership" | jq -r '.active')" = "false" ] && break
    membership=""
    sleep 10
done
[ -n "$membership" ] || die "the zone did not apply the slash within 10 minutes"
[ "$(printf '%s' "$membership" | jq -r '.removed')" = "slashed" ] \
    || die "removed is $(printf '%s' "$membership" | jq -r '.removed'), not slashed"
[ "$(printf '%s' "$membership" | jq -r '.proof')" = "null" ] || die "a removed membership still has a proof"
root=$(printf '%s' "$membership" | jq -r '.root')
membership_mark_slashed "$leaf_index"
say "leaf $leaf_index slashed, root $root"

# A removed leaf leaves its slot empty, so the root may be one the zone published before;
# the proof is a NEW batch, published after the slash, carrying the change and that root.
say "waiting for a batch published after the slash with that root"
for _ in $(seq 1 90); do
    status=$(zone /batches/5 | jq -r --arg r "$root" --argjson t "$slashed_at" \
        '.batches[] | select(.root == $r and .published_at_unix >= $t and .change_count > 0) | .status' | head -1)
    [ -n "$status" ] && break
    sleep 10
done
[ -n "$status" ] || die "no batch with root $root published after the slash within 15 minutes"
say "batch with root $root is $status"
echo "ID_COMMITMENT=$id_commitment LEAF_INDEX=$leaf_index ROOT=$root STATUS=$status"
