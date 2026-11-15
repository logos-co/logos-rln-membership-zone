//! The Zone channel as a consumer keeps it, after LEZ's `chain_state`: a final tier
//! (irreversible, from the finalized inscriptions) and a head that is always the final state
//! folded over the view, the unfinalized message lineage the Zone SDK reports. Every node
//! holding the same view derives the same head, and the pin is the view's last entry.
//!
//! A fold applies one batch the way LEZ applies one block: the batch must chain on the tip
//! (`from_lez_block_id` continues it), its changes must replay to its `root_after`, and the
//! judge (the indexer's own tree derived from LEZ) must agree with that root. A batch that
//! fails parks: the tip freezes and every later entry parks after it, the LEZ semantics.

use std::collections::HashSet;

use anyhow::{Context as _, Result};
use borsh::{BorshDeserialize, BorshSerialize};
use lb_core::mantle::ops::channel::MsgId;
use prost::Message as _;
use rln::prelude::{Fr, PoseidonHash};
use serde::{Deserialize, Serialize};
use zerokit_utils::merkle_tree::{OptimalMerkleConfig, OptimalMerkleTree, ZerokitMerkleTree};

use crate::{
    batch::{Batch, Change, Kind},
    reader::{TREE_DEPTH, fr_from_bytes_le, fr_to_bytes_le},
};

/// One inscription on the channel's message lineage.
#[derive(Debug, Clone)]
pub struct ChannelEntry {
    pub msg: MsgId,
    pub parent: MsgId,
    /// `None` for an entry that carries no batch: an empty or undecodable payload.
    pub batch: Option<Batch>,
}

impl ChannelEntry {
    /// An entry from an inscription's payload; an empty or undecodable one carries no batch.
    pub fn new(msg: MsgId, parent: MsgId, payload: &[u8]) -> Self {
        Self {
            msg,
            parent,
            batch: (!payload.is_empty())
                .then(|| Batch::decode(payload).ok())
                .flatten(),
        }
    }
}

/// The parent the next batch must chain on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tip {
    pub to_lez_block_id: u64,
    #[serde(with = "hex::serde")]
    pub root: [u8; 32],
}

impl From<&Batch> for Tip {
    fn from(batch: &Batch) -> Self {
        let mut root = [0u8; 32];
        if batch.root_after.len() == 32 {
            root.copy_from_slice(&batch.root_after);
        }
        Self {
            to_lez_block_id: batch.to_lez_block_id,
            root,
        }
    }
}

/// What recording one of our own publishes did.
#[derive(Debug, PartialEq)]
pub enum PublishRecord {
    /// The entry is in the view and its batch entered the head.
    Extended,
    /// The entry is in the view but the head did not gain its batch.
    Skipped,
    /// The view already holds the entry.
    AlreadyInView,
    /// The view moved past the entry's parent since the batch was built.
    Stale,
}

/// Why a batch does not apply on a tip (LEZ `BlockIngestError`, the batch's checks; the two
/// last variants are the judge's and have no LEZ counterpart).
#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BatchIngestError {
    /// No batch, or a change that is not one (a leaf that is not 32 bytes, a change without a
    /// kind).
    #[error("Failed to deserialize the batch: {0}")]
    Deserialize(String),
    #[error("Unexpected block id: expected {expected}, got {got}")]
    UnexpectedBlockId { expected: u64, got: u64 },
    /// The changes replay to `computed`, the batch claims `claimed`.
    #[error("Hash mismatch: computed {computed}, claimed {claimed}")]
    HashMismatch { computed: String, claimed: String },
    /// The judge's tree derived from LEZ says `derived` at `to_lez_block_id`.
    #[error("Derivation mismatch: derived {derived}, claimed {claimed}")]
    DerivationMismatch { derived: String, claimed: String },
    #[error("LEZ not read up to block {to} yet")]
    LezNotRead { to: u64 },
}

impl BatchIngestError {
    /// Whether the failure may pass later: the judge has not read LEZ that far yet.
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::LezNotRead { .. })
    }
}

/// Outcome of feeding a batch to a tip.
#[derive(Debug, PartialEq)]
pub enum AcceptOutcome {
    /// Chained and applied; the tip advances.
    Applied(Vec<Change>),
    /// A duplicate re-delivery of an already-applied batch. No state change.
    AlreadyApplied,
    /// Did not chain or failed to apply; the tip stays frozen.
    Parked(BatchIngestError),
    /// Chained but failed to apply, possibly transiently ([`BatchIngestError::is_retryable`]);
    /// nothing recorded, tip and state untouched. The caller retries later.
    RetryableFailure(BatchIngestError),
}

/// Diagnostic record of the first batch that broke the final tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StallReason {
    #[serde(with = "hex::serde")]
    pub msg: [u8; 32],
    pub l1_slot: u64,
    pub error: BatchIngestError,
    /// Later finalized batches that could not apply since (orphans, the tip being frozen).
    pub orphans_since: u64,
}

