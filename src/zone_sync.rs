//! The Zone channel as the node follows it through the Zone SDK: the checkpoint and the
//! start-up checks, the SDK's `BlocksProcessed` events folded into `ZoneChain` (extension,
//! conflict, finality), the finality scan that reads what the stream missed, and the
//! sequencer's tick (one batch in flight, heartbeat, publish and republish).

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail};
use lb_core::mantle::ops::channel::{ChannelId, MsgId, inscribe::Inscription};
use lb_zone_sdk::{
    Slot,
    adapter::{Node as _, NodeHttpClient},
    sequencer::{
        ChannelUpdate, Error as SeqError, FinalizedOp, FinalizedTx, InscriptionInfo,
        SequencerCheckpoint, ZoneSequencer, channel_inscriptions,
    },
};
use prost::Message as _;
use reqwest::Url;
use tracing::{info, warn};

use crate::{
    batch::Batch,
    http::Shared,
    registry::{BedrockStatus, Registry},
    zone_chain::{AcceptOutcome, ChannelEntry, PublishRecord},
};

/// Seconds between two finality scans (`reconcile_finality`).
pub const FINALITY_SCAN_SEC: u64 = 300;
/// L1 slots per `immutable_blocks` page of the finality scan.
pub const FINALITY_SCAN_PAGE: u64 = 100;

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn short(msg: &[u8; 32]) -> String {
    hex::encode(&msg[..4])
}

/// A channel entry from an inscription the SDK reports.
pub fn entry_of(info: &InscriptionInfo) -> ChannelEntry {
    ChannelEntry::new(info.this_msg, info.parent_msg, &info.payload)
}

/// The Zone SDK checkpoint on the Zone channel, kept with the tree store: set here, written by
/// the caller's `save()` together with the state the checkpoint stands for, so a restart never
/// finds a checkpoint ahead of the state (the SDK does not report again what the checkpoint
/// already passed).
pub fn set_checkpoint(registry: &Shared, checkpoint: &SequencerCheckpoint) -> Result<()> {
    registry
        .write()
        .expect("registry lock")
        .set_checkpoint(serde_json::to_value(checkpoint)?);
    Ok(())
}

/// The Zone channel and the node to ask about it.
pub struct ZoneChannel {
    pub node_url: Url,
    pub id: ChannelId,
}

/// What the node holds for a channel (`GET /channel/:id`), the fields this check reads.
#[derive(serde::Deserialize)]
pub struct ChannelOnChain {
    accredited_keys: Vec<String>,
    tip_message: String,
}

