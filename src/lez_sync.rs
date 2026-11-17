//! The LEZ channel as the node follows it through the Zone SDK: the checkpoint at the LIB on
//! a fresh store, the finalized history filled from the LEZ Sequencer under the hash chain,
//! the SDK's `BlocksProcessed` events folded into `LezChain` (adopted blocks onto the head, a
//! conflict refolding it, finalized blocks onto the finalized tree).

use anyhow::{Context as _, Result, anyhow, bail};
use lb_core::mantle::ops::channel::{ChannelId, MsgId};
use lb_zone_sdk::{
    Slot,
    adapter::{Node as _, NodeHttpClient},
    sequencer::{
        ChannelUpdate, FinalizedOp, FinalizedTx, InscriptionInfo, SequencerCheckpoint,
        channel_inscriptions,
    },
};
use tracing::{info, warn};

use crate::{
    http::Shared,
    lez::{self, Block, LezSequencer},
    lez_chain::check_filler,
    zone_sync::{FINALITY_SCAN_PAGE, short},
};

/// Pages of `FINALITY_SCAN_PAGE` finalized L1 slots read backwards from the LIB to find the
/// newest LEZ inscription (LEZ inscribes a block every minute or two).
pub const LEZ_INSCRIPTION_SCAN_PAGES: u64 = 60;

/// The Zone SDK checkpoint on the LEZ channel; written with the state, like `set_checkpoint`.
pub fn set_lez_checkpoint(registry: &Shared, checkpoint: &SequencerCheckpoint) -> Result<()> {
    registry
        .write()
        .expect("registry lock")
        .set_lez_checkpoint(serde_json::to_value(checkpoint)?);
    Ok(())
}

/// The checkpoint the LEZ channel's reader starts from on a fresh store: at the node's LIB,
/// on the newest LEZ inscription at or below it (found by reading the finalized L1 blocks
/// backwards from the LIB, a page at a time), so the SDK walks nothing before. Returns that
/// inscription's LEZ block too: the finalized tree is built up to it.
pub async fn lez_checkpoint_at_lib(
    node: &NodeHttpClient,
    lez_channel_id: ChannelId,
    source: &LezSequencer,
) -> Result<(SequencerCheckpoint, Block)> {
    let info = node
        .consensus_info()
        .await
        .map_err(|e| anyhow!("consensus info: {e}"))?
        .cryptarchia_info;
    let lib_slot = u64::from(info.lib_slot);
    let mut page_to = lib_slot;
    for _ in 0..LEZ_INSCRIPTION_SCAN_PAGES {
        let page_from = page_to.saturating_sub(FINALITY_SCAN_PAGE - 1);
        let blocks = node
            .immutable_blocks(Slot::from(page_from), Slot::from(page_to))
            .await
            .map_err(|e| anyhow!("immutable blocks {page_from}..{page_to}: {e}"))?;
        let newest = blocks
            .iter()
            .rev()
            .flat_map(|block| block.transactions.iter().rev())
            .flat_map(|tx| channel_inscriptions(tx, lez_channel_id).into_iter().rev())
            .next();
        if let Some(inscription) = newest {
            let block = source
                .decode_block(&inscription.payload)
                .context("the newest finalized LEZ inscription is not a LEZ block")?;
            return Ok((
                SequencerCheckpoint {
                    last_msg_id: inscription.this_msg,
                    pending_txs: vec![],
                    lib: info.lib,
                    lib_slot: info.lib_slot,
                    channel_notes: vec![],
                    finalized_config: MsgId::root(),
                    funding: vec![],
                },
                block,
            ));
        }
        if page_from == 0 {
            break;
        }
        page_to = page_from - 1;
    }
    bail!(
        "no LEZ inscription in the {} finalized L1 slots below the LIB ({lib_slot}): is \
         LEZ_CHANNEL_ID the LEZ channel?",
        LEZ_INSCRIPTION_SCAN_PAGES * FINALITY_SCAN_PAGE
    )
}

/// The LEZ blocks `expected..block_id` missing before `block`, read from the LEZ Sequencer.
/// `None` when the RPC did not answer: logged, nothing applied, the gap is filled under the
/// next block Bedrock reports (or on a restart).
pub async fn read_filler(
    source: &LezSequencer,
    expected: u64,
    block: &Block,
    under: &str,
) -> Option<Vec<Block>> {
    let to = block.block_id() - 1;
    info!(
        "LEZ blocks {expected}..{to} read from the LEZ Sequencer under {under} block {}",
        block.block_id()
    );
    match source.blocks(expected, to).await {
        Ok(filler) => Some(filler),
        Err(e) => {
            warn!(
                "LEZ blocks {expected}..{to} not read: {e}; {under} block {} waits for the next \
                 one",
                block.block_id()
            );
            None
        }
    }
}