/// What the tree derived from LEZ is at a block: the indexer's own derivation. The sequencer
/// has no judge (`None`): its batches are its own tree's roots.
pub trait Judge {
    /// The tree's root at `to_lez_block_id`; `None` while LEZ is not read that far.
    fn root_at(&self, to_lez_block_id: u64) -> Option<[u8; 32]>;
}

/// `tree` after `batch` when it chains on `tip` (the first batch on `first_lez_block_id`) and
/// the judge, if any, agrees.
fn apply_batch(
    first_lez_block_id: u64,
    tip: Option<&Tip>,
    batch: &Batch,
    tree: &ReplayTree,
    judge: Option<&dyn Judge>,
) -> Result<ReplayTree, BatchIngestError> {
    let expected = tip.map_or(first_lez_block_id, |tip| tip.to_lez_block_id + 1);
    if batch.from_lez_block_id != expected {
        return Err(BatchIngestError::UnexpectedBlockId {
            expected,
            got: batch.from_lez_block_id,
        });
    }
    let claimed = Tip::from(batch).root;
    let mut next = tree.clone();
    next.apply(&batch.changes)
        .map_err(|e| BatchIngestError::Deserialize(format!("{e:#}")))?;
    let computed = next.root();
    if computed != claimed {
        return Err(BatchIngestError::HashMismatch {
            computed: hex::encode(computed),
            claimed: hex::encode(claimed),
        });
    }
    if let Some(judge) = judge {
        match judge.root_at(batch.to_lez_block_id) {
            None => {
                return Err(BatchIngestError::LezNotRead {
                    to: batch.to_lez_block_id,
                });
            }
            Some(derived) if derived != claimed => {
                return Err(BatchIngestError::DerivationMismatch {
                    derived: hex::encode(derived),
                    claimed: hex::encode(claimed),
                });
            }
            Some(_) => {}
        }
    }
    Ok(next)
}

/// The tree the batches replay to: in memory, rebuilt from the final tier on start.
#[derive(Clone)]
pub struct ReplayTree(OptimalMerkleTree<PoseidonHash>);

impl ReplayTree {
    pub fn empty() -> Self {
        Self(
            OptimalMerkleTree::<PoseidonHash>::new(
                TREE_DEPTH,
                Fr::default(),
                OptimalMerkleConfig::default(),
            )
            .expect("an in-memory tree of depth 20"),
        )
    }

    pub fn root(&self) -> [u8; 32] {
        fr_to_bytes_le(&self.0.root())
    }

    /// Apply a batch's changes: an insert sets the leaf, a removal empties it.
    pub fn apply(&mut self, changes: &[Change]) -> Result<()> {
        for change in changes {
            match &change.kind {
                Some(Kind::Insert(insert)) => {
                    let bytes: [u8; 32] = insert
                        .rate_commitment
                        .as_slice()
                        .try_into()
                        .context("a leaf that is not 32 bytes")?;
                    let leaf = fr_from_bytes_le(&bytes).map_err(|e| anyhow::anyhow!(e))?;
                    self.0
                        .set(insert.leaf_index as usize, leaf)
                        .with_context(|| format!("insert leaf {}", insert.leaf_index))?;
                }
                Some(Kind::Remove(remove)) => self
                    .0
                    .delete(remove.leaf_index as usize)
                    .with_context(|| format!("remove leaf {}", remove.leaf_index))?,
                None => anyhow::bail!("a change has no kind"),
            }
        }
        Ok(())
    }
}

/// The head after folding one view entry.
struct Fold {
    state: ReplayTree,
    tip: Option<Tip>,
    /// The batch's changes, when the fold applied it; the error when it did not.
    outcome: Result<Vec<Change>, BatchIngestError>,
}

/// A finalized entry as the final tier keeps it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalEntry {
    #[serde(with = "hex::serde")]
    pub msg: [u8; 32],
    #[serde(with = "hex::serde")]
    pub parent: [u8; 32],
    pub l1_slot: u64,
    /// The batch, prost bytes in hex; `None` for an entry that carried none.
    pub batch: Option<String>,
    /// The final tier's outcome: `None` applied, `Some` the error it parked on.
    pub error: Option<BatchIngestError>,
}

impl FinalEntry {
    pub fn batch(&self) -> Option<Batch> {
        let hex = self.batch.as_ref()?;
        Batch::decode(hex::decode(hex).ok()?.as_slice()).ok()
    }

    /// Whether the judge has still to decide it (LEZ not read that far yet).
    pub fn is_waiting(&self) -> bool {
        self.error
            .as_ref()
            .is_some_and(BatchIngestError::is_retryable)
    }
}