/// The channel as the node knows it (`GET /channel/:id`): `None` while no message of it is on
/// chain (the first inscription creates it).
pub async fn channel_on_chain(
    node_url: &Url,
    channel_id: ChannelId,
) -> Result<Option<ChannelOnChain>> {
    let url = node_url.join(&format!("channel/{}", hex::encode(channel_id.as_ref())))?;
    let response = reqwest::get(url.clone())
        .await
        .with_context(|| format!("GET {url}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    response
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .json()
        .await
        .map(Some)
        .with_context(|| format!("GET {url}: unexpected body"))
}

/// The Zone SDK checkpoint for a channel with no message on chain yet: nothing to backfill, so
/// the SDK starts at the node's LIB instead of walking finalized history from genesis (30 to
/// 60 minutes on the testnet, hours when the node is slow). Every field is what a cold start
/// computes for an empty channel: root tip, no pending transaction, no channel note, root
/// config lineage.
pub async fn checkpoint_at_lib(node: &NodeHttpClient) -> Result<SequencerCheckpoint> {
    let info = node
        .consensus_info()
        .await
        .map_err(|e| anyhow!("consensus info: {e}"))?
        .cryptarchia_info;
    Ok(SequencerCheckpoint {
        last_msg_id: MsgId::root(),
        pending_txs: vec![],
        lib: info.lib,
        lib_slot: info.lib_slot,
        channel_notes: vec![],
        finalized_config: MsgId::root(),
        funding: vec![],
    })
}

/// Refuse to start a sequencer on a channel this data dir cannot continue. The Zone SDK learns
/// a channel's tip from finalized history only, so without a checkpoint it chains the next
/// batch on the root; on a channel that already has messages that batch is invalid
/// (`InvalidParent`) and vanishes without a trace. Likewise another key cannot write at all.
/// The tip on chain must be one of our entries, with nothing after it but publishes of ours
/// still in flight (the checkpoint carries them; the SDK resubmits or orphans them).
pub fn check_channel_on_chain(
    channel: &ChannelOnChain,
    our_key: &[u8; 32],
    registry: &Shared,
    resumed: bool,
) -> Result<()> {
    let ours = hex::encode(our_key);
    if !channel
        .accredited_keys
        .iter()
        .any(|k| k.eq_ignore_ascii_case(&ours))
    {
        bail!(
            "channel is on chain with accredited keys {:?}, not ours ({ours}): this key cannot \
             write to it; restore the original key.hex or pick another CHANNEL_NAME",
            channel.accredited_keys
        );
    }
    let entries = registry.read().expect("registry lock").lineage();
    let tip = channel.tip_message.to_ascii_lowercase();
    let Some(at) = entries.iter().position(|(msg, _)| hex::encode(msg) == tip) else {
        match entries.last() {
            Some((newest, _)) => bail!(
                "channel tip on chain is {}.., our newest batch is {}..: the Zone channel state \
                 does not match the channel (data from another run was lost); restore it or pick \
                 another CHANNEL_NAME",
                &tip[..8],
                short(newest)
            ),
            None => bail!(
                "channel is on chain (tip {}..) but this data dir holds no batch: data from \
                 another run was lost; restore it or pick another CHANNEL_NAME",
                &tip[..8]
            ),
        }
    };
    if !resumed {
        bail!(
            "channel tip {}.. is our batch but the data dir holds no Zone SDK checkpoint: the SDK \
             would chain on the root and every batch would be invalid; restore the data dir or \
             pick another CHANNEL_NAME",
            &tip[..8]
        );
    }
    let after = &entries[at + 1..];
    if let Some((landed, status)) = after
        .iter()
        .find(|(_, status)| *status != BedrockStatus::Pending)
    {
        bail!(
            "channel tip on chain is {}.., but our batch {}.. after it is {status:?} here: the \
             Zone channel state does not match the channel (data from another run?); restore it \
             or pick another CHANNEL_NAME",
            &tip[..8],
            short(landed)
        );
    }
    info!(
        "chain: channel tip {}.. is our {} batch{}",
        &tip[..8],
        if after.is_empty() {
            "newest"
        } else {
            "last landed"
        },
        match after.len() {
            0 => String::new(),
            n => format!("; {n} batch(es) of ours still in flight"),
        }
    );
    Ok(())
}

/// Say what the final tier made of a finalized entry.
pub fn report_finalized(entry: &ChannelEntry, l1_slot: u64, outcome: Option<&AcceptOutcome>) {
    let msg: [u8; 32] = entry.msg.into();
    let range = entry.batch.as_ref().map_or("no batch".to_string(), |b| {
        format!("[{}..{}]", b.from_lez_block_id, b.to_lez_block_id)
    });
    match outcome {
        Some(AcceptOutcome::Applied(_)) => info!(
            "batch {range} msg {} finalized at L1 slot {l1_slot}: applied, root {}..",
            short(&msg),
            entry
                .batch
                .as_ref()
                .map(|b| hex::encode(&b.root_after[..4]))
                .unwrap_or_default()
        ),
        Some(AcceptOutcome::Parked(e)) => warn!(
            "batch {range} msg {} finalized at L1 slot {l1_slot}: parked, {e}; the final tier \
             is frozen here",
            short(&msg)
        ),
        Some(AcceptOutcome::RetryableFailure(e)) => info!(
            "batch {range} msg {} finalized at L1 slot {l1_slot}: {e}",
            short(&msg)
        ),
        Some(AcceptOutcome::AlreadyApplied) => {}
        None => info!(
            "entry msg {} finalized at L1 slot {l1_slot}: {range}",
            short(&msg)
        ),
    }
}

/// The finality the SDK stream may have missed (a restart between the event and the save):
/// read the finalized L1 blocks from the anchor up to the LIB and apply every inscription of
/// the channel found there. Idempotent with the stream path. Returns how many were new.
pub async fn reconcile_finality(
    node: &NodeHttpClient,
    channel_id: ChannelId,
    registry: &Shared,
) -> Result<usize> {
    let Some(anchor) = registry.read().expect("registry lock").anchor() else {
        return Ok(0);
    };
    let lib = u64::from(
        node.consensus_info()
            .await
            .map_err(|e| anyhow!("consensus info: {e}"))?
            .cryptarchia_info
            .lib_slot,
    );
    let mut applied = 0;
    let mut page_from = anchor.slot + 1;
    while page_from <= lib {
        let page_to = lib.min(page_from + FINALITY_SCAN_PAGE - 1);
        let blocks = node
            .immutable_blocks(Slot::from(page_from), Slot::from(page_to))
            .await
            .map_err(|e| anyhow!("immutable blocks {page_from}..{page_to}: {e}"))?;
        let mut registry = registry.write().expect("registry lock");
        for block in &blocks {
            let l1_slot = u64::from(block.header.slot);
            for tx in &block.transactions {
                for info in channel_inscriptions(tx, channel_id) {
                    let entry = entry_of(&info);
                    let outcome = registry.apply_zone_finalized(entry.clone(), l1_slot);
                    if !matches!(outcome, Some(AcceptOutcome::AlreadyApplied)) {
                        report_finalized(&entry, l1_slot, outcome.as_ref());
                        applied += 1;
                    }
                }
            }
        }
        registry.save()?;
        page_from = page_to + 1;
    }
    // Everything up to the LIB is read: the next scan starts after it.
    let mut registry = registry.write().expect("registry lock");
    registry.scanned_to(lib);
    registry.save()?;
    Ok(applied)
}

/// One `BlocksProcessed` event of the Zone SDK: the channel tip holds our batches up to `tip`,
/// the view moved (`channel_update`: an extension, or a conflict whose dropped batches of ours
/// are published again) and Bedrock finality reached the `finalized` entries, which go into
/// the final tier. A conflict the node does not confirm (its channel tip is still our newest
/// entry) is ignored: the SDK also reports a pending entry it could not see mined as orphaned,
/// and publishing it again would fork our own channel.
pub async fn on_blocks_processed(
    sdk: &mut ZoneSequencer<NodeHttpClient>,
    registry: &Shared,
    channel: &ZoneChannel,
    tip: [u8; 32],
    checkpoint: &SequencerCheckpoint,
    channel_update: &ChannelUpdate,
    finalized: &[FinalizedTx],
) -> Result<()> {
    set_checkpoint(registry, checkpoint)?;
    // The channel tip is one of our batches: it and every pending publish before it are in a
    // Bedrock block (an inscription we published is not echoed in `adopted` on a plain chain
    // extension).
    for msg in registry
        .write()
        .expect("registry lock")
        .mark_adopted_up_to(tip)
    {
        info!(
            "batch msg {} adopted in a Bedrock block (channel tip)",
            short(&msg)
        );
    }
    match channel_update {
        ChannelUpdate::Extension { adopted } => {
            let entries: Vec<ChannelEntry> = adopted
                .iter()
                .filter_map(|tx| tx.inscription())
                .map(entry_of)
                .collect();
            for entry in &entries {
                info!(
                    "channel entry msg {} adopted in a Bedrock block",
                    short(&entry.msg.into())
                );
            }
            registry
                .write()
                .expect("registry lock")
                .apply_zone_extension(entries);
            warn_lineage_gap(registry);
        }
        ChannelUpdate::Conflict { .. } => {
            let newest_held: Option<String> = registry
                .read()
                .expect("registry lock")
                .zone
                .view()
                .last()
                .map(|entry| hex::encode(<[u8; 32]>::from(entry.msg)));
            if let Some(on_chain) = channel_on_chain(&channel.node_url, channel.id).await?
                && newest_held.is_some_and(|m| m.eq_ignore_ascii_case(&on_chain.tip_message))
            {
                warn!(
                    "the SDK reports a conflict on the channel, but the node's channel tip is \
                     still our newest entry {}..: nothing was dropped, ignored",
                    &on_chain.tip_message[..8]
                );
            } else {
                let canonical: Vec<ChannelEntry> = channel_update
                    .canonical_chain()
                    .into_iter()
                    .flatten()
                    .filter_map(|tx| tx.inscription())
                    .map(entry_of)
                    .collect();
                let (dropped, publishes) = {
                    let mut registry = registry.write().expect("registry lock");
                    let dropped = registry.apply_zone_conflict(canonical);
                    (dropped, !registry.is_indexer())
                };
                warn_lineage_gap(registry);
                // A reorg dropped the sequencer's batches: it publishes the same payloads
                // again, in order (new messages, since their parent changed). An indexer
                // publishes nothing.
                for entry in dropped {
                    let Some(batch) = entry.batch else { continue };
                    if publishes {
                        republish(sdk, registry, entry.msg.into(), &batch).await?;
                    } else {
                        info!(
                            "batch [{}..{}] msg {} left the view in a reorg; the sequencer \
                             publishes it again",
                            batch.from_lez_block_id,
                            batch.to_lez_block_id,
                            short(&entry.msg.into())
                        );
                    }
                }
            }
        }
    }
    // Bedrock finality reached entries of the channel: into the final tier.
    for tx in finalized {
        let l1_slot = tx.l1_slot.into_inner();
        for op in &tx.ops {
            let FinalizedOp::Inscription(info) = op else {
                continue;
            };
            let entry = entry_of(info);
            let outcome = registry
                .write()
                .expect("registry lock")
                .apply_zone_finalized(entry.clone(), l1_slot);
            report_finalized(&entry, l1_slot, outcome.as_ref());
        }
    }
    registry.write().expect("registry lock").save()
}

/// Said once per channel update that left the view with a gap in its lineage.
pub fn warn_lineage_gap(registry: &Shared) {
    if let Some(gap) = registry
        .read()
        .expect("registry lock")
        .zone
        .check_for_gaps_in_lineage()
    {
        warn!(
            "channel entry msg {} chains on {}, which the view does not hold: the \
             lineage has a gap",
            short(&gap.msg.into()),
            short(&gap.parent.into())
        );
    }
}

/// Publish `batch` again after a reorg orphaned its message `orphaned`: a new message, since
/// its parent changed. A refusal by the SDK is logged and left to the next conflict or poll.
pub async fn republish(
    sdk: &mut ZoneSequencer<NodeHttpClient>,
    registry: &Shared,
    orphaned: [u8; 32],
    batch: &Batch,
) -> Result<()> {
    let inscription = Inscription::try_from(batch.encode_to_vec())
        .map_err(|e| anyhow!("payload too large: {e:?}"))?;
    match sdk.handle().publish(inscription).await {
        Ok((result, checkpoint)) => {
            let info = result.tx.inscription();
            let record = {
                let mut registry = registry.write().expect("registry lock");
                let record = registry.record_publish(info.this_msg, info.parent_msg, batch);
                registry.set_checkpoint(serde_json::to_value(&checkpoint)?);
                registry.save()?;
                record
            };
            info!(
                "batch [{}..{}] msg {} orphaned by a reorg, published again as msg {} \
                 ({record:?})",
                batch.from_lez_block_id,
                batch.to_lez_block_id,
                short(&orphaned),
                short(&info.this_msg.into())
            );
        }
        Err(e) => warn!("republish after reorg failed: {e}"),
    }
    Ok(())
}

/// One tick. The indexer judges what it could not judge before (the finalized tree moved).
/// The sequencer checks its own statements against the tree, then publishes one batch
/// covering the adopted LEZ blocks not covered yet when changes wait or the heartbeat is due,
/// with at most one batch in flight. `Err` only for the tree store.
pub async fn on_tick(
    sdk: &mut ZoneSequencer<NodeHttpClient>,
    registry: &Shared,
    heartbeat_sec: u64,
    contradicted_count: &mut usize,
    last_publish_unix: &mut u64,
) -> Result<()> {
    if registry.read().expect("registry lock").is_indexer() {
        let decided = registry.write().expect("registry lock").retry_folds();
        if decided > 0 {
            info!("{decided} batch(es) judged now that LEZ finality reached them");
            registry.write().expect("registry lock").save()?;
        }
        return Ok(());
    }
    warn_contradicted(&registry.read().expect("registry lock"), contradicted_count);
    let batch = {
        let registry = registry.read().expect("registry lock");
        if registry.in_flight().is_some() {
            return Ok(());
        }
        let due = registry.lineage().is_empty()
            || !registry.pending_changes.is_empty()
            || now_unix().saturating_sub(*last_publish_unix) >= heartbeat_sec;
        if due { registry.next_batch() } else { None }
    };
    let Some(batch) = batch else { return Ok(()) };
    publish_batch(sdk, registry, &batch, last_publish_unix).await
}

/// The sequencer's own batches that LEZ now contradicts (the tree at `to_lez_block_id` has
/// another root than the batch claims), warned once per change of the set.
pub fn warn_contradicted(registry: &Registry, contradicted_count: &mut usize) {
    let contradicted = registry.contradicted();
    if contradicted.len() == *contradicted_count {
        return;
    }
    *contradicted_count = contradicted.len();
    for e in contradicted {
        warn!(
            "batch msg {} claims root {}.. at LEZ block {}, LEZ now gives {}..: an \
             indexer parks on it and every later batch",
            short(&e.msg),
            short(&e.claimed),
            e.to_lez_block_id,
            short(&e.derived)
        );
    }
}

/// Publish the next batch: recorded as in flight and saved with the checkpoint on success, so
/// a restart knows it; `last_publish_unix` restarts the heartbeat. A batch too large for one
/// inscription or a refusal by the SDK is logged and tried again next poll. A batch that
/// would not apply on our own head is a bug: fatal before anything reaches Bedrock.
pub async fn publish_batch(
    sdk: &mut ZoneSequencer<NodeHttpClient>,
    registry: &Shared,
    batch: &Batch,
    last_publish_unix: &mut u64,
) -> Result<()> {
    if let Err(e) = registry.read().expect("registry lock").would_apply(batch) {
        bail!(
            "batch [{}..{}] would not apply on our own head ({e}); stopping before publishing \
             it",
            batch.from_lez_block_id,
            batch.to_lez_block_id
        );
    }
    let inscription = match Inscription::try_from(batch.encode_to_vec()) {
        Ok(inscription) => inscription,
        Err(e) => {
            warn!(
                "batch [{}..{}] does not fit one inscription: {e:?}",
                batch.from_lez_block_id, batch.to_lez_block_id
            );
            return Ok(());
        }
    };
    match sdk.handle().publish(inscription).await {
        Ok((result, checkpoint)) => {
            let info = result.tx.inscription();
            let record = {
                let mut registry = registry.write().expect("registry lock");
                let record = registry.record_publish(info.this_msg, info.parent_msg, batch);
                registry.set_checkpoint(serde_json::to_value(&checkpoint)?);
                registry.save()?;
                record
            };
            *last_publish_unix = now_unix();
            let described: Vec<_> = batch.changes.iter().map(|c| c.describe()).collect();
            info!(
                "batch [{}..{}] published as msg {} on {}: root {}.., {}{}",
                batch.from_lez_block_id,
                batch.to_lez_block_id,
                short(&info.this_msg.into()),
                short(&info.parent_msg.into()),
                hex::encode(&batch.root_after[..4]),
                if described.is_empty() {
                    "no changes".into()
                } else {
                    described.join("; ")
                },
                if record == PublishRecord::Extended {
                    "".into()
                } else {
                    format!(" ({record:?})")
                }
            );
            // Checked above; a disagreement here means the head moved under the publish.
            if record == PublishRecord::Skipped {
                let error = registry
                    .read()
                    .expect("registry lock")
                    .zone
                    .view_outcomes()
                    .last()
                    .and_then(|(_, outcome)| outcome.as_ref().err().map(ToString::to_string))
                    .unwrap_or_default();
                bail!(
                    "batch msg {} does not apply on our own head ({error}); stopping instead of \
                     publishing it again",
                    short(&info.this_msg.into())
                );
            }
        }
        Err(SeqError::Unavailable { reason }) => {
            info!("not ready to publish ({reason}), retry next poll")
        }
        Err(e) => warn!("publish failed: {e}"),
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use std::sync::{Arc, RwLock};

    use super::*;
    use crate::lez::{Block, BlockHeader};

    fn block(block_id: u64) -> Block {
        Block {
            header: BlockHeader {
                block_id,
                prev_block_hash: common::HashType([block_id as u8 - 1; 32]),
                hash: common::HashType([block_id as u8; 32]),
                timestamp: 1_000,
            },
            events: vec![],
            private_transaction_count: 0,
        }
    }

    /// A sequencer store with two batches published: msg 1 landed (the tip the node reports),
    /// msg 2 still in flight.
    fn store(mark_adopted: &[[u8; 32]]) -> Shared {
        let mut registry = Registry::open_temporary(1, false).unwrap();
        registry.apply_lez_adopted(&block(1)).unwrap();
        let first = registry.next_batch().unwrap();
        registry.record_publish(MsgId::from([1; 32]), MsgId::root(), &first);
        registry.apply_lez_adopted(&block(2)).unwrap();
        let second = registry.next_batch().unwrap();
        registry.record_publish(MsgId::from([2; 32]), MsgId::from([1; 32]), &second);
        for msg in mark_adopted {
            registry.mark_adopted_up_to(*msg);
        }
        Arc::new(RwLock::new(registry))
    }

    fn on_chain(tip: [u8; 32]) -> ChannelOnChain {
        ChannelOnChain {
            accredited_keys: vec![hex::encode([7; 32])],
            tip_message: hex::encode(tip),
        }
    }

    #[test]
    fn test_a_sequencer_restarts_with_a_batch_in_flight() {
        // The node's tip is our first batch, the second is in flight: start.
        check_channel_on_chain(&on_chain([1; 32]), &[7; 32], &store(&[[1; 32]]), true).unwrap();
        // The node's tip is our newest batch: start.
        check_channel_on_chain(&on_chain([2; 32]), &[7; 32], &store(&[[2; 32]]), true).unwrap();
        // Our second batch counts as adopted here but the node's tip is still the first: the
        // data dir is not the channel's state.
        assert!(
            check_channel_on_chain(&on_chain([1; 32]), &[7; 32], &store(&[[2; 32]]), true).is_err()
        );
        // No checkpoint: the SDK would chain on the root.
        assert!(
            check_channel_on_chain(&on_chain([1; 32]), &[7; 32], &store(&[[1; 32]]), false)
                .is_err()
        );
        // A tip that is not ours, another key.
        assert!(
            check_channel_on_chain(&on_chain([9; 32]), &[7; 32], &store(&[[1; 32]]), true).is_err()
        );
        assert!(
            check_channel_on_chain(&on_chain([1; 32]), &[8; 32], &store(&[[1; 32]]), true).is_err()
        );
    }
}