/// A LEZ block Bedrock finalized: onto the finalized tree, the blocks it skipped filled from
/// the LEZ Sequencer first. The filler is trusted only through the hash chain: it must chain
/// on the final block and lead to this block's `prev_block_hash`, checked before anything is
/// applied; a history that does not is a LEZ Sequencer that does not follow Bedrock, fatal
/// with the store intact. An RPC that does not answer is logged, the block waits.
pub async fn apply_lez_finalized(
    source: &LezSequencer,
    registry: &Shared,
    block: &Block,
) -> Result<()> {
    let gap = registry
        .write()
        .expect("registry lock")
        .apply_lez_finalized(block)?;
    let Some(expected) = gap else {
        return Ok(());
    };
    let Some(filler) = read_filler(source, expected, block, "finalized").await else {
        return Ok(());
    };
    let prev = registry
        .read()
        .expect("registry lock")
        .lez
        .final_reader()
        .last_block_hash();
    check_filler(prev, expected, &filler, block).map_err(|e| {
        anyhow!(
            "the LEZ Sequencer's history does not lead to LEZ block {}, which Bedrock finalized \
             ({e}): point LEZ_SEQUENCER_URL at a sequencer that follows Bedrock",
            block.block_id()
        )
    })?;
    let mut registry = registry.write().expect("registry lock");
    for candidate in filler.iter().chain(std::iter::once(block)) {
        if registry.apply_lez_finalized(candidate)?.is_some() {
            bail!(
                "LEZ block {} still does not follow the finalized tree",
                candidate.block_id()
            );
        }
    }
    Ok(())
}

/// A LEZ block adopted on Bedrock, onto the head. Blocks the SDK skipped before it (a stream
/// it dropped and reopened, entries it does not echo) are read from the LEZ Sequencer first,
/// trusted only through the hash chain: they must chain on the head and lead to the adopted
/// block, or nothing is applied.
pub async fn adopt_lez_block(
    source: &LezSequencer,
    registry: &Shared,
    block: &Block,
) -> Result<()> {
    let gap = registry
        .write()
        .expect("registry lock")
        .apply_lez_adopted(block)?;
    let Some(expected) = gap else {
        return Ok(());
    };
    let Some(filler) = read_filler(source, expected, block, "adopted").await else {
        return Ok(());
    };
    let prev = registry
        .read()
        .expect("registry lock")
        .head()
        .last_block_hash();
    check_filler(prev, expected, &filler, block).map_err(|e| {
        anyhow!(
            "the LEZ Sequencer's blocks do not lead to adopted LEZ block {} ({e}); not applied",
            block.block_id()
        )
    })?;
    let mut registry = registry.write().expect("registry lock");
    for candidate in filler.iter().chain(std::iter::once(block)) {
        if registry.apply_lez_adopted(candidate)?.is_some() {
            bail!(
                "adopted LEZ block {} still does not follow the head",
                candidate.block_id()
            );
        }
    }
    Ok(())
}

/// One `BlocksProcessed` event of the Zone SDK on the LEZ channel: adopted LEZ blocks onto the
/// head, a Bedrock reorg refolds the head over the canonical lineage, finalized LEZ blocks onto
/// the finalized tree. The events of a block are logged when the head first sees it.
pub async fn on_lez_blocks_processed(
    source: &LezSequencer,
    registry: &Shared,
    checkpoint: &SequencerCheckpoint,
    channel_update: &ChannelUpdate,
    finalized: &[FinalizedTx],
) -> Result<()> {
    set_lez_checkpoint(registry, checkpoint)?;
    let decode = |info: &InscriptionInfo| match source.decode_block(&info.payload) {
        Ok(block) => Some(block),
        Err(e) => {
            warn!(
                "LEZ channel entry msg {} is not a LEZ block: {e}",
                short(&info.this_msg.into())
            );
            None
        }
    };
    match channel_update {
        ChannelUpdate::Extension { adopted } => {
            for block in adopted
                .iter()
                .filter_map(|tx| tx.inscription())
                .filter_map(decode)
            {
                if !block.events.is_empty() {
                    let names: Vec<_> = block.events.iter().map(lez::selector_name).collect();
                    info!(
                        "LEZ block {} adopted: events {}",
                        block.block_id(),
                        names.join(", ")
                    );
                }
                if let Err(e) = adopt_lez_block(source, registry, &block).await {
                    warn!("{e}");
                }
            }
        }
        ChannelUpdate::Conflict { .. } => {
            let canonical: Vec<Block> = channel_update
                .canonical_chain()
                .into_iter()
                .flatten()
                .filter_map(|tx| tx.inscription())
                .filter_map(decode)
                .collect();
            registry
                .write()
                .expect("registry lock")
                .apply_lez_conflict()?;
            for block in &canonical {
                if let Err(e) = adopt_lez_block(source, registry, block).await {
                    warn!("{e}");
                    break;
                }
            }
            info!(
                "LEZ channel reorg on Bedrock: the head is the finalized tree folded over the \
                 canonical lineage again, at LEZ block {}",
                registry.read().expect("registry lock").head().lez_block_id
            );
        }
    }
    for tx in finalized {
        for op in &tx.ops {
            let FinalizedOp::Inscription(info) = op else {
                continue;
            };
            let Some(block) = decode(info) else { continue };
            apply_lez_finalized(source, registry, &block).await?;
        }
    }
    registry.write().expect("registry lock").save()
}