/// The persisted form of the view (LEZ `StoredView`).
#[derive(BorshSerialize, BorshDeserialize)]
struct StoredView {
    final_msg: [u8; 32],
    entries: Vec<StoredEntry>,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct StoredEntry {
    msg: [u8; 32],
    parent: [u8; 32],
    batch: Option<Vec<u8>>,
}

/// The final tier (irreversible, from `finalized`) and the head, which is always the final
/// state folded over `view`.
pub struct ChainState {
    /// The LEZ block the first batch starts at (the program's deployment block).
    first_lez_block_id: u64,
    final_state: ReplayTree,
    final_tip: Option<Tip>,
    /// The newest finalized entry on the message lineage, batch or not.
    final_msg: MsgId,
    /// Every finalized entry, in lineage order.
    final_entries: Vec<FinalEntry>,
    /// The first finalized batch that could not apply, if any: the final tip is frozen there.
    stall: Option<StallReason>,
    /// The unfinalized message lineage, oldest first, chained on `final_msg`.
    view: Vec<ChannelEntry>,
    /// `folds[i]` is the head after folding `view[..=i]`.
    folds: Vec<Fold>,
}

impl ChainState {
    /// Fresh state: the empty tree, no entry; the first batch must start at `first_lez_block_id`.
    pub fn new(first_lez_block_id: u64) -> Self {
        Self {
            first_lez_block_id,
            final_state: ReplayTree::empty(),
            final_tip: None,
            final_msg: MsgId::root(),
            final_entries: vec![],
            stall: None,
            view: vec![],
            folds: vec![],
        }
    }

    /// State restored from a persisted final tier (the entries replay in order, the first
    /// parked one freezes the tip) and the persisted view.
    pub fn restore(
        first_lez_block_id: u64,
        final_entries: Vec<FinalEntry>,
        stall: Option<StallReason>,
        view: &[u8],
        judge: Option<&dyn Judge>,
    ) -> Result<Self> {
        let mut chain = Self::new(first_lez_block_id);
        for entry in final_entries {
            if entry.error.is_none()
                && let Some(batch) = entry.batch()
            {
                chain.final_state = apply_batch(
                    first_lez_block_id,
                    chain.final_tip.as_ref(),
                    &batch,
                    &chain.final_state,
                    None,
                )
                .map_err(|e| anyhow::anyhow!("final batch msg {}: {e}", hex::encode(entry.msg)))?;
                chain.final_tip = Some(Tip::from(&batch));
            }
            chain.final_msg = MsgId::from(entry.msg);
            chain.final_entries.push(entry);
        }
        chain.stall = stall;
        if !view.is_empty() {
            chain.restore_view(view, judge)?;
        }
        Ok(chain)
    }

    pub fn final_tip(&self) -> Option<&Tip> {
        self.final_tip.as_ref()
    }

    pub fn final_msg(&self) -> MsgId {
        self.final_msg
    }

    pub fn final_entries(&self) -> &[FinalEntry] {
        &self.final_entries
    }

    pub fn stall(&self) -> Option<&StallReason> {
        self.stall.as_ref()
    }

    pub fn view(&self) -> &[ChannelEntry] {
        &self.view
    }

    /// The outcome of every view entry, in view order.
    pub fn view_outcomes(
        &self,
    ) -> impl Iterator<Item = (&ChannelEntry, &Result<Vec<Change>, BatchIngestError>)> {
        self.view.iter().zip(self.folds.iter().map(|f| &f.outcome))
    }

    /// Parent the next batch must chain on.
    pub fn head_tip(&self) -> Option<&Tip> {
        self.folds
            .last()
            .map_or(self.final_tip.as_ref(), |fold| fold.tip.as_ref())
    }

    /// The root the head's replay is at.
    pub fn head_root(&self) -> [u8; 32] {
        self.folds
            .last()
            .map_or(&self.final_state, |fold| &fold.state)
            .root()
    }

    /// Parent the next inscription must chain on: the view's last entry.
    pub fn pin(&self) -> MsgId {
        self.view.last().map_or(self.final_msg, |entry| entry.msg)
    }

    /// The first view entry not chained on the one before it, or on `final_msg` for the first.
    pub fn check_for_gaps_in_lineage(&self) -> Option<&ChannelEntry> {
        let mut parent = self.final_msg;
        self.view.iter().find(|entry| {
            let gap = entry.parent != parent;
            parent = entry.msg;
            gap
        })
    }

    /// The view and `final_msg` in their persisted form. A skipped entry is stored without
    /// its batch: it folds the same either way.
    pub fn encode_view(&self) -> Vec<u8> {
        let stored = StoredView {
            final_msg: self.final_msg.into(),
            entries: self
                .view
                .iter()
                .zip(&self.folds)
                .map(|(entry, fold)| StoredEntry {
                    msg: entry.msg.into(),
                    parent: entry.parent.into(),
                    batch: entry
                        .batch
                        .as_ref()
                        .filter(|_| {
                            fold.outcome.is_ok()
                                || fold
                                    .outcome
                                    .as_ref()
                                    .is_err_and(BatchIngestError::is_retryable)
                        })
                        .map(Batch::encode_to_vec),
                })
                .collect(),
        };
        borsh::to_vec(&stored).expect("view serializes")
    }

