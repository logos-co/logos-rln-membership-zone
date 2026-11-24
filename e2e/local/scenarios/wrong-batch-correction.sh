#!/usr/bin/env bash
# e2e/local/scenarios/wrong-batch-correction.sh: a wrong batch lands on the channel under the
# sequencer's key; the sequencer corrects it, the indexer parks it and applies the correction.
#
#   bash e2e/local/scenarios/wrong-batch-correction.sh e2e/local/local.env
#
# The Correction response of the RFC ("a key published a wrong batch"), provoked without any
# hook in the Zone binary: a second sequencer, the `rogue` role of ctl.sh, runs on a copy
# of the sequencer's data dir (same channel key, same Zone SDK checkpoint) with REBUILD_TREE=true
# and a TREE_ID nothing on LEZ carries, so it derives an empty tree, finds every batch on the
# channel contradicted and honestly publishes a "correction" from the first batch's block that
# states the empty root: from the chain's point of view, a wrong batch under the accredited
# key. It is stopped as soon as that batch is adopted. The real sequencer takes the entry into
# its view, finds the channel's root is not the tree's and publishes a correction from the same
# `from` (`correction batch [`), superseding the wrong batch and every root it re-covered; the
# indexer parks the wrong batch once Bedrock finalizes it (`parked`, `stall` set), then applies
# the correction (`stall` null, `superseded_count` up) and ends with the sequencer's roots; a
# registration after that is batched on both.
#
# If the real sequencer stops instead (its SDK chained the correction on a tip the channel
# moved past: `the SDK's tip is stale`, the checkpoint corrected), it is started again and the
# correction is expected from the restart; the log says which path was taken.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENV_FILE="${1:?usage: wrong-batch-correction.sh <local.env>}"
# shellcheck source=/dev/null
. "$ENV_FILE"
# shellcheck source=/dev/null
. "$ROOT/lib.sh"
# shellcheck source=/dev/null
. "$ROOT/local/ctl.sh"
[ "$NETWORK" = local ] || die "the local stack only"

# A TREE_ID no event on LEZ carries: the rogue's rebuilt tree stays empty.
ROGUE_TREE_ID="ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
range_of() { sed -E 's/.*correction batch \[([0-9]+)\.\.([0-9]+)\].*/\1 \2/' <<< "$1"; }

lez_rln_env
zone_wait_http sequencer 30
zone_wait_http indexer 30
[ "$(zone_field sequencer .batch_count)" -ge 1 ] || die "the sequencer published no batch yet: run register.sh first"
zone_wait_agree 300
for i in $(seq 1 120); do
    [ "$(zone_field sequencer .in_flight)" = "null" ] && break
    sleep 1
done
[ "$(zone_field sequencer .in_flight)" = "null" ] || die "the sequencer still has a batch in flight"
superseded_before=$(zone_field indexer .superseded_count)
seq_superseded_before=$(zone_field sequencer .superseded_count)
batches_before=$(zone_field sequencer .batch_count)
say "before: $batches_before batches, superseded $seq_superseded_before/$superseded_before (sequencer/indexer)"

# The rogue's data dir: a copy of the sequencer's, taken while the sequencer is stopped.
rogue_dir=$(zone_role_data rogue)
zone_stop sequencer
rm -rf "$rogue_dir"
cp -R "$(zone_role_data sequencer)" "$rogue_dir"
zone_start sequencer
zone_wait_http sequencer 120
say "rogue: data dir copied from the sequencer ($rogue_dir)"

trap 'zone_stop rogue -KILL > /dev/null 2>&1 || true' EXIT
mark_seq=$(log_mark "$(zone_role_log sequencer)")
mark_idx=$(log_mark "$(zone_role_log indexer)")
mark_rogue=$(log_mark "$(zone_role_log rogue)")
zone_start rogue REBUILD_TREE=true TREE_ID="$ROGUE_TREE_ID" POLL_SEC=2
zone_wait_http rogue 120
zone_wait_log rogue "$mark_rogue" 'tree store dropped' 60 > /dev/null
rogue_line=$(zone_wait_log rogue "$mark_rogue" 'correction batch \[' 300)
read -r rogue_from rogue_to <<< "$(range_of "$rogue_line")"
say "rogue: publishes the wrong batch [$rogue_from..$rogue_to] (root of an empty tree)"
say "sequencer: $(zone_wait_log sequencer "$mark_seq" 'channel entry msg .* adopted on Bedrock' 300)"
zone_stop rogue -KILL

# The real sequencer's correction, from the same block; a restart if its publish was stale.
restarted=0
for i in $(seq 1 300); do
    seq_line=$(log_since "$(zone_role_log sequencer)" "$mark_seq" | grep -E -m1 'correction batch \[' || true)
    [ -n "$seq_line" ] && break
    if [ -z "$(zone_role_pid sequencer)" ]; then
        say "sequencer: exited: $(log_since "$(zone_role_log sequencer)" "$mark_seq" | grep -E -m1 'stale|Error|error' || echo 'no reason logged')"
        restarted=$((restarted + 1))
        [ "$restarted" -le 1 ] || die "the sequencer exited twice after the wrong batch"
        zone_start sequencer
        zone_wait_http sequencer 120
    fi
    sleep 1
done
[ -n "$seq_line" ] || die "the sequencer published no correction within 300 s"
read -r seq_from seq_to <<< "$(range_of "$seq_line")"
say "sequencer: $seq_line"
[ "$seq_from" = "$rogue_from" ] || die "the correction starts at LEZ block $seq_from, the wrong batch at $rogue_from"
if [ "$restarted" -eq 0 ]; then
    say "sequencer: corrected without a restart"
else
    say "sequencer: corrected after $restarted restart"
fi

say "indexer: $(zone_wait_log indexer "$mark_idx" 'parked' 600)"
for i in $(seq 1 60); do
    [ "$(zone_field indexer .stall)" = "null" ] && [ "$(zone_field indexer .superseded_count)" -gt "$superseded_before" ] && break
    sleep 5
done
[ "$(zone_field indexer .stall)" = "null" ] || die "the indexer stays stalled after the correction: $(zone_field indexer .stall)"
[ "$(zone_field indexer .superseded_count)" -gt "$superseded_before" ] || die "the indexer superseded nothing"
# The wrong batch: parked on the indexer (its error stands), superseded on the sequencer that
# applied it unjudged, so the sequencer counts one more; neither lists its root.
seq_delta=$(( $(zone_field sequencer .superseded_count) - seq_superseded_before ))
idx_delta=$(( $(zone_field indexer .superseded_count) - superseded_before ))
[ "$seq_delta" -eq $(( idx_delta + 1 )) ] \
    || die "superseded since the start: sequencer +$seq_delta, indexer +$idx_delta; expected one more on the sequencer"
! zone_log_has sequencer "$mark_seq" 'would not apply|panicked' || die "the sequencer logged a fatal line: see $(zone_role_log sequencer)"
zone_wait_agree 300

read -r leaf_index id_commitment <<< "$(register_submit)"
membership_wait_batched "$id_commitment" 600 > /dev/null
ZONE_HTTP="$ZONE_INDEXER_HTTP" membership_wait_batched "$id_commitment" 600 > /dev/null
zone_wait_agree 300
[ -z "$(zone_role_pid rogue)" ] || die "the rogue is still running"
echo "WRONG_BATCH_CORRECTION wrong=[$rogue_from..$rogue_to] correction=[$seq_from..$seq_to] restarts=$restarted superseded=$(zone_field indexer .superseded_count) leaf=$leaf_index"
