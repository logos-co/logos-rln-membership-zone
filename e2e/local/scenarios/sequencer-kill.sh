#!/usr/bin/env bash
# e2e/local/scenarios/sequencer-kill.sh: the Zone sequencer dies without warning (SIGKILL)
# right after a registration is submitted; the restart loses nothing and publishes once.
#
#   bash e2e/local/scenarios/sequencer-kill.sh e2e/local/local.env
#
# Whatever the process had seen or published before the kill, the restart must recover its
# state files, reconcile with the channel (a publish recorded in flight that never chained is
# dropped, said in the log), carry the registration in exactly one applied batch, never hit
# the fatal "would not apply", and agree with the indexer.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENV_FILE="${1:?usage: sequencer-kill.sh <local.env>}"
# shellcheck source=/dev/null
. "$ENV_FILE"
# shellcheck source=/dev/null
. "$ROOT/lib.sh"
# shellcheck source=/dev/null
. "$ROOT/local/ctl.sh"
[ "$NETWORK" = local ] || die "the local stack only"

lez_rln_env
zone_wait_http sequencer 30
zone_wait_http indexer 30
zone_wait_agree 300

read -r leaf_index id_commitment <<< "$(register_submit)"
zone_stop sequencer -KILL

mark=$(log_mark "$(zone_role_log sequencer)")
zone_start sequencer
zone_wait_http sequencer 120
say "reconciled: $(zone_wait_log sequencer "$mark" 'chain: channel tip|chain: channel not on chain yet' 180)"
if zone_log_has sequencer "$mark" 'recorded in flight never chained'; then
    say "a publish recorded in flight before the kill never chained: dropped on restart"
fi

membership_wait_batched "$id_commitment" 600 > /dev/null
inserts=$(zone_at sequencer /roots/50 | jq --argjson leaf "$leaf_index" \
    '[.roots[].batches[] | select(.superseded == false and .error == null) | .changes[]
      | select(.leaf_index == $leaf and .kind == "insert")] | length')
[ "$inserts" = 1 ] || die "leaf $leaf_index is inserted by $inserts applied batches, not one"
if zone_log_has sequencer "$mark" 'would not apply|panicked'; then
    die "the restarted sequencer hit a fatal path: $(log_since "$(zone_role_log sequencer)" "$mark" | grep -E -m1 'would not apply|panicked')"
fi
zone_wait_agree 300
echo "SEQUENCER_KILL leaf=$leaf_index inserted_once=true"