    /// Installs a persisted view on the restored final tier and folds it.
    pub fn restore_view(&mut self, bytes: &[u8], judge: Option<&dyn Judge>) -> Result<()> {
        let stored = StoredView::try_from_slice(bytes).context("the persisted channel view")?;
        self.final_msg = MsgId::from(stored.final_msg);
        self.view = stored
            .entries
            .into_iter()
            .map(|entry| ChannelEntry {
                msg: MsgId::from(entry.msg),
                parent: MsgId::from(entry.parent),
                batch: entry.batch.and_then(|b| Batch::decode(b.as_slice()).ok()),
            })
            .collect();
        self.fold_from(0, judge);
        Ok(())
    }

    /// Entries that entered the view, on a channel update that dropped nothing.
    pub fn apply_extension(&mut self, adopted: Vec<ChannelEntry>, judge: Option<&dyn Judge>) {
        for entry in adopted {
            // The sdk re-reports held entries after a restart.
            if self.view.iter().any(|held| held.msg == entry.msg) {
                continue;
            }
            if entry.parent != self.pin() {
                println!(
                    "adopted channel entry {} chains on {}, not on the pin {}",
                    short(&entry.msg),
                    short(&entry.parent),
                    short(&self.pin())
                );
            }
            self.push(entry, judge);
        }
    }

    /// The whole view at the new tip, on a channel update that dropped entries. Returns the
    /// entries that left the view (ours to publish again).
    pub fn apply_conflict(
        &mut self,
        canonical: Vec<ChannelEntry>,
        judge: Option<&dyn Judge>,
    ) -> Vec<ChannelEntry> {
        let mut seen = HashSet::new();
        let canonical: Vec<ChannelEntry> = canonical
            .into_iter()
            .filter(|entry| seen.insert(entry.msg))
            .collect();
        let dropped: Vec<ChannelEntry> = self
            .view
            .iter()
            .filter(|held| !seen.contains(&held.msg))
            .cloned()
            .collect();
        // The entries both views hold keep their folds: a `MsgId` commits to its payload,
        // so the same id carries the same batch.
        let fork = self
            .view
            .iter()
            .zip(&canonical)
            .take_while(|(held, entry)| held.msg == entry.msg)
            .count();
        self.view = canonical;
        self.fold_from(fork, judge);
        dropped
    }

    /// Records an inscription of ours, which chains on the pin it was built on.
    pub fn record_publish(
        &mut self,
        entry: ChannelEntry,
        judge: Option<&dyn Judge>,
    ) -> PublishRecord {
        if self.view.iter().any(|held| held.msg == entry.msg) {
            return PublishRecord::AlreadyInView;
        }
        if entry.parent != self.pin() {
            return PublishRecord::Stale;
        }
        self.push(entry, judge);
        match &self.folds.last().expect("just pushed").outcome {
            Ok(_) => PublishRecord::Extended,
            Err(_) => PublishRecord::Skipped,
        }
    }

    /// A finalized entry chained on `parent`. Returns the final tier's outcome for its
    /// batch, `None` when it carries none. `final_msg` moves only to an entry chained on it;
    /// anything else leaves the view if held, and its batch still applies to the final tier.
    pub fn apply_finalized(
        &mut self,
        entry: ChannelEntry,
        l1_slot: u64,
        judge: Option<&dyn Judge>,
    ) -> Option<AcceptOutcome> {
        if self
            .final_entries
            .iter()
            .any(|f| f.msg == <[u8; 32]>::from(entry.msg))
        {
            return entry.batch.as_ref().map(|_| AcceptOutcome::AlreadyApplied);
        }
        // Finality is prefix-monotone: the held entries this one chains on finalize first.
        if let Some(ancestor) = self
            .view
            .iter()
            .find(|held| held.msg == entry.parent)
            .cloned()
        {
            self.apply_finalized(ancestor, l1_slot, judge);
        }
        if entry.parent != self.final_msg {
            println!(
                "finalized entry {} chains on {}, not on the final entry {}",
                short(&entry.msg),
                short(&entry.parent),
                short(&self.final_msg)
            );
        }
        let outcome = entry
            .batch
            .as_ref()
            .map(|batch| self.apply_final_batch(batch, &entry, l1_slot, judge));
        self.final_entries.push(FinalEntry {
            msg: entry.msg.into(),
            parent: entry.parent.into(),
            l1_slot,
            batch: entry.batch.as_ref().map(|b| hex::encode(b.encode_to_vec())),
            error: match &outcome {
                Some(AcceptOutcome::Parked(e) | AcceptOutcome::RetryableFailure(e)) => {
                    Some(e.clone())
                }
                _ => None,
            },
        });
        self.advance_final_msg(entry.msg, judge);
        outcome
    }

    /// Makes `msg` the final entry and keeps only the view entries chained on it.
    fn advance_final_msg(&mut self, msg: MsgId, judge: Option<&dyn Judge>) {
        self.final_msg = msg;
        let mut lineage = HashSet::from([msg]);
        self.view
            .retain(|entry| lineage.contains(&entry.parent) && lineage.insert(entry.msg));
        // The folds sit on the final tier, so none survives it moving.
        self.fold_from(0, judge);
    }

