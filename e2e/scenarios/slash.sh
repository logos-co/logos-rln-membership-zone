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
root=$(printf '%s' "$membership" | jq -r '.head.root')
membership_mark_slashed "$leaf_index"
say "leaf $leaf_index slashed, root $root"

# A removed leaf leaves its slot empty, so the root may be one the zone published before; the
# proof is a NEW root entry, produced after the slash by a batch whose changes remove the leaf.
say "waiting for the batch removing leaf $leaf_index (root $root) to be adopted by a Bedrock block"
status=$(root_wait_adopted \
    '.root == $r and .batches[0].published_at_unix >= $t
        and any(.changes[]; .kind == "remove" and .leaf_index == $leaf)' \
    --arg r "$root" --argjson t "$slashed_at" --argjson leaf "$leaf_index") \
    || die "no adopted batch removing leaf $leaf_index after the slash within 30 minutes"
say "slash done: leaf $leaf_index is removed in a root Bedrock holds"
echo "ID_COMMITMENT=$id_commitment LEAF_INDEX=$leaf_index ROOT=$root STATUS=$status"
