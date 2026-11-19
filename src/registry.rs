//! One node's state: the LEZ chain as the Zone SDK reports it (`LezChain`: the finalized tree
//! on disk, the adopted blocks folded over it), the Zone channel as the SDK reports it
//! (`ZoneChain`), the changes no batch carries yet, and the Zone SDK checkpoint on the Zone
//! channel. It lives under the data dir in small pieces, each written only when it changed:
//! the tree store (`tree/`, sled), the reader's state (`reader.json`), the Zone channel
//! (`zone.json`, plus `final.jsonl`, one line per finalized entry, appended) and the adopted
//! LEZ blocks (`lez_view.json`). The LEZ channel has no checkpoint on disk: a start computes
//! one at the LIB, and the finalized tree is filled up to it from the LEZ Sequencer under the
//! hash chain. The files are written in an order a crash between two writes cannot break: the
//! final tier first (what the Zone SDK never reports again), then `zone.json`, then the LEZ
//! view and the reader (what the LEZ channel reports again from the LIB).

use std::{
    collections::HashSet,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use lb_core::mantle::ops::channel::{MsgId, inscribe::MAX_BYTES as MAX_INSCRIPTION_BYTES};
use prost::Message as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::{
    batch::{Batch, Change},
    lez::{Block, ProgramEvent, RlnEvent, StoredBlock},
    lez_chain::{Adopted, Finalized, LezChain},
    reader::{BlockIngestError, EMPTY_ROOT, READER_FILE, Reader, TREE_DIR, parse_32, write_atomic},
    zone_chain::{
        AcceptOutcome, BatchIngestError, ChannelEntry, FinalEntry, Judge, PublishRecord, Tip,
        ZoneChain,
    },
};

/// A state file past this size is warned about: the state is meant to stay small (bytes per
/// member, per batch, per adopted block).
const STATE_FILE_WARN_BYTES: usize = 1 << 20;

/// The indexer judges a batch against the tree of the finalized LEZ blocks only.
impl Judge for LezChain {
    fn root_at(&self, to_lez_block_id: u64) -> Option<[u8; 32]> {
        self.final_reader().root_at(to_lez_block_id)
    }
}

/// Where the final tier was last read up to on Bedrock (LEZ `ZoneAnchorRecord`): the slot the
/// finality scan continues after, and the final tip there (kept for inspection).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ZoneAnchorRecord {
    pub slot: u64,
    pub to_lez_block_id: u64,
    #[serde(with = "hex::serde")]
    pub root: [u8; 32],
}

/// Where a channel entry stands on Bedrock, the Zone SDK's word: `pending` (a publish of ours
/// no Bedrock block holds yet), `adopted` (in a Bedrock block on the current branch: the view)
/// or `finalized` (the final tier).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BedrockStatus {
    Pending,
    Adopted,
    Finalized,
}

/// A batch on the channel whose root LEZ has since contradicted: the tree derived from LEZ
/// says `derived` at its `to_lez_block_id`, the batch claims `claimed`.
#[derive(Debug, Clone, PartialEq)]
pub struct Contradicted {
    pub msg: [u8; 32],
    pub to_lez_block_id: u64,
    pub claimed: [u8; 32],
    pub derived: [u8; 32],
}

/// A pending change as `zone.json` keeps it: the LEZ block that made it and the change, prost
/// in hex.
#[derive(Serialize, Deserialize)]
struct PendingChangeRecord {
    block: u64,
    change: String,
}

/// The Zone channel side of the state (`zone.json`): the view and what goes with it. Small,
/// rewritten when it changed (the checkpoint moves with every Zone SDK event).
#[derive(Serialize, Deserialize)]
struct ZoneState {
    first_lez_block_id: u64,
    /// The view in its persisted form (`ZoneChain::encode_view`), hex.
    view: String,
    anchor: Option<ZoneAnchorRecord>,
    /// Publishes of ours not seen in a Bedrock block yet, hex, in publish order.
    own_pending: Vec<String>,
    orphaned: u64,
    pending_changes: Vec<PendingChangeRecord>,
    /// Orphaned batches of ours whose publish again failed, prost in hex, in publish order: the
    /// tick publishes them before any new batch.
    #[serde(default)]
    to_republish: Vec<String>,
    /// The Zone SDK checkpoint on the Zone channel, its `channel_notes` emptied.
    checkpoint: Option<Value>,
}

/// The Zone channel state file under a data dir.
pub const ZONE_FILE: &str = "zone.json";
/// The finalized entries of the Zone channel under a data dir: one JSON line each, appended.
pub const FINAL_FILE: &str = "final.jsonl";
/// The adopted LEZ blocks under a data dir.
pub const LEZ_VIEW_FILE: &str = "lez_view.json";

/// What a data dir held when the registry opened.
struct StateFiles {
    dir: Option<PathBuf>,
    zone: Option<ZoneState>,
    zone_bytes: Vec<u8>,
    final_entries: Vec<FinalEntry>,
    /// `final.jsonl` must be written whole at the next save: a torn line was dropped, or the
    /// entries were changed on the way in.
    final_rewrite: bool,
    lez_view: Vec<StoredBlock>,
    lez_view_bytes: Vec<u8>,
}

impl StateFiles {
    #[cfg(test)]
    fn none() -> Self {
        Self {
            dir: None,
            zone: None,
            zone_bytes: vec![],
            final_entries: vec![],
            final_rewrite: false,
            lez_view: vec![],
            lez_view_bytes: vec![],
        }
    }

    fn read(dir: &Path) -> Result<Self> {
        let read = |name: &str| -> Result<Option<Vec<u8>>> {
            let path = dir.join(name);
            if !path.exists() {
                return Ok(None);
            }
            fs::read(&path)
                .map(Some)
                .with_context(|| format!("read {}", path.display()))
        };
        let zone_bytes = read(ZONE_FILE)?.unwrap_or_default();
        let zone = (!zone_bytes.is_empty())
            .then(|| serde_json::from_slice(&zone_bytes).context(ZONE_FILE))
            .transpose()?;
        let lez_view_bytes = read(LEZ_VIEW_FILE)?.unwrap_or_default();
        let lez_view = if lez_view_bytes.is_empty() {
            vec![]
        } else {
            serde_json::from_slice(&lez_view_bytes).context(LEZ_VIEW_FILE)?
        };
        // One finalized entry per line; a torn last line (a crash while appending) is dropped
        // with a warning and the file is written whole at the next save; the entry comes again
        // from the finality scan (the files are written final tier first, so the anchor is
        // still before it).
        let mut final_entries = vec![];
        let mut final_rewrite = false;
        if let Some(bytes) = read(FINAL_FILE)? {
            for (i, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<FinalEntry>(line) {
                    Ok(entry) => final_entries.push(entry),
                    Err(e) => {
                        warn!(
                            "{FINAL_FILE} line {}: {e}; the line and the rest are dropped",
                            i + 1
                        );
                        final_rewrite = true;
                        break;
                    }
                }
            }
        }
        Ok(Self {
            dir: Some(dir.to_path_buf()),
            zone,
            zone_bytes,
            final_entries,
            final_rewrite,
            lez_view,
            lez_view_bytes,
        })
    }
}

/// One node's state: the LEZ chain, the Zone channel, the changes no batch carries yet, the
/// publishes in flight and the Zone SDK checkpoint, persisted under the data dir.
pub struct Registry {
    /// The LEZ chain: the finalized tree on disk, the adopted blocks folded over it.
    pub lez: LezChain,
    /// The channel: final tier and view.
    pub zone: ZoneChain,
    pub first_lez_block_id: u64,
    /// Changes of the LEZ blocks after the head tip, in derivation-rule order, each with the
    /// block that made it: the next batch's content for the sequencer, the head's lead over
    /// the channel for the indexer; trimmed as the tip advances.
    pub pending_changes: Vec<(u64, Change)>,
    /// Our own publishes not seen in a Bedrock block yet, in publish order.
    own_pending: Vec<[u8; 32]>,
    /// Publishes of ours a Bedrock reorg orphaned (the SDK's word), published again.
    orphaned: u64,
    /// Orphaned batches whose publish again failed, in publish order: the tick publishes them
    /// before any new batch, so their changes (trimmed from `pending_changes` when they were
    /// first published) reach the channel.
    to_republish: Vec<Batch>,
    anchor: Option<ZoneAnchorRecord>,
    /// The Zone SDK checkpoint on the Zone channel, as JSON, `channel_notes` emptied.
    checkpoint: Option<Value>,
    /// The indexer judges every batch against its tree; the sequencer trusts its own.
    indexer: bool,
    /// Where the state files live; `None` for a temporary registry, which persists nothing.
    state_dir: Option<PathBuf>,
    /// What `zone.json` and `lez_view.json` hold, so unchanged state is not written again.
    written_zone: Vec<u8>,
    written_lez_view: Vec<u8>,
    /// How many final entries `final.jsonl` holds, and whether an entry it holds changed
    /// (the judge decided one) so the file is written whole again.
    final_written: usize,
    final_rewrite: bool,
    /// LEZ blocks the LEZ Sequencer produced beyond the head, not on Bedrock yet, ascending:
    /// a preview for the API and the log, never batched, never persisted. Trimmed as the
    /// head moves.
    preview: Vec<Block>,
    /// The newest block the LEZ Sequencer said it holds (`getLastBlockId`), when asked.
    sequencer_block_id: Option<u64>,
}

impl Registry {
    /// Open the state under `data_dir` (created when missing); a fresh one starts at
    /// `first_lez_block_id`. With `rebuild`, the tree, the reader's state and the adopted
    /// LEZ blocks are dropped and read again from LEZ, while the Zone channel state is kept;
    /// an indexer then judges again the finalized batches the old tree parked, once the new
    /// tree reaches them.
    pub fn open(
        data_dir: &Path,
        tree_id: [u8; 32],
        first_lez_block_id: u64,
        rebuild: bool,
        indexer: bool,
    ) -> Result<Self> {
        fs::create_dir_all(data_dir).with_context(|| format!("create {}", data_dir.display()))?;
        if rebuild {
            let tree = data_dir.join(TREE_DIR);
            if tree.exists() {
                fs::remove_dir_all(&tree).context("drop the tree store")?;
            }
            for name in [READER_FILE, LEZ_VIEW_FILE] {
                let path = data_dir.join(name);
                if path.exists() {
                    fs::remove_file(&path).with_context(|| format!("drop {}", path.display()))?;
                }
            }
            info!("tree store dropped; the tree is read again from LEZ");
        }
        let final_reader = Reader::open(data_dir, tree_id, first_lez_block_id.saturating_sub(1))?;
        let mut files = StateFiles::read(data_dir)?;
        if rebuild && indexer {
            for entry in &mut files.final_entries {
                if let Some(error) = &entry.error
                    && !error.is_retryable()
                    && let Some(batch) = entry.batch()
                {
                    entry.error = Some(BatchIngestError::LezNotRead {
                        to: batch.to_lez_block_id,
                    });
                    files.final_rewrite = true;
                }
            }
        }
        Self::from_parts(final_reader, files, first_lez_block_id, indexer)
    }

