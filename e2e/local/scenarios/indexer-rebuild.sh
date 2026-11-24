#!/usr/bin/env bash
# e2e/local/scenarios/indexer-rebuild.sh: an indexer drops its tree and reads it again from
# LEZ (REBUILD_TREE=true), keeping its Zone channel state; it ends where it was.
#
#   bash e2e/local/scenarios/indexer-rebuild.sh e2e/local/local.env
#
# The Rebuild response of the RFC, on the reader that can replay. Before: the indexer agrees
# with the sequencer. It restarts with REBUILD_TREE=true (the log says the tree store is
# dropped), judges every batch again against the tree it derives from the LEZ blocks on
# Bedrock, and must come back to what the sequencer says now (the chain keeps moving: LEZ
# blocks every 2 s, a heartbeat batch every HEARTBEAT_SEC), still carrying every root it had,
# with no stall.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENV_FILE="${1:?usage: indexer-rebuild.sh <local.env>}"
# shellcheck source=/dev/null
. "$ENV_FILE"
# shellcheck source=/dev/null
. "$ROOT/lib.sh"
# shellcheck source=/dev/null
. "$ROOT/local/ctl.sh"
[ "$NETWORK" = local ] || die "the local stack only"

zone_wait_http sequencer 30
zone_wait_http indexer 30
[ "$(zone_field sequencer .batch_count)" -ge 1 ] || die "the sequencer published no batch yet: run register.sh first"
zone_wait_agree 300
summary() { zone_at "$1" / | jq -c '{batch_count, root: .newest_published.root, leaves: .tree.active_leaf_count, removed: .tree.removed_leaf_count}'; }
before=$(summary indexer)
sig_before=$(roots_signature indexer)
say "indexer before: $before"

zone_stop indexer
tree_dir="$(zone_role_data indexer)/tree"
[ -d "$tree_dir" ] || die "no tree store at $tree_dir to rebuild"
mark=$(log_mark "$(zone_role_log indexer)")
zone_start indexer REBUILD_TREE=true
zone_wait_http indexer 120
say "indexer: $(zone_wait_log indexer "$mark" 'tree store dropped' 60)"
zone_wait_log indexer "$mark" 'ready' 300 > /dev/null

# A rebuilt tree judges the parked batches again: a wrong batch a correction already answered
# stalls the indexer once more for a few seconds, until the correction is judged again too.
stalled=""
for i in $(seq 1 300); do
    after=$(summary indexer)
    stall=$(zone_field indexer .stall)
    if [ "$stall" != "null" ]; then
        [ -n "$stalled" ] || say "  stalled again at $i s: $(jq -c '{msg: .msg[0:8], error}' <<< "$stall")"
        stalled=$i
    elif [ "$after" = "$(summary sequencer)" ]; then
        break
    fi
    [ $((i % 30)) -eq 0 ] && say "  rebuilding: $after"
    sleep 1
done
[ "$(zone_field indexer .stall)" = "null" ] || die "the rebuilt indexer is still stalled after 300 s: $(zone_field indexer .stall)"
[ "$after" = "$(summary sequencer)" ] || die "the rebuilt indexer says $after, the sequencer $(summary sequencer)"
[ -z "$stalled" ] || say "  the stall passed after $i s"
sig_after=$(roots_signature indexer)
while IFS= read -r line; do
    [ -z "$line" ] && continue
    printf '%s\n' "$sig_after" | grep -q -F -x -- "$line" || die "root entry '$line' is gone after the rebuild"
done <<< "$sig_before"
say "indexer after rebuild: $after (before $before), every root it had is still there"
zone_wait_agree 300
echo "INDEXER_REBUILD $after"
