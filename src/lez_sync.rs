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
    reader::short,
    zone_sync::FINALITY_SCAN_PAGE,
};

/// Pages of the LEZ inscription scan between two progress lines.
const LEZ_INSCRIPTION_SCAN_REPORT_PAGES: u64 = 50;

/// Pages of `FINALITY_SCAN_PAGE` finalized Bedrock slots read backwards from the LIB to find the
/// newest LEZ inscription: LEZ inscribes a block every minute or two, so the first page has
/// one as a rule; the scan goes on (a page per node call) through the hours a LEZ sequencer
/// may have been down, and gives up after this many.
pub const LEZ_INSCRIPTION_SCAN_PAGES: u64 = 2_000;

/// The checkpoint the LEZ channel's reader starts from on a fresh store: at the node's LIB,
/// on the newest LEZ inscription at or below it (found by reading the finalized Bedrock blocks
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
    for page in 0..LEZ_INSCRIPTION_SCAN_PAGES {
        if page > 0 && page % LEZ_INSCRIPTION_SCAN_REPORT_PAGES == 0 {
            info!(
                "no LEZ inscription in the {} finalized Bedrock slots below the LIB yet ({page_to}..\
                 {lib_slot}); reading further back",
                page * FINALITY_SCAN_PAGE
            );
        }
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
        "no LEZ inscription in the {} finalized Bedrock slots below the LIB ({lib_slot}): is \
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
    if !block.events.is_empty()
        && registry
            .read()
            .expect("registry lock")
            .lez
            .final_reader()
            .lez_block_id
            < block.block_id()
    {
        info!(
            "LEZ block {} finalized on Bedrock: {}",
            block.block_id(),
            lez::describe_events(&block.events)
        );
    }
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
/// the finalized tree. The events of an adopted block are logged whenever the SDK reports it
/// (held blocks after a restart included), those of a finalized block when the final tier
/// first sees it.
pub async fn on_lez_blocks_processed(
    source: &LezSequencer,
    registry: &Shared,
    channel_update: &ChannelUpdate,
    finalized: &[FinalizedTx],
) -> Result<()> {
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
                    info!(
                        "LEZ block {} adopted on Bedrock: {}",
                        block.block_id(),
                        lez::describe_events(&block.events)
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

#[cfg(test)]
mod test {
    use std::{
        future::IntoFuture as _,
        sync::{Arc, Mutex, RwLock},
    };

    use axum::{Json, Router, extract::State, routing::post};
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use serde_json::{Value, json};

    use super::*;
    use crate::{lez::test_blocks::chained_block, registry::Registry};

    /// A LEZ Sequencer JSON-RPC stub serving `getBlockRange` over a chain of empty blocks:
    /// a range wider than `too_big_over` is refused with LEZ's `-32008`, `short` answers one
    /// block fewer than asked, `tamper` serves another block 4 (same id, other timestamp). The
    /// widths asked are recorded.
    struct Stub {
        too_big_over: u64,
        short: bool,
        tamper: bool,
        widths: Mutex<Vec<u64>>,
    }

    async fn rpc(State(stub): State<Arc<Stub>>, Json(request): Json<Value>) -> Json<Value> {
        match request["method"].as_str() {
            Some("getLastBlockId") => Json(json!({ "jsonrpc": "2.0", "id": 1, "result": 5 })),
            Some("getBlockRange") => {
                let from = request["params"][0].as_u64().unwrap();
                let to = request["params"][1].as_u64().unwrap();
                stub.widths.lock().unwrap().push(to - from + 1);
                if to - from + 1 > stub.too_big_over {
                    return Json(json!({ "jsonrpc": "2.0", "id": 1, "error":
                        { "code": -32008, "message": "Response is too big" } }));
                }
                let upto = if stub.short { to - 1 } else { to };
                let blocks: Vec<String> = (from..=upto)
                    .map(|id| BASE64.encode(chained_block(id, stub.tamper && id == 4)))
                    .collect();
                Json(json!({ "jsonrpc": "2.0", "id": 1, "result": blocks }))
            }
            _ => Json(json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32601 } })),
        }
    }

    async fn serve(stub: Arc<Stub>) -> String {
        let app = Router::new().route("/", post(rpc)).with_state(stub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, app).into_future());
        url
    }

    fn stub(too_big_over: u64, short: bool, tamper: bool) -> Arc<Stub> {
        Arc::new(Stub {
            too_big_over,
            short,
            tamper,
            widths: Mutex::new(vec![]),
        })
    }

    /// `blocks` halves the range on LEZ's "too big" answer down to one block, and refuses an
    /// answer with fewer blocks than asked.
    #[tokio::test]
    async fn test_blocks_halve_the_range_on_a_too_big_answer_and_refuse_a_short_one() {
        let big = stub(2, false, false);
        let source = LezSequencer::new(serve(big.clone()).await, [1; 32], [7; 32]);
        let blocks = source.blocks(1, 5).await.unwrap();
        assert_eq!(
            blocks.iter().map(Block::block_id).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        let widths = big.widths.lock().unwrap().clone();
        assert_eq!(widths[0], 5);
        assert!(widths[1..].iter().all(|w| *w <= 2));
        let short = stub(1_024, true, false);
        let source = LezSequencer::new(serve(short).await, [1; 32], [7; 32]);
        let error = source.blocks(1, 3).await.unwrap_err().to_string();
        assert!(error.contains("returned 2 blocks"), "{error}");
    }

    /// The fill under a finalized block (every start takes it): the blocks the LEZ Sequencer
    /// serves are applied only when they chain from the final block to the finalized one; a
    /// history that does not lead to it is refused with the store intact. The same fill under
    /// an adopted block, and the preview reading the stub's newest block.
    #[tokio::test]
    async fn test_the_fill_under_a_finalized_block_is_trusted_through_the_hash_chain() {
        let honest = stub(1_024, false, false);
        let source = LezSequencer::new(serve(honest).await, [1; 32], [7; 32]);
        let registry: Shared = Arc::new(RwLock::new(Registry::open_temporary(1, false).unwrap()));
        let first = source.decode_block(&chained_block(1, false)).unwrap();
        let fifth = source.decode_block(&chained_block(5, false)).unwrap();
        registry
            .write()
            .unwrap()
            .apply_lez_finalized(&first)
            .unwrap();
        apply_lez_finalized(&source, &registry, &fifth)
            .await
            .unwrap();
        assert_eq!(registry.read().unwrap().lez.final_reader().lez_block_id, 5);
        assert_eq!(registry.read().unwrap().head().lez_block_id, 5);

        let tampered = stub(1_024, false, true);
        let source = LezSequencer::new(serve(tampered).await, [1; 32], [7; 32]);
        let registry: Shared = Arc::new(RwLock::new(Registry::open_temporary(1, false).unwrap()));
        registry
            .write()
            .unwrap()
            .apply_lez_finalized(&first)
            .unwrap();
        let error = apply_lez_finalized(&source, &registry, &fifth)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not lead to LEZ block 5"), "{error}");
        assert_eq!(registry.read().unwrap().lez.final_reader().lez_block_id, 1);

        let honest = stub(1_024, false, false);
        let source = LezSequencer::new(serve(honest).await, [1; 32], [7; 32]);
        adopt_lez_block(&source, &registry, &fifth).await.unwrap();
        assert_eq!(registry.read().unwrap().head().lez_block_id, 5);
        assert_eq!(registry.read().unwrap().lez.final_reader().lez_block_id, 1);
        let mut failed = true;
        crate::zone_sync::refresh_preview(&source, &registry, &mut failed).await;
        assert!(!failed);
        assert_eq!(registry.read().unwrap().sequencer_block_id(), Some(5));
    }
}