    /// A throwaway store, for tests: nothing is written.
    #[cfg(test)]
    pub fn open_temporary(first_lez_block_id: u64, indexer: bool) -> Result<Self> {
        let final_reader = Reader::open_temporary([0; 32], first_lez_block_id.saturating_sub(1))?;
        Self::from_parts(
            final_reader,
            StateFiles::none(),
            first_lez_block_id,
            indexer,
        )
    }

    fn from_parts(
        final_reader: Reader,
        files: StateFiles,
        first_lez_block_id: u64,
        indexer: bool,
    ) -> Result<Self> {
        if let Some(zone) = &files.zone
            && zone.first_lez_block_id != first_lez_block_id
        {
            bail!(
                "{ZONE_FILE} starts at LEZ block {}, FIRST_LEZ_BLOCK_ID says {first_lez_block_id}: \
                 another registry, or another data dir",
                zone.first_lez_block_id
            );
        }
        let mut lez = LezChain::new(final_reader)?;
        // The adopted LEZ blocks of the last run: the LEZ reader starts at the LIB on every
        // start, so the SDK reports them again, minutes later; the file brings them back at
        // once, their changes pending again below.
        let blocks = files
            .lez_view
            .iter()
            .map(Block::from_stored)
            .collect::<Result<Vec<_>>>()?;
        let restored_view = lez.restore_view(&blocks)?;
        let view = files
            .zone
            .as_ref()
            .map(|z| hex::decode(&z.view).context("view in zone.json"))
            .transpose()?
            .unwrap_or_default();
        let final_written = files.final_entries.len();
        let judge = indexer.then_some(&lez as &dyn Judge);
        let zone = ZoneChain::restore(first_lez_block_id, files.final_entries, &view, judge)?;
        // A publish of ours is in flight only while the view holds it: `zone.json` may be
        // behind the final tier by one write.
        let held: HashSet<[u8; 32]> = zone.view().iter().map(|e| e.msg.into()).collect();
        let own_pending = files.zone.as_ref().map_or(Ok(vec![]), |z| {
            z.own_pending
                .iter()
                .map(|h| parse_32(h, "own_pending msg"))
                .filter(|msg| msg.as_ref().is_ok_and(|m| held.contains(m)))
                .collect::<Result<Vec<_>>>()
        })?;
        // The changes applied but not yet in a batch survive a restart with the tree they
        // changed, as far as the final tier goes (the adopted blocks bring theirs again below).
        let final_block_id = lez.final_reader().lez_block_id;
        let pending_changes = files.zone.as_ref().map_or(Ok(vec![]), |z| {
            z.pending_changes
                .iter()
                .filter(|record| record.block <= final_block_id)
                .map(|record| {
                    let bytes = hex::decode(&record.change).context("pending change is not hex")?;
                    let change = Change::decode(bytes.as_slice()).context("pending change")?;
                    Ok((record.block, change))
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let to_republish = files.zone.as_ref().map_or(Ok(vec![]), |z| {
            z.to_republish
                .iter()
                .map(|h| {
                    let bytes = hex::decode(h).context("batch to publish again is not hex")?;
                    Batch::decode(bytes.as_slice()).context("batch to publish again")
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let mut registry = Self {
            lez,
            zone,
            first_lez_block_id,
            pending_changes,
            own_pending,
            orphaned: files.zone.as_ref().map_or(0, |z| z.orphaned),
            to_republish,
            anchor: files.zone.as_ref().and_then(|z| z.anchor),
            checkpoint: files.zone.as_ref().and_then(|z| z.checkpoint.clone()),
            indexer,
            state_dir: files.dir,
            written_zone: files.zone_bytes,
            written_lez_view: files.lez_view_bytes,
            final_written,
            final_rewrite: files.final_rewrite,
            preview: vec![],
            sequencer_block_id: None,
        };
        for (block_id, changes) in restored_view {
            registry.add_pending(block_id, changes);
        }
        Ok(registry)
    }

    /// Write what changed, in the order a crash between two writes leaves recoverable: the
    /// finalized entries appended to `final.jsonl` (the Zone SDK never reports them again),
    /// then `zone.json`, then `lez_view.json` and the reader's state (the LEZ channel reports
    /// the adopted blocks again from the LIB, and `zone.json` keeps the pending changes of a
    /// finalized block the reader has not saved yet). A temporary registry writes nothing.
    pub fn save(&mut self) -> Result<()> {
        let Some(dir) = self.state_dir.clone() else {
            return Ok(());
        };
        self.save_final(&dir)?;
        let zone = ZoneState {
            first_lez_block_id: self.first_lez_block_id,
            view: hex::encode(self.zone.encode_view()),
            anchor: self.anchor,
            own_pending: self.own_pending.iter().map(hex::encode).collect(),
            orphaned: self.orphaned,
            pending_changes: self
                .pending_changes
                .iter()
                .map(|(block, change)| PendingChangeRecord {
                    block: *block,
                    change: hex::encode(change.encode_to_vec()),
                })
                .collect(),
            to_republish: self
                .to_republish
                .iter()
                .map(|batch| hex::encode(batch.encode_to_vec()))
                .collect(),
            checkpoint: self.checkpoint.clone(),
        };
        let bytes = serde_json::to_vec(&zone)?;
        if bytes != self.written_zone {
            write_sized(&dir.join(ZONE_FILE), &bytes)?;
            self.written_zone = bytes;
        }
        let stored: Vec<StoredBlock> = self.lez.view().iter().map(Block::to_stored).collect();
        let bytes = serde_json::to_vec(&stored)?;
        if bytes != self.written_lez_view {
            write_sized(&dir.join(LEZ_VIEW_FILE), &bytes)?;
            self.written_lez_view = bytes;
        }
        self.lez.persist()
    }

    /// `final.jsonl`: the new entries appended, or the file written whole when an entry it
    /// holds changed (the judge decided one) or a torn line was dropped on the way in.
    fn save_final(&mut self, dir: &Path) -> Result<()> {
        let final_entries = self.zone.final_entries();
        let path = dir.join(FINAL_FILE);
        if self.final_rewrite || final_entries.len() < self.final_written {
            let mut bytes = vec![];
            for entry in final_entries {
                serde_json::to_writer(&mut bytes, entry)?;
                bytes.push(b'\n');
            }
            write_sized(&path, &bytes)?;
            self.final_rewrite = false;
        } else if final_entries.len() > self.final_written {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
                .with_context(|| format!("open {}", path.display()))?;
            for entry in &final_entries[self.final_written..] {
                let mut line = serde_json::to_vec(entry)?;
                line.push(b'\n');
                file.write_all(&line)
                    .with_context(|| format!("append to {}", path.display()))?;
            }
        }
        self.final_written = final_entries.len();
        Ok(())
    }

    /// The size of every state file, for the start-up report.
    pub fn state_sizes(&self) -> Vec<(&'static str, u64)> {
        let Some(dir) = &self.state_dir else {
            return vec![];
        };
        [READER_FILE, ZONE_FILE, FINAL_FILE, LEZ_VIEW_FILE]
            .into_iter()
            .map(|name| (name, fs::metadata(dir.join(name)).map_or(0, |m| m.len())))
            .collect()
    }

    pub fn close(&mut self) -> Result<()> {
        self.lez.close()
    }

    pub fn checkpoint(&self) -> Option<&Value> {
        self.checkpoint.as_ref()
    }

    /// The Zone SDK checkpoint on the Zone channel, kept without its `channel_notes`: the
    /// notes the channel owns on the ledger, which only a withdraw spends and this node never
    /// does; the SDK restores from a checkpoint without them (and the set can be large).
    pub fn set_checkpoint(&mut self, mut checkpoint: Value) {
        if let Some(fields) = checkpoint.as_object_mut() {
            fields.insert("channel_notes".into(), json!([]));
        }
        self.checkpoint = Some(checkpoint);
    }

    /// The tree at the newest adopted LEZ block: what the API serves and the sequencer batches.
    pub fn head(&self) -> &Reader {
        self.lez.head()
    }

    pub fn anchor(&self) -> Option<ZoneAnchorRecord> {
        self.anchor
    }

    /// The LEZ block the head tip covers to: the block before the first while no batch does.
    pub fn covered_to(&self) -> u64 {
        self.zone
            .head_tip()
            .map_or(self.first_lez_block_id.saturating_sub(1), |tip| {
                tip.to_lez_block_id
            })
    }

    fn add_pending(&mut self, block_id: u64, changes: Vec<Change>) {
        if block_id > self.covered_to() {
            self.pending_changes
                .extend(changes.into_iter().map(|change| (block_id, change)));
        }
    }

    /// A LEZ block adopted on Bedrock (the SDK's `Extension` on the LEZ channel): onto the
    /// head; its changes after the head tip become pending. `Ok(Some(expected))` when blocks
    /// are missing before it (`expected..block_id`), to be filled first; `Err` when it does
    /// not chain for another reason.
    pub fn apply_lez_adopted(&mut self, block: &Block) -> Result<Option<u64>> {
        match self.lez.apply_adopted(block)? {
            Adopted::Applied(changes) => {
                self.add_pending(block.block_id(), changes);
                Ok(None)
            }
            Adopted::AlreadyApplied => Ok(None),
            Adopted::DoesNotChain(BlockIngestError::UnexpectedBlockId { expected, got })
                if got > expected =>
            {
                Ok(Some(expected))
            }
            Adopted::DoesNotChain(e) => Err(anyhow::anyhow!(
                "adopted LEZ block {} does not chain on the head: {e}",
                block.block_id()
            )),
        }
    }

    /// A Bedrock reorg on the LEZ channel (the SDK's `Conflict`): the head is the finalized
    /// tree again, the pending changes of the adopted blocks go with it; the caller adopts the
    /// canonical lineage again.
    pub fn apply_lez_conflict(&mut self) -> Result<()> {
        let final_block_id = self.lez.final_reader().lez_block_id;
        self.pending_changes
            .retain(|(block, _)| *block <= final_block_id);
        self.lez.reset_to_final()
    }

    /// A LEZ block Bedrock finalized (the SDK's `finalized` on the LEZ channel, or the RPC
    /// filling a gap under a finalized block): onto the finalized tree. `Ok(Some(expected))`
    /// when blocks are missing before it (`expected..block_id`), to be filled first.
    pub fn apply_lez_finalized(&mut self, block: &Block) -> Result<Option<u64>> {
        match self.lez.apply_finalized(block)? {
            Finalized::Applied {
                changes,
                new_to_head,
                view_dropped,
            } => {
                if view_dropped {
                    // The adopted blocks from this id on left the head: so do their changes.
                    self.pending_changes
                        .retain(|(held, _)| *held < block.block_id());
                }
                if new_to_head {
                    self.add_pending(block.block_id(), changes);
                }
                Ok(None)
            }
            Finalized::AlreadyApplied => Ok(None),
            Finalized::Gap { expected } => Ok(Some(expected)),
        }
    }

    /// The applied batches whose root is not what the tree derived from LEZ says at their
    /// `to_lez_block_id`: statements LEZ has since contradicted, not corrected yet (a parked or
    /// superseded batch is judged already). An indexer parks on them once LEZ finality reaches
    /// them; the sequencer answers with a correction.
    pub fn contradicted(&self) -> Vec<Contradicted> {
        let superseded = self.zone.superseded();
        let final_batches = self
            .zone
            .final_entries()
            .iter()
            .filter(|f| f.error.is_none())
            .filter_map(|f| Some((f.msg, f.batch()?)));
        let view_batches = self
            .zone
            .view_outcomes()
            .filter(|(_, outcome)| outcome.is_ok())
            .filter_map(|(e, _)| Some((e.msg.into(), e.batch.clone()?)));
        final_batches
            .chain(view_batches)
            .filter(|(msg, _)| !superseded.contains(msg))
            .filter_map(|(msg, batch)| {
                let claimed = Tip::from(&batch).root;
                let derived = self.head().root_at(batch.to_lez_block_id)?;
                (derived != claimed).then_some(Contradicted {
                    msg,
                    to_lez_block_id: batch.to_lez_block_id,
                    claimed,
                    derived,
                })
            })
            .collect()
    }

    /// The correction the channel needs, if any: the head tip's root is not the tree's root at
    /// its block (a Bedrock reorg replaced a LEZ block a batch covered), so a batch from the
    /// registry's first block restates the tree (every leaf set, on the empty tree) up to the
    /// head. `None` while the channel's statements hold.
    pub fn correction_batch(&self) -> Option<Batch> {
        let tip = self.zone.head_tip()?;
        if self.head().root_at(tip.to_lez_block_id)? == tip.root {
            return None;
        }
        let head = self.head();
        Some(Batch {
            from_lez_block_id: self.first_lez_block_id,
            to_lez_block_id: head.lez_block_id,
            changes: head
                .active_leaves()
                .iter()
                .map(|(_, m)| Change::insert(m.leaf_index, m.rate_commitment.to_vec()))
                .collect(),
            root_after: head.root().to_vec(),
        })
    }

    /// The next batch: every LEZ block after the head tip up to the head, with the pending
    /// changes and the root after them; `None` when the head is already covered. When the
    /// changes would not fit one inscription, the batch stops at the last LEZ block whose
    /// changes still fit (whole blocks, at least one) and the next batch continues from there.
    pub fn next_batch(&self) -> Option<Batch> {
        self.next_batch_within(MAX_INSCRIPTION_BYTES)
    }

    /// `next_batch` with the inscription size given.
    fn next_batch_within(&self, max_bytes: usize) -> Option<Batch> {
        let covered_to = self.covered_to();
        let head = self.head();
        if head.lez_block_id <= covered_to {
            return None;
        }
        let mut batch = Batch {
            from_lez_block_id: covered_to + 1,
            to_lez_block_id: head.lez_block_id,
            changes: vec![],
            root_after: head.root().to_vec(),
        };
        for block in self.pending_changes.chunk_by(|(a, _), (b, _)| a == b) {
            let block_id = block[0].0;
            let mut longer = batch.clone();
            longer
                .changes
                .extend(block.iter().map(|(_, change)| change.clone()));
            if longer.encoded_len() > max_bytes && !batch.changes.is_empty() {
                // Stop before this block: the batch covers up to the block before it.
                batch.to_lez_block_id = block_id - 1;
                batch.root_after = head.root_at(block_id - 1).unwrap_or(*EMPTY_ROOT).to_vec();
                return Some(batch);
            }
            batch = longer;
        }
        Some(batch)
    }

    /// Whether `batch` would apply on the head of the channel: checked before publishing, so
    /// a batch this node would park never reaches Bedrock.
    pub fn would_apply(&self, batch: &Batch) -> Result<(), BatchIngestError> {
        let judge = self.indexer.then_some(&self.lez as &dyn Judge);
        self.zone.would_apply(batch, judge)
    }

    /// A batch of ours handed to the SDK: into the view on the pin it was built on.
    pub fn record_publish(&mut self, msg: MsgId, parent: MsgId, batch: &Batch) -> PublishRecord {
        let entry = ChannelEntry {
            msg,
            parent,
            batch: Some(batch.clone()),
        };
        let judge = self.indexer.then_some(&self.lez as &dyn Judge);
        let record = self.zone.record_publish(entry, judge);
        // A stale publish chained on a pin the view moved past: no Bedrock block will ever
        // hold it, so nothing waits for it.
        if matches!(record, PublishRecord::Extended | PublishRecord::Skipped) {
            self.own_pending.push(msg.into());
        }
        self.trim_pending();
        record
    }

    /// Drops the publishes of ours recorded in flight that the view does not hold (a stale
    /// publish of an earlier run, which no Bedrock block will ever hold). Returns how many.
    pub fn drop_pending_not_in_view(&mut self) -> usize {
        let held: HashSet<[u8; 32]> = self.zone.view().iter().map(|e| e.msg.into()).collect();
        let before = self.own_pending.len();
        self.own_pending.retain(|m| held.contains(m));
        before - self.own_pending.len()
    }

    /// Sets the Zone SDK checkpoint's `last_msg_id` to `tip`, the channel's tip on chain, when
    /// it differs: the SDK restores the channel's tip from the checkpoint alone (and from the
    /// pending publishes it holds), never from the node, so a checkpoint whose tip fell behind
    /// would make it publish on a parent that is no longer the tip. Returns whether it changed.
    pub fn align_checkpoint_tip(&mut self, tip: [u8; 32]) -> bool {
        let Some(fields) = self.checkpoint.as_mut().and_then(Value::as_object_mut) else {
            return false;
        };
        let wanted = serde_json::to_value(MsgId::from(tip)).expect("a message id serializes");
        if fields.get("last_msg_id") == Some(&wanted) {
            return false;
        }
        fields.insert("last_msg_id".into(), wanted);
        true
    }

    /// Entries that entered the view (`ChannelUpdate::Extension`).
    pub fn apply_zone_extension(&mut self, adopted: Vec<ChannelEntry>) {
        let judge = self.indexer.then_some(&self.lez as &dyn Judge);
        self.zone.apply_extension(adopted, judge);
        self.trim_pending();
    }

    /// The channel's tip on chain is one of our entries and the view holds entries after it:
    /// they are not on chain (a reorg while this node was down, which the Zone SDK never
    /// reported). Treated as orphaned: out of the view, their batches queued to publish again
    /// in order, the Zone SDK checkpoint moved back to the tip (it would chain on them
    /// otherwise). Returns how many left the view.
    pub fn reconcile_with_chain(&mut self, tip: [u8; 32]) -> usize {
        let canonical: Vec<ChannelEntry> = match self
            .zone
            .view()
            .iter()
            .position(|e| <[u8; 32]>::from(e.msg) == tip)
        {
            Some(at) => self.zone.view()[..=at].to_vec(),
            None => vec![],
        };
        self.mark_adopted_up_to(tip);
        let orphaned = self.apply_zone_conflict(canonical);
        self.to_republish
            .extend(orphaned.iter().filter_map(|e| e.batch.clone()));
        if let Some(fields) = self.checkpoint.as_mut().and_then(Value::as_object_mut) {
            fields.insert(
                "last_msg_id".into(),
                serde_json::to_value(MsgId::from(tip)).expect("a message id serializes"),
            );
            fields.insert("pending_txs".into(), json!([]));
        }
        orphaned.len()
    }

    /// The whole view at the new tip (`ChannelUpdate::Conflict`); returns the entries the reorg
    /// orphaned: the sequencer's own, to publish again (an indexer publishes nothing and counts
    /// nothing as orphaned).
    pub fn apply_zone_conflict(&mut self, canonical: Vec<ChannelEntry>) -> Vec<ChannelEntry> {
        let judge = self.indexer.then_some(&self.lez as &dyn Judge);
        let orphaned = self.zone.apply_conflict(canonical, judge);
        if !self.indexer {
            self.orphaned += orphaned.len() as u64;
        }
        for entry in &orphaned {
            let msg: [u8; 32] = entry.msg.into();
            self.own_pending.retain(|m| *m != msg);
        }
        self.trim_pending();
        orphaned
    }

    /// A finalized inscription of the channel (the SDK stream or the finality scan). A publish
    /// of ours the finalized lineage does not hold leaves the view: Bedrock orphaned it without
    /// a `Conflict`, so it is treated as one (counted, its batch queued to publish again) and
    /// nothing waits for it any more.
    pub fn apply_zone_finalized(
        &mut self,
        entry: ChannelEntry,
        l1_slot: u64,
    ) -> Option<AcceptOutcome> {
        let judge = self.indexer.then_some(&self.lez as &dyn Judge);
        let ours: Vec<(MsgId, Option<Batch>)> = self
            .zone
            .view()
            .iter()
            .filter(|e| self.own_pending.contains(&<[u8; 32]>::from(e.msg)))
            .map(|e| (e.msg, e.batch.clone()))
            .collect();
        let outcome = self.zone.apply_finalized(entry, l1_slot, judge);
        let held: HashSet<[u8; 32]> = self.zone.view().iter().map(|e| e.msg.into()).collect();
        for (msg, batch) in ours {
            let msg: [u8; 32] = msg.into();
            if held.contains(&msg) || self.zone.final_entries().iter().any(|f| f.msg == msg) {
                continue;
            }
            self.orphaned += 1;
            if let Some(batch) = batch {
                self.to_republish.push(batch);
            }
        }
        self.own_pending.retain(|m| held.contains(m));
        let tip = self.zone.final_tip();
        self.anchor = Some(ZoneAnchorRecord {
            slot: l1_slot.max(self.anchor.map_or(0, |a| a.slot)),
            to_lez_block_id: tip.map_or(self.first_lez_block_id.saturating_sub(1), |t| {
                t.to_lez_block_id
            }),
            root: tip.map_or(*EMPTY_ROOT, |t| t.root),
        });
        self.trim_pending();
        outcome
    }

    /// The channel tip is one of our batches: it and every pending publish before it are in a
    /// Bedrock block. Returns the ones that just landed.
    pub fn mark_adopted_up_to(&mut self, tip: [u8; 32]) -> Vec<[u8; 32]> {
        let Some(at) = self.own_pending.iter().position(|m| *m == tip) else {
            return vec![];
        };
        self.own_pending.drain(..=at).collect()
    }

    /// Our newest publish not seen in a Bedrock block yet.
    pub fn in_flight(&self) -> Option<[u8; 32]> {
        self.own_pending.last().copied()
    }

    /// How many publishes of ours a Bedrock reorg orphaned (published again each time).
    pub fn orphaned_count(&self) -> u64 {
        self.orphaned
    }

    /// An orphaned batch whose publish again failed: the tick publishes it before any new
    /// batch.
    pub fn queue_republish(&mut self, batch: Batch) {
        self.to_republish.push(batch);
    }

    /// The oldest orphaned batch still to publish again, if any.
    pub fn next_republish(&self) -> Option<Batch> {
        self.to_republish.first().cloned()
    }

    /// The oldest orphaned batch is published: out of the queue.
    pub fn republished(&mut self) {
        if !self.to_republish.is_empty() {
            self.to_republish.remove(0);
        }
    }

    /// Whether this node judges the batches instead of publishing them.
    pub fn is_indexer(&self) -> bool {
        self.indexer
    }

    /// The finality scan read the finalized L1 blocks up to `slot`: the anchor moves there,
    /// so the next scan starts after it.
    pub fn scanned_to(&mut self, slot: u64) {
        if let Some(anchor) = &mut self.anchor {
            anchor.slot = anchor.slot.max(slot);
        }
    }

    /// An anchor for a channel with no finalized entry yet, at the slot the Zone SDK starts
    /// from: the finality scan covers everything after it.
    pub fn seed_anchor(&mut self, slot: u64) {
        if self.anchor.is_none() {
            self.anchor = Some(ZoneAnchorRecord {
                slot,
                to_lez_block_id: self.first_lez_block_id.saturating_sub(1),
                root: *EMPTY_ROOT,
            });
        }
    }

    /// The indexer folds again what the judge could not decide: LEZ read further. A decision
    /// changes finalized entries already on disk, so `final.jsonl` is written whole again.
    pub fn retry_folds(&mut self) -> usize {
        let decided = self.zone.retry_folds(Some(&self.lez));
        if decided > 0 {
            self.final_rewrite = true;
        }
        self.trim_pending();
        decided
    }

    fn trim_pending(&mut self) {
        let to = self.covered_to();
        self.pending_changes.retain(|(block, _)| *block > to);
    }

    /// Whether `msg` is a publish of ours no Bedrock block holds yet.
    pub fn is_pending(&self, msg: &[u8; 32]) -> bool {
        self.own_pending.contains(msg)
    }

    /// The newest block the LEZ Sequencer holds, as last asked; `None` before the first answer.
    pub fn sequencer_block_id(&self) -> Option<u64> {
        self.sequencer_block_id
    }

    /// The first LEZ block the preview lacks: after the newest held, else after the head.
    pub fn preview_next(&self) -> u64 {
        self.preview
            .last()
            .map_or(self.head().lez_block_id, Block::block_id)
            + 1
    }

    /// Blocks the LEZ Sequencer produced beyond the head (ascending, continuing the preview),
    /// and the newest block it holds. Blocks the head has reached are dropped.
    pub fn extend_preview(&mut self, blocks: Vec<Block>, sequencer_block_id: u64) {
        self.preview.extend(blocks);
        self.sequencer_block_id = Some(sequencer_block_id);
        self.trim_preview();
    }

    /// Drop the preview blocks the head has reached (adopted on Bedrock, or passed).
    pub fn trim_preview(&mut self) {
        let head = self.head().lez_block_id;
        self.preview.retain(|block| block.block_id() > head);
    }

    /// The newest preview event about `id_commitment`: `(lez_block_id, event)`.
    pub fn preview_for_id(&self, id_commitment: &[u8; 32]) -> Option<(u64, &ProgramEvent)> {
        self.preview_find(|event| event_id_commitment(event) == id_commitment)
    }

    /// The newest preview `Registered` at `leaf_index`: `(lez_block_id, event)`.
    pub fn preview_for_leaf(&self, leaf_index: u64) -> Option<(u64, &ProgramEvent)> {
        self.preview_find(
            |event| matches!(event, RlnEvent::Registered(r) if r.leaf_index == leaf_index),
        )
    }

    fn preview_find(&self, keep: impl Fn(&RlnEvent) -> bool) -> Option<(u64, &ProgramEvent)> {
        self.preview.iter().rev().find_map(|block| {
            block
                .events
                .iter()
                .rev()
                .find(|event| keep(&event.event))
                .map(|event| (block.block_id(), event))
        })
    }

    /// Every entry of the channel in lineage order, final tier first then the view, with where
    /// it stands on Bedrock.
    pub fn lineage(&self) -> Vec<([u8; 32], BedrockStatus)> {
        let final_entries = self
            .zone
            .final_entries()
            .iter()
            .map(|f| (f.msg, BedrockStatus::Finalized));
        let view = self.zone.view().iter().map(|e| {
            let msg: [u8; 32] = e.msg.into();
            let status = if self.is_pending(&msg) {
                BedrockStatus::Pending
            } else {
                BedrockStatus::Adopted
            };
            (msg, status)
        });
        final_entries.chain(view).collect()
    }
}

/// The identity commitment an event is about.
pub fn event_id_commitment(event: &RlnEvent) -> &[u8; 32] {
    match event {
        RlnEvent::Registered(e) => &e.id_commitment,
        RlnEvent::Extended(e) => &e.id_commitment,
        RlnEvent::Slashed(e) => &e.id_commitment,
        RlnEvent::Erased(e) => &e.id_commitment,
    }
}

/// `write_atomic`, and a warning (at every write) when one state file is past
/// `STATE_FILE_WARN_BYTES`.
fn write_sized(path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() > STATE_FILE_WARN_BYTES {
        warn!(
            "{} is {} KB: the state should stay small, look at what grows",
            path.display(),
            bytes.len() / 1024
        );
    }
    write_atomic(path, bytes)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        lez::{ProgramEvent, Registered, RlnEvent},
        zone_chain::ReplayTree,
    };

    fn registered(n: u8, leaf_index: u64) -> ProgramEvent {
        let mut id_commitment = [0u8; 32];
        id_commitment[0] = n;
        ProgramEvent::from(RlnEvent::Registered(Registered {
            tree_id: [0; 32],
            id_commitment,
            rate_limit: 100,
            expiry_timestamp_ms: 9_000,
            leaf_index,
        }))
    }

    fn block(block_id: u64, events: Vec<ProgramEvent>) -> Block {
        Block {
            header: crate::lez::BlockHeader {
                block_id,
                prev_block_hash: common::HashType([0; 32]),
                hash: common::HashType([0; 32]),
                timestamp: 1_000,
            },
            events,
            private_transaction_count: 0,
        }
    }

    fn msg(n: u8) -> MsgId {
        MsgId::from([n; 32])
    }

    #[test]
    fn test_sequencer_builds_publishes_and_sees_its_batches_land() {
        let mut registry = Registry::open_temporary(1, false).unwrap();
        registry
            .apply_lez_adopted(&block(1, vec![registered(1, 0)]))
            .unwrap();
        registry.apply_lez_adopted(&block(2, vec![])).unwrap();
        assert_eq!(registry.pending_changes.len(), 1);
        let batch = registry.next_batch().unwrap();
        assert_eq!((batch.from_lez_block_id, batch.to_lez_block_id), (1, 2));
        assert_eq!(batch.changes.len(), 1);
        assert_eq!(
            registry.record_publish(msg(1), MsgId::root(), &batch),
            PublishRecord::Extended
        );
        // Published: the changes are no longer pending, the head is covered, one in flight.
        assert!(registry.pending_changes.is_empty());
        assert_eq!(registry.covered_to(), 2);
        assert_eq!(registry.next_batch(), None);
        assert_eq!(registry.in_flight(), Some([1; 32]));
        assert_eq!(registry.lineage(), vec![([1; 32], BedrockStatus::Pending)]);
        // The channel tip is our batch: adopted.
        assert_eq!(registry.mark_adopted_up_to([1; 32]), vec![[1; 32]]);
        assert_eq!(registry.in_flight(), None);
        assert_eq!(registry.lineage(), vec![([1; 32], BedrockStatus::Adopted)]);
        // Finality: into the final tier, the anchor moves.
        let entry = ChannelEntry {
            msg: msg(1),
            parent: MsgId::root(),
            batch: Some(batch.clone()),
        };
        assert!(matches!(
            registry.apply_zone_finalized(entry, 500),
            Some(AcceptOutcome::Applied(_))
        ));
        assert_eq!(
            registry.lineage(),
            vec![([1; 32], BedrockStatus::Finalized)]
        );
        assert_eq!(registry.zone.final_entries()[0].l1_slot, 500);
        assert_eq!(registry.anchor().unwrap().slot, 500);
        assert_eq!(registry.anchor().unwrap().to_lez_block_id, 2);
        // The finality scan moves the anchor's slot forward only.
        registry.scanned_to(900);
        assert_eq!(registry.anchor().unwrap().slot, 900);
        registry.scanned_to(100);
        assert_eq!(registry.anchor().unwrap().slot, 900);
        assert_eq!(registry.anchor().unwrap().to_lez_block_id, 2);
        // A heartbeat carries the root on.
        registry.apply_lez_adopted(&block(3, vec![])).unwrap();
        let heartbeat = registry.next_batch().unwrap();
        assert!(heartbeat.changes.is_empty());
        registry.record_publish(msg(2), msg(1), &heartbeat);
        assert_eq!(registry.lineage().len(), 2);
        // Nothing LEZ contradicts; a batch claiming another root at its block does not even
        // apply (its changes replay to the tip's root): parked in the view, judged, so not
        // among the contradicted (an applied batch LEZ contradicts is the correction's case).
        assert!(registry.contradicted().is_empty());
        let mut liar = heartbeat.clone();
        liar.root_after = vec![9; 32];
        registry.apply_lez_adopted(&block(4, vec![])).unwrap();
        liar.from_lez_block_id = 4;
        liar.to_lez_block_id = 4;
        registry.apply_zone_extension(vec![ChannelEntry {
            msg: msg(3),
            parent: msg(2),
            batch: Some(liar),
        }]);
        assert!(matches!(
            registry.zone.view_outcomes().last().unwrap().1,
            Err(BatchIngestError::HashMismatch { .. })
        ));
        assert!(registry.contradicted().is_empty());
        assert_eq!(registry.covered_to(), 3);
    }

    #[test]
    fn test_reopen_keeps_the_channel_state_and_the_pending_changes() {
        let dir = std::env::temp_dir().join(format!("zone-registry-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let batch;
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            // Finalized blocks persist with the store; adopted ones persist as the view and
            // are folded again on the next start (the SDK does not echo them).
            assert_eq!(
                registry
                    .apply_lez_finalized(&block(1, vec![registered(1, 0)]))
                    .unwrap(),
                None
            );
            batch = registry.next_batch().unwrap();
            registry.record_publish(msg(1), MsgId::root(), &batch);
            assert_eq!(
                registry
                    .apply_lez_finalized(&block(2, vec![registered(2, 1)]))
                    .unwrap(),
                None
            );
            assert_eq!(registry.pending_changes.len(), 1);
            assert_eq!(
                registry
                    .apply_lez_adopted(&block(3, vec![registered(3, 2)]))
                    .unwrap(),
                None
            );
            // Adopted block 5 with 4 missing: the caller fills the gap first.
            assert_eq!(
                registry.apply_lez_adopted(&block(5, vec![])).unwrap(),
                Some(4)
            );
            assert_eq!(registry.head().lez_block_id, 3);
            assert_eq!(registry.pending_changes.len(), 2);
            registry.set_checkpoint(
                json!({"lib": 7, "channel_notes": [{"note_id": "aa", "value": 5}]}),
            );
            registry.save().unwrap();
            registry.close().unwrap();
        }
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            assert_eq!(registry.zone.view().len(), 1);
            assert_eq!(registry.in_flight(), Some([1; 32]));
            assert_eq!(registry.covered_to(), 1);
            // The finalized tree and the adopted block 3 over it, with the changes of both.
            assert_eq!(registry.lez.final_reader().lez_block_id, 2);
            assert_eq!(registry.head().lez_block_id, 3);
            assert_eq!(registry.lez.view().len(), 1);
            assert_eq!(registry.pending_changes.len(), 2);
            // The checkpoint comes back without the channel notes, which this node never spends.
            assert_eq!(
                registry.checkpoint(),
                Some(&json!({"lib": 7, "channel_notes": []}))
            );
            // The next batch continues from the view's tip up to the head.
            let next = registry.next_batch().unwrap();
            assert_eq!((next.from_lez_block_id, next.to_lez_block_id), (2, 3));
            // A Bedrock reorg on the LEZ channel: back to the finalized tree, the adopted
            // block's changes leave the pending ones until it is adopted again.
            registry.apply_lez_conflict().unwrap();
            assert_eq!(registry.head().lez_block_id, 2);
            assert_eq!(registry.pending_changes.len(), 1);
            registry
                .apply_lez_adopted(&block(3, vec![registered(3, 2)]))
                .unwrap();
            assert_eq!(registry.pending_changes.len(), 2);
            // Finalize the first: reopen keeps the final tier.
            registry.apply_zone_finalized(
                ChannelEntry {
                    msg: msg(1),
                    parent: MsgId::root(),
                    batch: Some(batch.clone()),
                },
                100,
            );
            registry.save().unwrap();
            registry.close().unwrap();
        }
        {
            let registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            assert_eq!(registry.zone.final_entries().len(), 1);
            assert_eq!(registry.zone.final_tip().unwrap().to_lez_block_id, 1);
            assert_eq!(registry.anchor().unwrap().slot, 100);
            assert!(registry.zone.view().is_empty());
            // The state on disk: four small files, one line per finalized entry.
            for name in [READER_FILE, ZONE_FILE, FINAL_FILE, LEZ_VIEW_FILE] {
                assert!(dir.join(name).exists(), "{name}");
            }
            let lines = std::fs::read_to_string(dir.join(FINAL_FILE)).unwrap();
            assert_eq!(lines.lines().count(), 1);
            assert!(
                registry
                    .state_sizes()
                    .iter()
                    .all(|(_, size)| *size < 10_000)
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_indexer_judges_batches_against_its_own_tree() {
        let mut registry = Registry::open_temporary(1, true).unwrap();
        // The indexer judges over the finalized tree: the blocks arrive finalized here.
        registry
            .apply_lez_finalized(&block(1, vec![registered(1, 0), registered(2, 1)]))
            .unwrap();
        let root_1 = registry.head().root();
        let changes: Vec<Change> = registry
            .pending_changes
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        let honest = Batch {
            from_lez_block_id: 1,
            to_lez_block_id: 1,
            changes: changes.clone(),
            root_after: root_1.to_vec(),
        };
        // Adopted on the channel: applied, the pending changes are the head's lead no more.
        registry.apply_zone_extension(vec![ChannelEntry {
            msg: msg(1),
            parent: MsgId::root(),
            batch: Some(honest.clone()),
        }]);
        assert!(registry.zone.view_outcomes().next().unwrap().1.is_ok());
        assert_eq!(registry.lineage()[0].1, BedrockStatus::Adopted);
        assert!(registry.pending_changes.is_empty());
        // A batch for blocks the indexer has not read yet: it replays (a heartbeat keeps the
        // root), so only the judge is missing; retryable, decided once LEZ is read.
        let early = Batch {
            from_lez_block_id: 2,
            to_lez_block_id: 3,
            changes: vec![],
            root_after: root_1.to_vec(),
        };
        registry.apply_zone_extension(vec![ChannelEntry {
            msg: msg(2),
            parent: msg(1),
            batch: Some(early),
        }]);
        let outcome =
            |registry: &Registry, i: usize| registry.zone.view_outcomes().nth(i).unwrap().1.clone();
        assert!(
            outcome(&registry, 1)
                .as_ref()
                .is_err_and(BatchIngestError::is_retryable)
        );
        // Adopted, not finalized: the judge waits.
        registry.apply_lez_adopted(&block(2, vec![])).unwrap();
        registry
            .apply_lez_adopted(&block(3, vec![registered(3, 2)]))
            .unwrap();
        assert_eq!(registry.retry_folds(), 0);
        registry.apply_lez_finalized(&block(2, vec![])).unwrap();
        registry
            .apply_lez_finalized(&block(3, vec![registered(3, 2)]))
            .unwrap();
        assert_eq!(registry.retry_folds(), 1);
        // Decided: block 3 registered someone, the tree derived from LEZ moved on.
        assert!(matches!(
            outcome(&registry, 1),
            Err(BatchIngestError::DerivationMismatch { .. })
        ));
        // The head tip stays at the last applied batch.
        assert_eq!(registry.covered_to(), 1);
        // A conflict replaces the parked entry by an honest one for the same blocks.
        let root_3 = registry.head().root();
        let changes_3: Vec<Change> = registry
            .pending_changes
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        let honest_3 = Batch {
            from_lez_block_id: 2,
            to_lez_block_id: 3,
            changes: changes_3,
            root_after: root_3.to_vec(),
        };
        let dropped = registry.apply_zone_conflict(vec![
            ChannelEntry {
                msg: msg(1),
                parent: MsgId::root(),
                batch: Some(honest),
            },
            ChannelEntry {
                msg: msg(3),
                parent: msg(1),
                batch: Some(honest_3),
            },
        ]);
        assert_eq!(dropped.len(), 1);
        // The dropped entry was the sequencer's: an indexer counts nothing as orphaned.
        assert_eq!(registry.orphaned_count(), 0);
        assert!(outcome(&registry, 1).is_ok());
        assert_eq!(registry.covered_to(), 3);
        assert_eq!(registry.zone.head_root(), root_3);
        // A sequencer counts its own.
        let mut sequencer = Registry::open_temporary(1, false).unwrap();
        sequencer.apply_lez_adopted(&block(1, vec![])).unwrap();
        let own = sequencer.next_batch().unwrap();
        sequencer.record_publish(msg(1), MsgId::root(), &own);
        assert_eq!(sequencer.apply_zone_conflict(vec![]).len(), 1);
        assert_eq!(sequencer.orphaned_count(), 1);
    }

    /// Block `block_id` with hash `hash` chaining on hash `prev`.
    fn chained(block_id: u64, prev: u8, hash: u8, events: Vec<ProgramEvent>) -> Block {
        let mut b = block(block_id, events);
        b.header.prev_block_hash = common::HashType([prev; 32]);
        b.header.hash = common::HashType([hash; 32]);
        b
    }

    /// Bedrock finalizes a LEZ block with another hash than the adopted one at its id, with
    /// no `Conflict` first: the view goes, and so do the pending changes derived from it; the
    /// next batch carries the finalized block's changes only.
    #[test]
    fn test_finality_dropping_the_view_drops_its_pending_changes() {
        let mut registry = Registry::open_temporary(1, false).unwrap();
        let b1 = chained(1, 0, 1, vec![]);
        registry.apply_lez_adopted(&b1).unwrap();
        registry.apply_lez_finalized(&b1).unwrap();
        let first = registry.next_batch().unwrap();
        registry.record_publish(msg(1), MsgId::root(), &first);
        // Adopted: block 2 registers leaf 0, block 3 registers leaf 1.
        registry
            .apply_lez_adopted(&chained(2, 1, 2, vec![registered(1, 0)]))
            .unwrap();
        registry
            .apply_lez_adopted(&chained(3, 2, 3, vec![registered(2, 1)]))
            .unwrap();
        assert_eq!(registry.pending_changes.len(), 2);
        // Finalized: another block 2, registering leaf 0 for someone else.
        registry
            .apply_lez_finalized(&chained(2, 1, 9, vec![registered(3, 0)]))
            .unwrap();
        assert_eq!(registry.head().lez_block_id, 2);
        assert_eq!(registry.lez.view().len(), 0);
        assert_eq!(registry.pending_changes.len(), 1);
        assert_eq!(registry.pending_changes[0].0, 2);
        let next = registry.next_batch().unwrap();
        assert_eq!((next.from_lez_block_id, next.to_lez_block_id), (2, 2));
        assert_eq!(next.changes.len(), 1);
        registry.would_apply(&next).unwrap();
        // Its changes are the finalized block's: the leaf is the third commitment's.
        assert_eq!(registry.head().member_at(0).map(|(id, _)| id[0]), Some(3));
    }

    #[test]
    fn test_a_store_of_another_first_block_is_refused() {
        let dir = std::env::temp_dir().join(format!("zone-registry-first-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut registry = Registry::open(&dir, [0; 32], 10, false, false).unwrap();
            registry.save().unwrap();
            registry.close().unwrap();
        }
        assert!(Registry::open(&dir, [0; 32], 11, false, false).is_err());
        assert!(Registry::open(&dir, [0; 32], 10, false, false).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The preview holds the LEZ blocks produced beyond the head, answers by commitment and
    /// by leaf, and lets go of a block once the head reaches it.
    #[test]
    fn test_the_preview_follows_the_head() {
        let mut registry = Registry::open_temporary(1, false).unwrap();
        registry.apply_lez_adopted(&block(1, vec![])).unwrap();
        assert_eq!(registry.preview_next(), 2);
        assert_eq!(registry.sequencer_block_id(), None);
        let mut slashed = ProgramEvent::from(RlnEvent::Slashed(crate::lez::Slashed {
            tree_id: [0; 32],
            id_commitment: {
                let mut id = [0u8; 32];
                id[0] = 1;
                id
            },
        }));
        slashed.tx_hash = [9; 32];
        registry.extend_preview(
            vec![
                block(2, vec![registered(1, 0)]),
                block(3, vec![]),
                block(4, vec![slashed]),
            ],
            5,
        );
        assert_eq!(registry.sequencer_block_id(), Some(5));
        assert_eq!(registry.preview_next(), 5);
        let (at, event) = registry.preview_for_leaf(0).unwrap();
        assert_eq!(at, 2);
        assert!(matches!(event.event, RlnEvent::Registered(_)));
        // By commitment: the newest event wins.
        let id = {
            let mut id = [0u8; 32];
            id[0] = 1;
            id
        };
        let (at, event) = registry.preview_for_id(&id).unwrap();
        assert_eq!((at, event.tx_hash), (4, [9; 32]));
        assert!(registry.preview_for_leaf(3).is_none());
        // The head reaches block 2: that block leaves the preview, the later ones stay.
        registry
            .apply_lez_adopted(&block(2, vec![registered(1, 0)]))
            .unwrap();
        registry.trim_preview();
        assert_eq!(registry.preview_for_leaf(0), None);
        assert_eq!(registry.preview_for_id(&id).map(|(at, _)| at), Some(4));
        assert_eq!(registry.preview_next(), 5);
    }

    /// A reorg drops a published batch and publishing it again fails: its changes left
    /// `pending_changes` when it was first published, so the batch waits in a queue the tick
    /// drains before any new batch, across a restart.
    #[test]
    fn test_a_dropped_batch_whose_republish_failed_waits_for_the_tick() {
        let dir = std::env::temp_dir().join(format!(
            "zone-registry-republish-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let batch;
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            registry
                .apply_lez_finalized(&block(1, vec![registered(1, 0)]))
                .unwrap();
            batch = registry.next_batch().unwrap();
            registry.record_publish(msg(1), MsgId::root(), &batch);
            assert!(registry.pending_changes.is_empty());
            // The reorg drops it; the view is empty again and no new batch carries leaf 0.
            let dropped = registry.apply_zone_conflict(vec![]);
            assert_eq!(dropped.len(), 1);
            assert_eq!(registry.orphaned_count(), 1);
            assert!(registry.in_flight().is_none());
            assert!(registry.next_batch().unwrap().changes.is_empty());
            registry.queue_republish(batch.clone());
            registry.save().unwrap();
            registry.close().unwrap();
        }
        let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
        assert_eq!(registry.next_republish(), Some(batch.clone()));
        // Published again: the same payload applies on the head, the queue is empty.
        assert!(registry.would_apply(&batch).is_ok());
        assert_eq!(
            registry.record_publish(msg(2), MsgId::root(), &batch),
            PublishRecord::Extended
        );
        registry.republished();
        assert_eq!(registry.next_republish(), None);
        assert_eq!(registry.covered_to(), 1);
        registry.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A Bedrock reorg on the LEZ channel replaces an adopted block the sequencer already
    /// batched (the LEZ Sequencer produced it again with other contents): the batch on the
    /// channel claims a root the canonical lineage contradicts. The sequencer reports it and
    /// refuses to publish on top of it (`would_apply` fails on its own head), instead of
    /// publishing a root the channel replay cannot reach; the correction is RFC Q6.
    #[test]
    fn test_a_lez_reorg_contradicting_a_published_batch_is_reported_and_stops_the_batches() {
        let mut registry = Registry::open_temporary(1, false).unwrap();
        registry
            .apply_lez_adopted(&block(1, vec![registered(1, 0)]))
            .unwrap();
        registry
            .apply_lez_adopted(&block(2, vec![registered(2, 1)]))
            .unwrap();
        let batch = registry.next_batch().unwrap();
        assert_eq!(
            registry.record_publish(msg(1), MsgId::root(), &batch),
            PublishRecord::Extended
        );
        assert!(registry.contradicted().is_empty());
        // The reorg: block 1 stays, block 2 comes back with another registration.
        registry.apply_lez_conflict().unwrap();
        assert_eq!(registry.head().lez_block_id, 0);
        assert!(registry.pending_changes.is_empty());
        registry
            .apply_lez_adopted(&block(1, vec![registered(1, 0)]))
            .unwrap();
        registry
            .apply_lez_adopted(&block(2, vec![registered(3, 1)]))
            .unwrap();
        // Block 2 is covered by the batch, so its changes are not pending again; the batch's
        // root is not the tree's root at block 2 any more.
        assert!(registry.pending_changes.is_empty());
        let contradicted = registry.contradicted();
        assert_eq!(contradicted.len(), 1);
        assert_eq!(contradicted[0].to_lez_block_id, 2);
        assert_eq!(contradicted[0].claimed, Tip::from(&batch).root);
        assert_eq!(contradicted[0].derived, registry.head().root());
        assert!(registry.next_batch().is_none());
        // The next block: a batch from the replay's root cannot reach the tree's root.
        registry
            .apply_lez_adopted(&block(3, vec![registered(4, 2)]))
            .unwrap();
        let next = registry.next_batch().unwrap();
        assert_eq!((next.from_lez_block_id, next.to_lez_block_id), (3, 3));
        assert!(matches!(
            registry.would_apply(&next),
            Err(BatchIngestError::HashMismatch { .. })
        ));
        // The correction: from the contradicted batch's first block to the head, every leaf
        // set (0, 1, 2) on the empty tree. It applies, supersedes the wrong batch, and the
        // channel's statements hold again.
        let correction = registry.correction_batch().unwrap();
        assert_eq!(
            (correction.from_lez_block_id, correction.to_lez_block_id),
            (1, 3)
        );
        assert_eq!(correction.changes.len(), 3);
        assert_eq!(correction.root_after, registry.head().root().to_vec());
        assert!(registry.would_apply(&correction).is_ok());
        assert_eq!(
            registry.record_publish(msg(2), msg(1), &correction),
            PublishRecord::Extended
        );
        assert_eq!(
            registry.zone.superseded(),
            std::collections::HashSet::from([[1; 32]])
        );
        assert!(registry.contradicted().is_empty());
        assert!(registry.correction_batch().is_none());
        assert_eq!(registry.covered_to(), 3);
        assert!(registry.pending_changes.is_empty());
        assert!(registry.next_batch().is_none());
        // Life goes on: the next block batches as usual.
        registry
            .apply_lez_adopted(&block(4, vec![registered(5, 3)]))
            .unwrap();
        let next = registry.next_batch().unwrap();
        assert_eq!((next.from_lez_block_id, next.to_lez_block_id), (4, 4));
        assert!(registry.would_apply(&next).is_ok());
    }

    /// A Bedrock reorg drops two published batches at once: they leave the view in order and
    /// go back in order, each on the one before (`Stale` when not), and no new batch can stand
    /// in for them, since their changes left `pending_changes` when they were first published.
    #[test]
    fn test_two_dropped_batches_go_back_in_order() {
        let mut registry = Registry::open_temporary(1, false).unwrap();
        registry
            .apply_lez_adopted(&block(1, vec![registered(1, 0)]))
            .unwrap();
        let first = registry.next_batch().unwrap();
        registry.record_publish(msg(1), MsgId::root(), &first);
        registry
            .apply_lez_adopted(&block(2, vec![registered(2, 1)]))
            .unwrap();
        let second = registry.next_batch().unwrap();
        assert_eq!((second.from_lez_block_id, second.to_lez_block_id), (2, 2));
        registry.record_publish(msg(2), msg(1), &second);
        assert_eq!(registry.mark_adopted_up_to([2; 32]), vec![[1; 32], [2; 32]]);
        assert_eq!(registry.covered_to(), 2);
        // The reorg drops both.
        let dropped = registry.apply_zone_conflict(vec![]);
        assert_eq!(
            dropped.iter().map(|e| e.msg).collect::<Vec<_>>(),
            vec![msg(1), msg(2)]
        );
        assert_eq!(registry.orphaned_count(), 2);
        assert_eq!(registry.covered_to(), 0);
        assert!(registry.in_flight().is_none());
        // A new batch over the same span would carry no changes: it cannot replace them.
        let instead = registry.next_batch().unwrap();
        assert_eq!((instead.from_lez_block_id, instead.to_lez_block_id), (1, 2));
        assert!(instead.changes.is_empty());
        assert!(matches!(
            registry.would_apply(&instead),
            Err(BatchIngestError::HashMismatch { .. })
        ));
        // Published again in order, each on the one before.
        assert!(registry.would_apply(&first).is_ok());
        assert_eq!(
            registry.record_publish(msg(3), MsgId::root(), &first),
            PublishRecord::Extended
        );
        // The second on the root would be stale: the pin moved to the first.
        assert_eq!(
            registry.record_publish(msg(4), MsgId::root(), &second),
            PublishRecord::Stale
        );
        assert!(registry.would_apply(&second).is_ok());
        assert_eq!(
            registry.record_publish(msg(5), msg(3), &second),
            PublishRecord::Extended
        );
        assert_eq!(registry.covered_to(), 2);
        assert_eq!(registry.in_flight(), Some([5; 32]));
        assert_eq!(
            registry.lineage(),
            vec![
                ([3; 32], BedrockStatus::Pending),
                ([5; 32], BedrockStatus::Pending)
            ]
        );
        assert!(registry.contradicted().is_empty());
    }

    /// A fresh data dir for a test, removed first.
    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zone-registry-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn file(dir: &Path, name: &str) -> Vec<u8> {
        fs::read(dir.join(name)).unwrap()
    }

    fn entry(n: u8, parent: Option<u8>, batch: &Batch) -> ChannelEntry {
        ChannelEntry {
            msg: msg(n),
            parent: parent.map_or(MsgId::root(), msg),
            batch: Some(batch.clone()),
        }
    }

    /// `save` writes `zone.json` before `reader.json`: a crash between them leaves the reader
    /// a block behind, with the pending changes of that block in `zone.json` filtered out on
    /// the way in (`block > final`) and made again when the LIB fill finalizes the block.
    #[test]
    fn test_the_pending_changes_of_a_finalized_block_survive_a_crash_before_the_reader_saved() {
        let dir = dir("crash-reader");
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            registry
                .apply_lez_finalized(&block(1, vec![registered(1, 0)]))
                .unwrap();
            let first = registry.next_batch().unwrap();
            registry.record_publish(msg(1), MsgId::root(), &first);
            registry.save().unwrap();
            let reader_before = file(&dir, READER_FILE);
            registry
                .apply_lez_finalized(&block(2, vec![registered(2, 1)]))
                .unwrap();
            assert_eq!(registry.pending_changes.len(), 1);
            registry.save().unwrap();
            registry.close().unwrap();
            // The crash: zone.json is on disk, reader.json is the one from before.
            fs::write(dir.join(READER_FILE), reader_before).unwrap();
        }
        let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
        assert_eq!(registry.lez.final_reader().lez_block_id, 1);
        assert!(registry.pending_changes.is_empty());
        // The LIB fill finalizes block 2 again: its changes are pending again.
        registry
            .apply_lez_finalized(&block(2, vec![registered(2, 1)]))
            .unwrap();
        registry.apply_lez_adopted(&block(3, vec![])).unwrap();
        let next = registry.next_batch().unwrap();
        assert_eq!((next.from_lez_block_id, next.to_lez_block_id), (2, 3));
        assert_eq!(next.changes.len(), 1);
        assert!(registry.would_apply(&next).is_ok());
        registry.close().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    /// `save` appends `final.jsonl` before writing `zone.json`: a crash between them leaves a
    /// `zone.json` whose view and in-flight list still hold an entry the final tier has. The
    /// final tier wins: the entry leaves the view and the in-flight list.
    #[test]
    fn test_a_zone_json_behind_the_final_tier_defers_to_it() {
        let dir = dir("crash-zone");
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            registry
                .apply_lez_adopted(&block(1, vec![registered(1, 0)]))
                .unwrap();
            let first = registry.next_batch().unwrap();
            registry.record_publish(msg(1), MsgId::root(), &first);
            registry.apply_zone_finalized(entry(1, None, &first), 10);
            registry
                .apply_lez_adopted(&block(2, vec![registered(2, 1)]))
                .unwrap();
            let second = registry.next_batch().unwrap();
            registry.record_publish(msg(2), msg(1), &second);
            registry.save().unwrap();
            let zone_before = file(&dir, ZONE_FILE);
            registry.apply_zone_finalized(entry(2, Some(1), &second), 20);
            registry.save().unwrap();
            registry.close().unwrap();
            // The crash: final.jsonl holds entry 2, zone.json is the one from before.
            fs::write(dir.join(ZONE_FILE), zone_before).unwrap();
        }
        let registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
        assert_eq!(registry.zone.final_entries().len(), 2);
        assert_eq!(registry.zone.final_msg(), msg(2));
        assert!(registry.zone.view().is_empty());
        assert_eq!(registry.covered_to(), 2);
        assert!(registry.in_flight().is_none());
        assert!(registry.next_batch().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A torn last line of `final.jsonl` (a crash while appending) is dropped on the way in and
    /// the file is written whole at the next save, so nothing is appended onto the torn line.
    #[test]
    fn test_a_torn_final_jsonl_line_is_dropped_and_the_file_heals() {
        let dir = dir("torn");
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            registry
                .apply_lez_adopted(&block(1, vec![registered(1, 0)]))
                .unwrap();
            let first = registry.next_batch().unwrap();
            registry.record_publish(msg(1), MsgId::root(), &first);
            registry.apply_zone_finalized(entry(1, None, &first), 10);
            registry.save().unwrap();
            registry.close().unwrap();
        }
        let mut torn = file(&dir, FINAL_FILE);
        torn.extend_from_slice(b"{\"msg\":\"ab");
        fs::write(dir.join(FINAL_FILE), torn).unwrap();
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            assert_eq!(registry.zone.final_entries().len(), 1);
            registry
                .apply_lez_adopted(&block(2, vec![registered(2, 1)]))
                .unwrap();
            let second = registry.next_batch().unwrap();
            registry.record_publish(msg(2), msg(1), &second);
            registry.apply_zone_finalized(entry(2, Some(1), &second), 20);
            registry.save().unwrap();
            registry.close().unwrap();
        }
        let registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
        assert_eq!(registry.zone.final_entries().len(), 2);
        let lines = String::from_utf8(file(&dir, FINAL_FILE)).unwrap();
        assert_eq!(lines.lines().count(), 2);
        assert!(
            lines
                .lines()
                .all(|l| l.starts_with('{') && l.ends_with('}'))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The indexer's verdict on a finalized batch it had to wait for reaches the disk: the
    /// entry is written again without its error, so a restart replays it.
    #[test]
    fn test_a_judged_final_entry_is_written_again_on_disk() {
        let dir = dir("judged");
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, true).unwrap();
            registry
                .apply_lez_finalized(&block(1, vec![registered(1, 0)]))
                .unwrap();
            let first = registry.next_batch().unwrap();
            assert!(matches!(
                registry.apply_zone_finalized(entry(1, None, &first), 10),
                Some(AcceptOutcome::Applied(_))
            ));
            let heartbeat = Batch {
                from_lez_block_id: 2,
                to_lez_block_id: 3,
                changes: vec![],
                root_after: registry.head().root().to_vec(),
            };
            assert!(matches!(
                registry.apply_zone_finalized(entry(2, Some(1), &heartbeat), 20),
                Some(AcceptOutcome::RetryableFailure(_))
            ));
            registry.save().unwrap();
            registry.apply_lez_finalized(&block(2, vec![])).unwrap();
            registry.apply_lez_finalized(&block(3, vec![])).unwrap();
            assert_eq!(registry.retry_folds(), 1);
            registry.save().unwrap();
            registry.close().unwrap();
        }
        let registry = Registry::open(&dir, [0; 32], 1, false, true).unwrap();
        assert!(registry.zone.final_entries()[1].error.is_none());
        assert_eq!(registry.zone.final_tip().unwrap().to_lez_block_id, 3);
        assert!(registry.zone.stall().is_none());
        assert_eq!(
            String::from_utf8(file(&dir, FINAL_FILE))
                .unwrap()
                .lines()
                .count(),
            2
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A frozen final tier (a parked batch, one parked after it) and the correction that ends
    /// the stall come back the same from disk: the stall is derived from the entries, the
    /// correction replays on the empty tree, the roots exclude the parked and the superseded.
    #[test]
    fn test_reopen_restores_a_frozen_final_tier_and_its_correction() {
        let dir = dir("frozen");
        let (truth, correction) = {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, true).unwrap();
            registry
                .apply_lez_finalized(&block(1, vec![registered(1, 0)]))
                .unwrap();
            registry
                .apply_lez_finalized(&block(2, vec![registered(2, 1)]))
                .unwrap();
            let leaf_0 = registry.head().member_at(0).unwrap().1.rate_commitment;
            let honest = Batch {
                from_lez_block_id: 1,
                to_lez_block_id: 1,
                changes: vec![Change::insert(0, leaf_0.to_vec())],
                root_after: registry.head().root_at(1).unwrap().to_vec(),
            };
            assert!(matches!(
                registry.apply_zone_finalized(entry(1, None, &honest), 10),
                Some(AcceptOutcome::Applied(_))
            ));
            // A batch claiming another leaf 1 than LEZ gave: parked; the heartbeat after it too.
            let mut wrong_tree = ReplayTree::empty();
            wrong_tree.apply(&honest.changes).unwrap();
            wrong_tree.apply(&[Change::insert(1, vec![9; 32])]).unwrap();
            let wrong = Batch {
                from_lez_block_id: 2,
                to_lez_block_id: 2,
                changes: vec![Change::insert(1, vec![9; 32])],
                root_after: wrong_tree.root().to_vec(),
            };
            assert!(matches!(
                registry.apply_zone_finalized(entry(2, Some(1), &wrong), 20),
                Some(AcceptOutcome::Parked(
                    BatchIngestError::DerivationMismatch { .. }
                ))
            ));
            let heartbeat = Batch {
                from_lez_block_id: 3,
                to_lez_block_id: 3,
                changes: vec![],
                root_after: wrong_tree.root().to_vec(),
            };
            assert!(matches!(
                registry.apply_zone_finalized(entry(3, Some(2), &heartbeat), 30),
                Some(AcceptOutcome::Parked(_))
            ));
            registry.save().unwrap();
            let truth = registry.head().root();
            let correction = Batch {
                from_lez_block_id: 1,
                to_lez_block_id: 2,
                changes: registry
                    .head()
                    .active_leaves()
                    .iter()
                    .map(|(_, m)| Change::insert(m.leaf_index, m.rate_commitment.to_vec()))
                    .collect(),
                root_after: truth.to_vec(),
            };
            registry.close().unwrap();
            (truth, correction)
        };
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, true).unwrap();
            let stall = registry.zone.stall().unwrap();
            assert_eq!(
                (stall.msg, stall.l1_slot, stall.orphans_since),
                ([2; 32], 20, 1)
            );
            assert_eq!(registry.zone.final_tip().unwrap().to_lez_block_id, 1);
            assert_eq!(crate::http::roots_of(&registry, 5).len(), 1);
            assert!(matches!(
                registry.apply_zone_finalized(entry(4, Some(3), &correction), 40),
                Some(AcceptOutcome::Applied(_))
            ));
            assert!(registry.zone.stall().is_none());
            registry.save().unwrap();
            registry.close().unwrap();
        }
        let registry = Registry::open(&dir, [0; 32], 1, false, true).unwrap();
        assert!(registry.zone.stall().is_none());
        assert_eq!(registry.zone.final_entries().len(), 4);
        assert_eq!(registry.zone.final_tip().unwrap().root, truth);
        assert_eq!(registry.zone.superseded(), HashSet::from([[1; 32]]));
        let roots = crate::http::roots_of(&registry, 5);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].produced_by().msg_id, hex::encode([4; 32]));
        assert!(registry.contradicted().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// `REBUILD_TREE` drops the tree, the reader's state and the adopted LEZ blocks, keeps the
    /// Zone channel state and its checkpoint, and derives the pending changes again from LEZ for
    /// the blocks the channel does not cover.
    #[test]
    fn test_rebuild_tree_keeps_the_channel_and_derives_the_tree_again() {
        let dir = dir("rebuild");
        let root_before = {
            let mut registry = Registry::open(&dir, [0; 32], 1, false, false).unwrap();
            registry
                .apply_lez_finalized(&block(1, vec![registered(1, 0)]))
                .unwrap();
            registry
                .apply_lez_finalized(&block(2, vec![registered(2, 1)]))
                .unwrap();
            let first = registry.next_batch().unwrap();
            registry.record_publish(msg(1), MsgId::root(), &first);
            registry.apply_zone_finalized(entry(1, None, &first), 10);
            registry
                .apply_lez_adopted(&block(3, vec![registered(3, 2)]))
                .unwrap();
            registry.set_checkpoint(json!({"lib": 7}));
            registry.save().unwrap();
            let root = registry.head().root();
            registry.close().unwrap();
            root
        };
        let mut registry = Registry::open(&dir, [0; 32], 1, true, false).unwrap();
        assert_eq!(registry.head().lez_block_id, 0);
        assert!(!dir.join(LEZ_VIEW_FILE).exists());
        assert_eq!(registry.zone.final_entries().len(), 1);
        assert_eq!(registry.covered_to(), 2);
        assert!(registry.checkpoint().is_some());
        assert!(registry.pending_changes.is_empty());
        // LEZ again: the covered blocks add nothing pending, the adopted one does.
        registry
            .apply_lez_finalized(&block(1, vec![registered(1, 0)]))
            .unwrap();
        registry
            .apply_lez_finalized(&block(2, vec![registered(2, 1)]))
            .unwrap();
        assert!(registry.pending_changes.is_empty());
        registry
            .apply_lez_adopted(&block(3, vec![registered(3, 2)]))
            .unwrap();
        assert_eq!(registry.pending_changes.len(), 1);
        assert_eq!(registry.head().root(), root_before);
        let next = registry.next_batch().unwrap();
        assert_eq!((next.from_lez_block_id, next.to_lez_block_id), (3, 3));
        assert!(registry.would_apply(&next).is_ok());
        registry.close().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    /// An indexer whose old tree parked a batch (a decoder that skipped a transaction, say)
    /// judges it again after `REBUILD_TREE`: the verdict becomes a wait, the new tree decides.
    #[test]
    fn test_rebuild_tree_on_an_indexer_judges_the_parked_batches_again() {
        let dir = dir("rejudge");
        let honest = {
            let mut sequencer = Registry::open_temporary(1, false).unwrap();
            sequencer
                .apply_lez_adopted(&block(1, vec![registered(1, 0)]))
                .unwrap();
            sequencer.next_batch().unwrap()
        };
        {
            // The old tree read block 1 wrong (another registration): the honest batch parks.
            let mut registry = Registry::open(&dir, [0; 32], 1, false, true).unwrap();
            registry
                .apply_lez_finalized(&block(1, vec![registered(9, 0)]))
                .unwrap();
            assert!(matches!(
                registry.apply_zone_finalized(entry(1, None, &honest), 10),
                Some(AcceptOutcome::Parked(
                    BatchIngestError::DerivationMismatch { .. }
                ))
            ));
            registry.save().unwrap();
            registry.close().unwrap();
        }
        {
            let mut registry = Registry::open(&dir, [0; 32], 1, true, true).unwrap();
            assert!(registry.zone.final_entries()[0].is_waiting());
            assert!(registry.zone.stall().is_none());
            registry
                .apply_lez_finalized(&block(1, vec![registered(1, 0)]))
                .unwrap();
            assert_eq!(registry.retry_folds(), 1);
            assert!(registry.zone.final_entries()[0].error.is_none());
            assert_eq!(registry.zone.final_tip().unwrap().to_lez_block_id, 1);
            registry.save().unwrap();
            registry.close().unwrap();
        }
        // The store's lock is held until the registry is dropped (sled retries for hours).
        let registry = Registry::open(&dir, [0; 32], 1, false, true).unwrap();
        assert!(registry.zone.final_entries()[0].error.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Finality on a lineage that does not hold our publishes (no `Conflict` came): they leave
    /// the view, nothing waits for them, and their batches are queued to publish again like
    /// orphaned ones.
    #[test]
    fn test_finality_on_another_lineage_orphans_the_publishes_in_flight() {
        let mut registry = Registry::open_temporary(1, false).unwrap();
        registry
            .apply_lez_adopted(&block(1, vec![registered(1, 0)]))
            .unwrap();
        let first = registry.next_batch().unwrap();
        registry.record_publish(msg(1), MsgId::root(), &first);
        registry
            .apply_lez_adopted(&block(2, vec![registered(2, 1)]))
            .unwrap();
        let second = registry.next_batch().unwrap();
        registry.record_publish(msg(2), msg(1), &second);
        assert_eq!(registry.in_flight(), Some([2; 32]));
        // Entry 7 on the root carries the first batch and finalizes: our entries are gone.
        assert!(matches!(
            registry.apply_zone_finalized(entry(7, None, &first), 10),
            Some(AcceptOutcome::Applied(_))
        ));
        assert!(registry.zone.view().is_empty());
        assert_eq!(registry.zone.final_msg(), msg(7));
        assert!(registry.in_flight().is_none());
        assert_eq!(registry.orphaned_count(), 2);
        assert_eq!(registry.covered_to(), 1);
        // Both go back to the queue in order; the first is covered by entry 7 already and
        // fails to apply (the tick drops it as any orphaned batch that no longer applies).
        assert_eq!(registry.next_republish(), Some(first.clone()));
        registry.republished();
        assert_eq!(registry.next_republish(), Some(second.clone()));
        assert!(registry.would_apply(&second).is_ok());
    }

    /// The anchor: seeded once at the slot the SDK starts from, moved forward only, untouched
    /// by an entry delivered again (the stream, then the finality scan).
    #[test]
    fn test_the_anchor_is_seeded_once_and_moves_forward_only() {
        let mut registry = Registry::open_temporary(1, false).unwrap();
        registry.scanned_to(5);
        assert!(registry.anchor().is_none());
        registry.seed_anchor(100);
        registry.seed_anchor(200);
        let seeded = registry.anchor().unwrap();
        assert_eq!(
            (seeded.slot, seeded.to_lez_block_id, seeded.root),
            (100, 0, *EMPTY_ROOT)
        );
        registry
            .apply_lez_adopted(&block(1, vec![registered(1, 0)]))
            .unwrap();
        let first = registry.next_batch().unwrap();
        registry.record_publish(msg(1), MsgId::root(), &first);
        registry.apply_zone_finalized(entry(1, None, &first), 150);
        let moved = registry.anchor().unwrap();
        assert_eq!((moved.slot, moved.to_lez_block_id), (150, 1));
        assert_eq!(moved.root, Tip::from(&first).root);
        assert_eq!(
            registry.apply_zone_finalized(entry(1, None, &first), 150),
            Some(AcceptOutcome::AlreadyApplied)
        );
        assert_eq!(registry.anchor(), Some(moved));
        registry.scanned_to(120);
        assert_eq!(registry.anchor().unwrap().slot, 150);
        registry.scanned_to(300);
        assert_eq!(registry.anchor().unwrap().slot, 300);
        // A batchless entry has no outcome, twice.
        let bare = ChannelEntry::new(msg(2), msg(1), &[]);
        assert_eq!(registry.apply_zone_finalized(bare.clone(), 310), None);
        assert_eq!(registry.apply_zone_finalized(bare, 310), None);
    }

    /// Corners: a channel tip that is not ours lands nothing; a checkpoint that is not an object
    /// is kept as it is; a canonical lineage listing an entry twice holds it once; the next
    /// batch stops where its changes stop fitting one inscription.
    #[test]
    fn test_registry_corners() {
        let mut registry = Registry::open_temporary(1, false).unwrap();
        registry
            .apply_lez_adopted(&block(1, vec![registered(1, 0)]))
            .unwrap();
        let first = registry.next_batch().unwrap();
        registry.record_publish(msg(1), MsgId::root(), &first);
        assert!(registry.mark_adopted_up_to([9; 32]).is_empty());
        assert_eq!(registry.in_flight(), Some([1; 32]));
        registry.set_checkpoint(json!(5));
        assert_eq!(registry.checkpoint(), Some(&json!(5)));
        let twice = vec![entry(1, None, &first), entry(1, None, &first)];
        assert!(registry.apply_zone_conflict(twice).is_empty());
        assert_eq!(registry.zone.view().len(), 1);
        // Three registrations in block 2, one in block 3, an inscription that holds two
        // changes: the batch covers block 2 whole (at least one block, over the size), the next
        // one block 3; a real inscription holds both.
        registry
            .apply_lez_adopted(&block(
                2,
                vec![registered(2, 1), registered(3, 2), registered(4, 3)],
            ))
            .unwrap();
        registry
            .apply_lez_adopted(&block(3, vec![registered(5, 4)]))
            .unwrap();
        let two_changes = Batch {
            from_lez_block_id: 2,
            to_lez_block_id: 3,
            changes: vec![
                Change::insert(1, vec![0; 32]),
                Change::insert(2, vec![0; 32]),
            ],
            root_after: vec![0; 32],
        }
        .encoded_len();
        let cut = registry.next_batch_within(two_changes).unwrap();
        assert_eq!((cut.from_lez_block_id, cut.to_lez_block_id), (2, 2));
        assert_eq!(cut.changes.len(), 3);
        assert_eq!(cut.root_after, registry.head().root_at(2).unwrap().to_vec());
        assert!(registry.would_apply(&cut).is_ok());
        let whole = registry.next_batch().unwrap();
        assert_eq!((whole.from_lez_block_id, whole.to_lez_block_id), (2, 3));
        assert_eq!(whole.changes.len(), 4);
        assert!(whole.encoded_len() <= MAX_INSCRIPTION_BYTES);
        assert!(registry.would_apply(&whole).is_ok());
    }
}