    /// Applies a finalized batch straight to the final tier: a failure parks it and every
    /// later one (the tip is frozen); a retryable failure (the judge has not read LEZ that
    /// far) leaves it and every later one waiting for `retry_folds`.
    fn apply_final_batch(
        &mut self,
        batch: &Batch,
        entry: &ChannelEntry,
        l1_slot: u64,
        judge: Option<&dyn Judge>,
    ) -> AcceptOutcome {
        if let Some(stall) = &mut self.stall {
            stall.orphans_since += 1;
            return AcceptOutcome::Parked(BatchIngestError::UnexpectedBlockId {
                expected: self.final_tip.as_ref().map_or(1, |t| t.to_lez_block_id + 1),
                got: batch.from_lez_block_id,
            });
        }
        if self.final_entries.iter().any(FinalEntry::is_waiting) {
            return AcceptOutcome::RetryableFailure(BatchIngestError::LezNotRead {
                to: batch.to_lez_block_id,
            });
        }
        match apply_batch(
            self.first_lez_block_id,
            self.final_tip.as_ref(),
            batch,
            &self.final_state,
            judge,
        ) {
            Ok(state) => {
                self.final_state = state;
                self.final_tip = Some(Tip::from(batch));
                AcceptOutcome::Applied(batch.changes.clone())
            }
            Err(error) if error.is_retryable() => AcceptOutcome::RetryableFailure(error),
            Err(error) => {
                self.stall = Some(StallReason {
                    msg: entry.msg.into(),
                    l1_slot,
                    error: error.clone(),
                    orphans_since: 0,
                });
                AcceptOutcome::Parked(error)
            }
        }
    }

    /// The final tier's batches still waiting for the judge, in order; the first still
    /// undecided keeps the ones after it waiting. Returns how many got an outcome.
    fn retry_final(&mut self, judge: Option<&dyn Judge>) -> usize {
        let mut decided = 0;
        for i in 0..self.final_entries.len() {
            if !self.final_entries[i].is_waiting() {
                continue;
            }
            let Some(batch) = self.final_entries[i].batch() else {
                continue;
            };
            match apply_batch(
                self.first_lez_block_id,
                self.final_tip.as_ref(),
                &batch,
                &self.final_state,
                judge,
            ) {
                Ok(state) => {
                    self.final_state = state;
                    self.final_tip = Some(Tip::from(&batch));
                    self.final_entries[i].error = None;
                    decided += 1;
                }
                Err(error) if error.is_retryable() => break,
                Err(error) => {
                    self.stall = Some(StallReason {
                        msg: self.final_entries[i].msg,
                        l1_slot: self.final_entries[i].l1_slot,
                        error: error.clone(),
                        orphans_since: 0,
                    });
                    self.final_entries[i].error = Some(error);
                    decided += 1;
                    break;
                }
            }
        }
        decided
    }

    /// Appends `entry` to the view and folds it onto the head.
    fn push(&mut self, entry: ChannelEntry, judge: Option<&dyn Judge>) {
        let idx = self.view.len();
        self.view.push(entry);
        self.fold_from(idx, judge);
    }

    /// Refolds the view from `idx` on, after it changed there.
    fn fold_from(&mut self, idx: usize, judge: Option<&dyn Judge>) {
        self.folds.truncate(idx);
        while let Some(entry) = self.view.get(self.folds.len()) {
            let fold = self.fold_next(entry, judge);
            self.folds.push(fold);
        }
    }

    /// Judges again what the judge could not decide yet, once LEZ is read further: the final
    /// tier's waiting batches first, then the view from its first undecided fold. Returns how
    /// many entries got an outcome.
    pub fn retry_folds(&mut self, judge: Option<&dyn Judge>) -> usize {
        let final_decided = self.retry_final(judge);
        let first = if final_decided > 0 {
            Some(0)
        } else {
            self.folds.iter().position(|fold| {
                fold.outcome
                    .as_ref()
                    .is_err_and(BatchIngestError::is_retryable)
            })
        };
        let Some(first) = first else {
            return final_decided;
        };
        let undecided_before = self.undecided();
        self.fold_from(first, judge);
        final_decided + undecided_before.saturating_sub(self.undecided())
    }

    fn undecided(&self) -> usize {
        self.folds
            .iter()
            .filter(|fold| {
                fold.outcome
                    .as_ref()
                    .is_err_and(BatchIngestError::is_retryable)
            })
            .count()
    }

    /// The head after folding `entry` onto the current one.
    fn fold_next(&self, entry: &ChannelEntry, judge: Option<&dyn Judge>) -> Fold {
        let (state, tip) = self
            .folds
            .last()
            .map_or((&self.final_state, self.final_tip.as_ref()), |fold| {
                (&fold.state, fold.tip.as_ref())
            });
        // Behind an entry the judge has still to decide nothing can be judged: the tip it would
        // chain on is not confirmed yet (as the final tier waits in `apply_final_batch`).
        let waiting = self.folds.last().is_some_and(|fold| {
            fold.outcome
                .as_ref()
                .is_err_and(BatchIngestError::is_retryable)
        });
        let outcome = match &entry.batch {
            Some(batch) if waiting => Err(BatchIngestError::LezNotRead {
                to: batch.to_lez_block_id,
            }),
            Some(batch) => apply_batch(self.first_lez_block_id, tip, batch, state, judge)
                .map(|next| (next, Tip::from(batch), batch.changes.clone())),
            None => Err(BatchIngestError::Deserialize(
                "the inscription is not a batch".into(),
            )),
        };
        match outcome {
            Ok((next, tip, changes)) => Fold {
                state: next,
                tip: Some(tip),
                outcome: Ok(changes),
            },
            Err(error) => Fold {
                state: state.clone(),
                tip: tip.cloned(),
                outcome: Err(error),
            },
        }
    }
}

fn short(msg: &MsgId) -> String {
    hex::encode(&<[u8; 32]>::from(*msg)[..4])
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::reader::EMPTY_ROOT;

    fn msg(n: u8) -> MsgId {
        MsgId::from([n; 32])
    }

    fn leaf(n: u8) -> Vec<u8> {
        let mut bytes = [0u8; 32];
        bytes[0] = n;
        bytes.to_vec()
    }

    /// A batch of `changes` over LEZ blocks `from..=to`, its root the replay of `before`.
    fn batch(from: u64, to: u64, changes: Vec<Change>, before: &ReplayTree) -> Batch {
        let mut tree = before.clone();
        tree.apply(&changes).unwrap();
        Batch {
            from_lez_block_id: from,
            to_lez_block_id: to,
            changes,
            root_after: tree.root().to_vec(),
        }
    }

    fn entry(n: u8, parent: u8, batch: Option<Batch>) -> ChannelEntry {
        ChannelEntry {
            msg: msg(n),
            parent: if parent == 0 {
                MsgId::root()
            } else {
                msg(parent)
            },
            batch,
        }
    }

    /// A judge whose tree is the replay of the batches it was told, up to `read_to`.
    struct Oracle {
        roots: Vec<(u64, [u8; 32])>,
        read_to: u64,
    }

    impl Judge for Oracle {
        fn root_at(&self, to: u64) -> Option<[u8; 32]> {
            if to > self.read_to {
                return None;
            }
            Some(
                self.roots
                    .iter()
                    .rev()
                    .find(|(block, _)| *block <= to)
                    .map_or(*EMPTY_ROOT, |(_, root)| *root),
            )
        }
    }

    #[test]
    fn test_view_folds_record_publish_and_finality_move_the_tiers() {
        let mut chain = ChainState::new(1);
        let empty = ReplayTree::empty();
        let b1 = batch(1, 3, vec![Change::insert(0, leaf(1))], &empty);
        let one = {
            let mut t = empty.clone();
            t.apply(&b1.changes).unwrap();
            t
        };
        let b2 = batch(4, 5, vec![Change::insert(1, leaf(2))], &one);

        // Our publish chains on the pin (root) and extends the head.
        assert_eq!(
            chain.record_publish(entry(1, 0, Some(b1.clone())), None),
            PublishRecord::Extended
        );
        assert_eq!(chain.pin(), msg(1));
        assert_eq!(chain.head_tip().unwrap().to_lez_block_id, 3);
        assert_eq!(
            chain.record_publish(entry(1, 0, Some(b1.clone())), None),
            PublishRecord::AlreadyInView
        );
        // A publish built on an old pin is stale.
        assert_eq!(
            chain.record_publish(entry(9, 0, Some(b2.clone())), None),
            PublishRecord::Stale
        );
        // The SDK reports the next one adopted.
        chain.apply_extension(vec![entry(2, 1, Some(b2.clone()))], None);
        assert_eq!(chain.view().len(), 2);
        assert_eq!(chain.head_root(), Tip::from(&b2).root);
        assert!(chain.check_for_gaps_in_lineage().is_none());

        // Persist and restore the view.
        let bytes = chain.encode_view();
        let mut restored = ChainState::new(1);
        restored.restore_view(&bytes, None).unwrap();
        assert_eq!(restored.pin(), msg(2));
        assert_eq!(restored.head_root(), chain.head_root());

        // Finality reaches entry 2: entry 1 finalizes first (prefix-monotone), the view empties,
        // the final tier holds both.
        let outcome = chain.apply_finalized(entry(2, 1, Some(b2.clone())), 500, None);
        assert!(matches!(outcome, Some(AcceptOutcome::Applied(_))));
        assert_eq!(chain.final_msg(), msg(2));
        assert_eq!(chain.final_entries().len(), 2);
        assert!(chain.view().is_empty());
        assert_eq!(chain.final_tip().unwrap().to_lez_block_id, 5);
        assert_eq!(chain.head_root(), Tip::from(&b2).root);
        // Re-delivered: already applied.
        assert_eq!(
            chain.apply_finalized(entry(2, 1, Some(b2)), 500, None),
            Some(AcceptOutcome::AlreadyApplied)
        );

        // Restore from the final tier alone.
        let again =
            ChainState::restore(1, chain.final_entries().to_vec(), None, &[], None).unwrap();
        assert_eq!(again.final_tip(), chain.final_tip());
        assert_eq!(again.head_root(), chain.head_root());
    }

    #[test]
    fn test_conflict_rebuilds_the_view_and_returns_what_left_it() {
        let mut chain = ChainState::new(1);
        let empty = ReplayTree::empty();
        let b1 = batch(1, 1, vec![Change::insert(0, leaf(1))], &empty);
        let b1_again = Batch {
            to_lez_block_id: 2,
            ..b1.clone()
        };
        chain.record_publish(entry(1, 0, Some(b1.clone())), None);
        // A reorg: our entry left the view, a competing one took its place.
        let dropped = chain.apply_conflict(vec![entry(7, 0, Some(b1_again.clone()))], None);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].msg, msg(1));
        assert_eq!(chain.pin(), msg(7));
        assert_eq!(chain.head_tip().unwrap().to_lez_block_id, 2);
        // The dropped entry goes out again on the new pin.
        let republished = ChannelEntry {
            msg: msg(8),
            parent: msg(7),
            batch: Some(Batch {
                from_lez_block_id: 3,
                to_lez_block_id: 3,
                changes: vec![],
                root_after: b1.root_after.clone(),
            }),
        };
        assert_eq!(
            chain.record_publish(republished, None),
            PublishRecord::Extended
        );
    }

    #[test]
    fn test_the_indexer_judges_parks_the_final_tier_and_retries_the_view() {
        let empty = ReplayTree::empty();
        let b1 = batch(1, 2, vec![Change::insert(0, leaf(1))], &empty);
        let root_1 = Tip::from(&b1).root;
        let one = {
            let mut t = empty.clone();
            t.apply(&b1.changes).unwrap();
            t
        };
        let b2 = batch(3, 4, vec![Change::insert(1, leaf(2))], &one);
        let root_2 = Tip::from(&b2).root;
        let mut oracle = Oracle {
            roots: vec![(2, root_1), (4, root_2)],
            read_to: 0,
        };
        let mut chain = ChainState::new(1);

        // LEZ not read at all: both entries wait, the second behind the first (it cannot park
        // on a tip the judge has not confirmed), the head stays empty.
        chain.apply_extension(
            vec![entry(1, 0, Some(b1.clone())), entry(2, 1, Some(b2.clone()))],
            Some(&oracle),
        );
        let outcomes: Vec<_> = chain.view_outcomes().map(|(_, o)| o.clone()).collect();
        assert_eq!(outcomes[0], Err(BatchIngestError::LezNotRead { to: 2 }));
        assert_eq!(outcomes[1], Err(BatchIngestError::LezNotRead { to: 4 }));
        assert_eq!(chain.head_tip(), None);
        // LEZ read to 2: entry 1 applied. Entry 2 needs block 4: undecided, retried.
        oracle.read_to = 2;
        assert_eq!(chain.retry_folds(Some(&oracle)), 1);
        let outcomes: Vec<_> = chain.view_outcomes().map(|(_, o)| o.clone()).collect();
        assert!(outcomes[0].is_ok());
        assert_eq!(outcomes[1], Err(BatchIngestError::LezNotRead { to: 4 }));
        assert_eq!(chain.head_tip().unwrap().to_lez_block_id, 2);
        oracle.read_to = 4;
        assert_eq!(chain.retry_folds(Some(&oracle)), 1);
        assert_eq!(chain.head_tip().unwrap().to_lez_block_id, 4);
        // A persisted view keeps an undecided entry's batch (it will be judged), not a parked
        // one's.
        let bytes = chain.encode_view();
        let mut restored = ChainState::new(1);
        restored.restore_view(&bytes, Some(&oracle)).unwrap();
        assert_eq!(restored.head_root(), root_2);

        // A batch that claims a root LEZ does not give: parked in the view, and a gap after it.
        let mut wrong = batch(5, 5, vec![], &one);
        wrong.root_after = vec![9; 32];
        oracle.read_to = 6;
        chain.apply_extension(vec![entry(3, 2, Some(wrong.clone()))], Some(&oracle));
        let outcomes: Vec<_> = chain.view_outcomes().map(|(_, o)| o.clone()).collect();
        assert!(matches!(
            outcomes[2],
            Err(BatchIngestError::HashMismatch { .. })
        ));
        assert_eq!(chain.head_tip().unwrap().to_lez_block_id, 4);

        // Finality reaches entry 2 before the finalized LEZ blocks do: it waits (so does
        // anything after it), then the judge decides it once LEZ is read.
        oracle.read_to = 2;
        let waiting = chain.apply_finalized(entry(2, 1, Some(b2.clone())), 100, Some(&oracle));
        assert_eq!(
            waiting,
            Some(AcceptOutcome::RetryableFailure(
                BatchIngestError::LezNotRead { to: 4 }
            ))
        );
        assert_eq!(chain.final_tip().unwrap().to_lez_block_id, 2);
        assert!(chain.final_entries()[1].is_waiting());
        assert!(chain.stall().is_none());
        oracle.read_to = 6;
        assert_eq!(chain.retry_folds(Some(&oracle)), 1);
        assert_eq!(chain.final_tip().unwrap().to_lez_block_id, 4);
        assert!(!chain.final_entries()[1].is_waiting());
        // Entry 3 parks the final tier, 4 counts as an orphan.
        let parked = chain.apply_finalized(entry(3, 2, Some(wrong)), 200, Some(&oracle));
        assert!(matches!(
            parked,
            Some(AcceptOutcome::Parked(BatchIngestError::HashMismatch { .. }))
        ));
        let stall = chain.stall().unwrap();
        assert_eq!(stall.msg, [3; 32]);
        assert_eq!(stall.l1_slot, 200);
        let b4 = Batch {
            from_lez_block_id: 6,
            to_lez_block_id: 6,
            changes: vec![],
            root_after: root_2.to_vec(),
        };
        let later = chain.apply_finalized(entry(4, 3, Some(b4)), 300, Some(&oracle));
        assert!(matches!(later, Some(AcceptOutcome::Parked(_))));
        assert_eq!(chain.stall().unwrap().orphans_since, 1);
        assert_eq!(chain.final_tip().unwrap().to_lez_block_id, 4);
        assert_eq!(chain.final_entries().len(), 4);
        assert!(chain.final_entries()[2].error.is_some());

        // A derivation mismatch: the batch replays fine but LEZ says otherwise.
        let mut fresh = ChainState::new(1);
        let liar = Oracle {
            roots: vec![(2, [5; 32])],
            read_to: 2,
        };
        let outcome = fresh.apply_finalized(entry(1, 0, Some(b1)), 100, Some(&liar));
        assert!(matches!(
            outcome,
            Some(AcceptOutcome::Parked(
                BatchIngestError::DerivationMismatch { .. }
            ))
        ));
    }

    #[test]
    fn test_an_undecodable_inscription_and_a_gap_are_held_not_fatal() {
        let mut chain = ChainState::new(1);
        let empty = ReplayTree::empty();
        let b1 = batch(1, 1, vec![Change::insert(0, leaf(1))], &empty);
        // Garbage on the channel: an entry without a batch, skipped, the lineage goes on.
        chain.apply_extension(
            vec![ChannelEntry::new(msg(1), MsgId::root(), &[0xff, 0xff])],
            None,
        );
        assert!(chain.view()[0].batch.is_none());
        assert_eq!(chain.head_tip(), None);
        // The next batch chains on the garbage entry's msg and applies on the empty tip.
        chain.apply_extension(vec![entry(2, 1, Some(b1.clone()))], None);
        assert_eq!(chain.head_tip().unwrap().to_lez_block_id, 1);
        // An entry the SDK reports before its parent: a gap in the lineage, visible, held.
        chain.apply_extension(vec![entry(4, 3, None)], None);
        assert_eq!(chain.check_for_gaps_in_lineage().unwrap().msg, msg(4));
        // A short leaf does not apply: the fold says so, nothing breaks.
        let bad = Batch {
            from_lez_block_id: 2,
            to_lez_block_id: 2,
            changes: vec![Change::insert(1, vec![1, 2, 3])],
            root_after: b1.root_after,
        };
        chain.apply_extension(vec![entry(5, 4, Some(bad))], None);
        let last = chain.view_outcomes().last().unwrap().1.clone();
        assert!(matches!(last, Err(BatchIngestError::Deserialize(_))));
        // Finalizing the garbage entry alone: no batch, no outcome, the final msg moves.
        let mut chain = ChainState::new(1);
        assert_eq!(
            chain.apply_finalized(ChannelEntry::new(msg(1), MsgId::root(), &[]), 10, None),
            None
        );
        assert_eq!(chain.final_msg(), msg(1));
    }

    /// The first batch starts at the program's deployment block, not at LEZ block 1 (the
    /// testnet program: 49431); one starting anywhere else parks.
    #[test]
    fn test_the_first_batch_starts_at_the_first_lez_block() {
        let mut chain = ChainState::new(49_431);
        let empty = ReplayTree::empty();
        let b1 = batch(49_431, 49_500, vec![Change::insert(0, leaf(1))], &empty);
        assert_eq!(
            chain.record_publish(entry(1, 0, Some(b1.clone())), None),
            PublishRecord::Extended
        );
        assert_eq!(chain.head_tip().unwrap().to_lez_block_id, 49_500);
        // The same batch again, chained on the first: it does not continue the tip.
        assert_eq!(
            chain.record_publish(entry(2, 1, Some(b1.clone())), None),
            PublishRecord::Skipped
        );
        assert!(matches!(
            chain.view_outcomes().nth(1).unwrap().1,
            Err(BatchIngestError::UnexpectedBlockId {
                expected: 49_501,
                got: 49_431
            })
        ));
        // A chain that expects block 1 parks the testnet's first batch.
        let mut from_one = ChainState::new(1);
        assert_eq!(
            from_one.record_publish(entry(1, 0, Some(b1)), None),
            PublishRecord::Skipped
        );
    }
}
