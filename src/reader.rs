//! A reader (RFC "Derivation Rules"): holds the tree and applies the rules to the registration
//! program's events of every LEZ block. The Zone sequencer is a reader; so is every Zone
//! indexer, which is why they hold the same roots. The tree is a sled-backed Zerokit
//! `PmTree`; the reader's state (memberships, counters, the block it is at, the root after
//! every block that changed the tree) lives in `reader.json` beside it, written when it
//! changed, so a restart opens both and continues.
//!
//! Rules, per event:
//! - `Registered` inserts `poseidon(id_commitment, rate_limit)` at `leaf_index`, with the
//!   membership's expiry (the end of its grace period: a membership in its grace period is
//!   still in the tree and still proves). A `Registered` for a commitment whose membership is
//!   not over (the program forbids it today) replaces it: the old leaf leaves the tree.
//! - `Extended` keeps the new expiry; the tree does not change, unless the reader had already
//!   expired the leaf (the chain accepts a clock claim a little behind its clock), in which
//!   case the leaf goes back where it was.
//! - `Slashed` and `Erased` end the membership and remove its leaf if it is still active
//!   (`Erase` needs an expired membership, so the expiry step has normally removed it).
//! - After a block's events, every active leaf whose expiry is at or before the block's
//!   timestamp is removed, in leaf order.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use anyhow::{Context as _, Result, anyhow, bail};
use rln::prelude::{
    CanonicalDeserialize, CanonicalSerialize, Fr, Hasher, PmTree, PmTreeSledConfig, PoseidonHash,
    SledDB,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;
use zerokit_utils::merkle_tree::{
    OptimalMerkleConfig, OptimalMerkleTree, ZerokitMerkleProof, ZerokitMerkleTree,
};

use crate::{
    batch::Change,
    lez::{Block, Erased, Extended, ProgramEvent, Registered, RlnEvent, Slashed},
};

/// The tree depth, the one Zerokit ships circuits for.
pub const TREE_DEPTH: usize = 20;

/// Why a span of LEZ blocks does not continue the reader's chain (the two chain checks of
/// LEZ's `BlockIngestError`, same names).
#[derive(Debug, thiserror::Error)]
pub enum BlockIngestError {
    /// A block id out of sequence: the answer is out of order or has a gap.
    #[error("Unexpected block id: expected {expected}, got {got}")]
    UnexpectedBlockId { expected: u64, got: u64 },
    /// The block does not chain on the block the reader applied before it: another lineage.
    #[error(
        "Broken chain link at LEZ block {block_id}: expected prev {}, got prev {}",
        short(expected_prev),
        short(got_prev)
    )]
    BrokenChainLink {
        block_id: u64,
        expected_prev: [u8; 32],
        got_prev: [u8; 32],
    },
}

/// The first four bytes of a hash in hex with `..`, for logs: `ab12cd34..`.
pub fn short(bytes: &[u8; 32]) -> String {
    format!("{}..", hex::encode(&bytes[..4]))
}

/// 32 bytes from hex (`0x` allowed); `what` names the value in the error.
pub fn parse_32(hex_str: &str, what: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_str.trim().trim_start_matches("0x"))
        .with_context(|| format!("{what} is not hex"))?;
    <[u8; 32]>::try_from(bytes).map_err(|_| anyhow!("{what} must be 32 bytes"))
}

/// The root of the empty tree, what every registry starts from.
pub static EMPTY_ROOT: LazyLock<[u8; 32]> = LazyLock::new(|| {
    let tree = OptimalMerkleTree::<PoseidonHash>::new(
        TREE_DEPTH,
        Fr::default(),
        OptimalMerkleConfig::default(),
    )
    .expect("an in-memory tree of depth 20");
    fr_to_bytes_le(&tree.root())
});

/// 32-byte little-endian representation of a field element (RLN-API Appendix B form).
pub fn fr_to_bytes_le(fr: &Fr) -> [u8; 32] {
    let mut out = [0u8; 32];
    fr.serialize_compressed(&mut out[..])
        .expect("an Fr is 32 bytes");
    out
}

/// The field element 32 little-endian bytes encode, if they are below the modulus.
pub fn fr_from_bytes_le(bytes: &[u8; 32]) -> Result<Fr> {
    Fr::deserialize_compressed(&bytes[..]).context("not a field element")
}

/// Why a leaf left the tree.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Removed {
    /// Its expiry passed on the LEZ clock; the membership itself is not over (an `Extend`
    /// may still put it back).
    Expired,
    Slashed,
    Erased,
}

/// A membership as the reader tracks it, active or not.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Member {
    pub leaf_index: u64,
    /// Messages per epoch the membership bought (`user_message_limit` in RLN).
    #[serde(default)]
    pub rate_limit: u64,
    pub expiry_timestamp_ms: u64,
    /// The leaf value, `poseidon(id_commitment, rate_limit)`.
    #[serde(with = "hex::serde")]
    pub rate_commitment: [u8; 32],
    /// Whether its leaf is in the tree (it proves while it is).
    pub active: bool,
    /// Why the leaf is not in the tree, when it is not.
    pub removed: Option<Removed>,
    /// The LEZ block of the `Registered` that made it, and the LEZ transaction.
    pub registered_at_lez_block_id: u64,
    #[serde(with = "hex::serde")]
    pub registered_lez_tx_hash: [u8; 32],
    /// The LEZ block whose event or expiry step took the leaf out, while it is out, and the
    /// LEZ transaction when an event did (the expiry step has none).
    pub removed_at_lez_block_id: Option<u64>,
    #[serde(with = "hex_option")]
    pub removed_lez_tx_hash: Option<[u8; 32]>,
}

/// `Option<[u8; 32]>` as hex or `null`.
mod hex_option {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<[u8; 32]>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(bytes) => s.serialize_some(&hex::encode(bytes)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<[u8; 32]>, D::Error> {
        let text: Option<String> = Option::deserialize(d)?;
        text.map(|t| super::parse_32(&t, "hash").map_err(serde::de::Error::custom))
            .transpose()
    }
}

impl Member {
    /// Slashed or erased: over for good; a later `Registered` of the same commitment is a
    /// new membership.
    fn ended(&self) -> bool {
        matches!(self.removed, Some(Removed::Slashed | Removed::Erased))
    }
}

/// Counts for monitoring: the program's events read from LEZ (one per successful
/// instruction), and what the rules did to the tree beyond the one insert per `Registered`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub registered: u64,
    pub extended: u64,
    pub slashed: u64,
    pub erased: u64,
    /// Leaves removed because their expiry passed on the LEZ clock (no transaction on LEZ).
    pub removed_expired: u64,
    /// Leaves removed by a `Slashed` or an `Erased` while active.
    pub removed_ended: u64,
    /// Leaves put back by an `Extended` that landed after the reader had expired them.
    pub reinserted: u64,
    /// Privacy-preserving transactions with a public effect under the registration program:
    /// they carry no instruction and emit no event, so each changed the registry in a way
    /// this tree does not have.
    pub private_transaction_count: u64,
}

impl Stats {
    /// The program's events applied, by name.
    pub fn lez_events(&self) -> Value {
        serde_json::json!({
            "registered": self.registered,
            "extended": self.extended,
            "slashed": self.slashed,
            "erased": self.erased,
        })
    }

    /// What the rules did to the tree besides the inserts of `Registered`.
    pub fn tree_changes(&self) -> Value {
        serde_json::json!({
            "removed_expired": self.removed_expired,
            "removed_ended": self.removed_ended,
            "reinserted": self.reinserted,
        })
    }
}

/// A Merkle proof path at one root, in RLN-API form.
pub struct ProofPath {
    pub rate_commitment: [u8; 32],
    pub path_elements: Vec<[u8; 32]>,
    pub identity_path_index: Vec<u8>,
}

/// The reader's state beside the tree (`reader.json`): where it is, what it counted, every
/// membership, the root after every block that changed the tree. Small (bytes per member and
/// per block that changed the tree), rewritten when a block was applied.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ReaderState {
    tree_id: String,
    lez_block_id: u64,
    /// Hash of the last LEZ block applied, hex; the next block must chain on it.
    last_block_hash: Option<String>,
    lez_timestamp_ms: u64,
    stats: Stats,
    /// By `id_commitment`, hex.
    members: HashMap<String, Member>,
    /// The root after every LEZ block that changed the tree, by block id (as a string, JSON
    /// keys), root hex.
    roots_by_block: BTreeMap<String, String>,
}

/// The tree store under a data dir.
pub const TREE_DIR: &str = "tree";
/// The reader's state file under a data dir.
pub const READER_FILE: &str = "reader.json";

pub struct Reader {
    /// The registry this reader follows, stamped on the state so another registry's
    /// sequencer cannot open it.
    tree_id: [u8; 32],
    tree: PmTree<SledDB, PoseidonHash>,
    /// Memberships by identity commitment, the latest per commitment.
    members: HashMap<[u8; 32], Member>,
    pub stats: Stats,
    /// Last LEZ `block_id` applied.
    pub lez_block_id: u64,
    /// Hash of the last block applied: the next block must chain on it. `None` before any.
    last_block_hash: Option<[u8; 32]>,
    /// `timestamp_ms` of the last LEZ block applied: the clock this reader's tree is at.
    pub lez_timestamp_ms: u64,
    /// The root after every LEZ block that changed the tree: what the tree was at any block
    /// (`root_at`), to check a batch's root against.
    roots_by_block: BTreeMap<u64, [u8; 32]>,
    /// `reader.json`; `None` for a temporary reader, which persists nothing.
    state_path: Option<PathBuf>,
    /// The bytes `persist` wrote last, so an unchanged state is not written again.
    written: Vec<u8>,
}

impl Reader {
    /// Open the tree store `data_dir/tree` and the reader's state `data_dir/reader.json`
    /// (both created when missing). `lez_block_id` is where a fresh reader starts; a store
    /// that already holds a reader keeps its own. A tree without a state file is refused: it
    /// was written by another layout (or the file was lost), start with a fresh data dir.
    pub fn open(data_dir: &Path, tree_id: [u8; 32], lez_block_id: u64) -> Result<Self> {
        let dir = data_dir.join(TREE_DIR);
        let state_path = data_dir.join(READER_FILE);
        if dir.exists() != state_path.exists() {
            bail!(
                "{} holds {} but not {}: another layout, or a file was lost; start with a fresh \
                 DATA_DIR",
                data_dir.display(),
                if dir.exists() { TREE_DIR } else { READER_FILE },
                if dir.exists() { READER_FILE } else { TREE_DIR }
            );
        }
        let config = PmTreeSledConfig::new()
            .path(dir.clone())
            .temporary(false)
            .build()
            .with_context(|| format!("tree store at {}", dir.display()))?;
        let mut reader = Self::open_with(config, tree_id, lez_block_id)?;
        if state_path.exists() {
            let bytes =
                fs::read(&state_path).with_context(|| format!("read {}", state_path.display()))?;
            reader.restore(&bytes, &state_path)?;
            reader.written = bytes;
        }
        reader.state_path = Some(state_path);
        Ok(reader)
    }

    /// A throwaway store (in a temporary directory sled removes when closed), no state file.
    pub fn open_temporary(tree_id: [u8; 32], lez_block_id: u64) -> Result<Self> {
        let config = PmTreeSledConfig::new().temporary(true).build()?;
        Self::open_with(config, tree_id, lez_block_id)
    }

    /// A clone of this reader in a temporary store: the same tree (every leaf the store holds,
    /// so a store a block ahead of the state after a crash is cloned as it is and both take
    /// that block again), memberships, counters, block, hash and root history. The head of a
    /// `LezChain` starts from this and folds the adopted blocks over it.
    pub fn clone_temporary(&self) -> Result<Self> {
        let mut clone = Self::open_temporary(self.tree_id, self.lez_block_id)?;
        for index in 0..self.tree.leaves_set() {
            let leaf = self
                .tree
                .get(index)
                .with_context(|| format!("read leaf {index}"))?;
            if leaf != Fr::default() {
                clone
                    .tree
                    .set(index, leaf)
                    .with_context(|| format!("set leaf {index}"))?;
            }
        }
        clone.members = self.members.clone();
        clone.stats = self.stats;
        clone.last_block_hash = self.last_block_hash;
        clone.lez_timestamp_ms = self.lez_timestamp_ms;
        clone.roots_by_block = self.roots_by_block.clone();
        if clone.root() != self.root() {
            bail!(
                "the cloned tree's root {} is not the store's {}",
                hex::encode(clone.root()),
                hex::encode(self.root())
            );
        }
        Ok(clone)
    }

    fn open_with(config: PmTreeSledConfig, tree_id: [u8; 32], lez_block_id: u64) -> Result<Self> {
        let tree = <PmTree<SledDB, PoseidonHash> as ZerokitMerkleTree>::new(
            TREE_DEPTH,
            Fr::default(),
            config,
        )
        .context("open the tree store")?;
        Ok(Self {
            tree_id,
            tree,
            members: HashMap::new(),
            stats: Stats::default(),
            lez_block_id,
            last_block_hash: None,
            lez_timestamp_ms: 0,
            roots_by_block: BTreeMap::new(),
            state_path: None,
            written: vec![],
        })
    }

    /// The state `persist` wrote, read back.
    fn restore(&mut self, bytes: &[u8], path: &Path) -> Result<()> {
        let state: ReaderState =
            serde_json::from_slice(bytes).with_context(|| format!("{}", path.display()))?;
        if state.tree_id != hex::encode(self.tree_id) {
            bail!(
                "{} follows registry {}, not {}",
                path.display(),
                state.tree_id,
                hex::encode(self.tree_id)
            );
        }
        for (id, member) in &state.members {
            self.members
                .insert(parse_32(id, "member id_commitment")?, *member);
        }
        self.stats = state.stats;
        self.lez_block_id = state.lez_block_id;
        self.last_block_hash = state
            .last_block_hash
            .as_deref()
            .map(|hash| parse_32(hash, "last_block_hash"))
            .transpose()?;
        self.lez_timestamp_ms = state.lez_timestamp_ms;
        for (block_id, root) in &state.roots_by_block {
            let block_id: u64 = block_id.parse().context("root history block id")?;
            self.roots_by_block
                .insert(block_id, parse_32(root, "root history root")?);
        }
        Ok(())
    }

    /// Write the reader's state to its file, when it changed since the last write. The tree
    /// itself is in the store already (sled writes through). A temporary reader writes nothing.
    pub fn persist(&mut self) -> Result<()> {
        let Some(path) = &self.state_path else {
            return Ok(());
        };
        let state = ReaderState {
            tree_id: hex::encode(self.tree_id),
            lez_block_id: self.lez_block_id,
            last_block_hash: self.last_block_hash.map(hex::encode),
            lez_timestamp_ms: self.lez_timestamp_ms,
            stats: self.stats,
            members: self
                .members
                .iter()
                .map(|(id, m)| (hex::encode(id), *m))
                .collect(),
            roots_by_block: self
                .roots_by_block
                .iter()
                .map(|(block_id, root)| (block_id.to_string(), hex::encode(root)))
                .collect(),
        };
        let bytes = serde_json::to_vec(&state)?;
        if bytes != self.written {
            write_atomic(path, &bytes)?;
            self.written = bytes;
        }
        Ok(())
    }

    /// Flush the store to disk.
    pub fn close(&mut self) -> Result<()> {
        self.tree.close().context("close the tree store")
    }

    pub fn root(&self) -> [u8; 32] {
        fr_to_bytes_le(&self.tree.root())
    }

    pub fn active_count(&self) -> usize {
        self.members.values().filter(|m| m.active).count()
    }

    /// Active leaves by `leaf_index`: `(id_commitment, member)`.
    pub fn active_leaves(&self) -> Vec<([u8; 32], Member)> {
        self.leaves(|m| m.active)
    }

    /// Memberships whose leaf is not in the tree, by `leaf_index`: `(id_commitment, member)`.
    pub fn removed_leaves(&self) -> Vec<([u8; 32], Member)> {
        self.leaves(|m| m.removed.is_some())
    }

    fn leaves(&self, keep: impl Fn(&Member) -> bool) -> Vec<([u8; 32], Member)> {
        let mut leaves: Vec<_> = self
            .members
            .iter()
            .filter(|(_, m)| keep(m))
            .map(|(id, m)| (*id, *m))
            .collect();
        leaves.sort_by_key(|(_, m)| m.leaf_index);
        leaves
    }

    /// Check that `blocks` continue this reader's chain: ids consecutive from the last block
    /// applied and each `prev_block_hash` the hash of the block before. A gap, a block out of
    /// order or another lineage fails here, before anything is applied.
    pub fn check_chain(&self, blocks: &[Block]) -> Result<(), BlockIngestError> {
        let mut expected_prev = self.last_block_hash;
        for (expected, block) in (self.lez_block_id + 1..).zip(blocks) {
            if block.block_id() != expected {
                return Err(BlockIngestError::UnexpectedBlockId {
                    expected,
                    got: block.block_id(),
                });
            }
            if let Some(prev) = expected_prev
                && block.prev_block_hash() != prev
            {
                return Err(BlockIngestError::BrokenChainLink {
                    block_id: block.block_id(),
                    expected_prev: prev,
                    got_prev: block.prev_block_hash(),
                });
            }
            expected_prev = Some(block.hash());
        }
        Ok(())
    }

    /// The hash of the last block applied, `None` before any.
    pub fn last_block_hash(&self) -> Option<[u8; 32]> {
        self.last_block_hash
    }

    /// Apply the derivation rules to the events of one LEZ block and return the changes, in
    /// order.
    pub fn apply_block(&mut self, block: &Block) -> Result<Vec<Change>> {
        let mut changes = vec![];
        self.stats.private_transaction_count += u64::from(block.private_transaction_count);
        for ProgramEvent { tx_hash, event } in &block.events {
            match event {
                RlnEvent::Registered(Registered {
                    tree_id: _,
                    id_commitment,
                    rate_limit,
                    expiry_timestamp_ms,
                    leaf_index,
                }) => {
                    // The program's `Registered` events of a registry carry 0, 1, 2.. in block
                    // order (its `events` module); a jump means a block this reader missed or
                    // a program change. Said by each reader that applies the block (the head,
                    // then the final reader).
                    if *leaf_index != self.stats.registered {
                        warn!(
                            "Registered carries leaf_index {leaf_index} after {} Registered \
                             applied: following the event",
                            self.stats.registered
                        );
                    }
                    self.stats.registered += 1;
                    // The program forbids a second registration of an open membership; should
                    // one come, the new leaf replaces the old one, which leaves the tree so
                    // the tree holds exactly the active leaves.
                    if let Some(old) = self.members.get(id_commitment).filter(|m| !m.ended()) {
                        warn!(
                            "Registered {} again while its membership is not over; the new \
                             leaf {leaf_index} replaces leaf {}",
                            short(id_commitment),
                            old.leaf_index
                        );
                        if old.active && old.leaf_index != *leaf_index {
                            let old_leaf_index = old.leaf_index;
                            self.tree
                                .delete(old_leaf_index as usize)
                                .with_context(|| format!("delete leaf {old_leaf_index}"))?;
                            changes.push(Change::remove(old_leaf_index));
                        }
                    }
                    let leaf = rate_commitment(id_commitment, *rate_limit)?;
                    self.tree
                        .set(*leaf_index as usize, leaf)
                        .with_context(|| format!("set leaf {leaf_index}"))?;
                    self.members.insert(
                        *id_commitment,
                        Member {
                            leaf_index: *leaf_index,
                            rate_limit: *rate_limit,
                            expiry_timestamp_ms: *expiry_timestamp_ms,
                            rate_commitment: fr_to_bytes_le(&leaf),
                            active: true,
                            removed: None,
                            registered_at_lez_block_id: block.block_id(),
                            registered_lez_tx_hash: *tx_hash,
                            removed_at_lez_block_id: None,
                            removed_lez_tx_hash: None,
                        },
                    );
                    changes.push(Change::insert(*leaf_index, fr_to_bytes_le(&leaf).to_vec()));
                }
                RlnEvent::Extended(Extended {
                    tree_id: _,
                    id_commitment,
                    expiry_timestamp_ms,
                }) => {
                    self.stats.extended += 1;
                    let Some(member) = self.members.get_mut(id_commitment) else {
                        warn!(
                            "Extended {} without a registration; ignored",
                            short(id_commitment)
                        );
                        continue;
                    };
                    if member.ended() {
                        warn!(
                            "Extended {} after its membership ended; ignored",
                            short(id_commitment)
                        );
                        continue;
                    }
                    member.expiry_timestamp_ms = *expiry_timestamp_ms;
                    if !member.active {
                        // Expired by this reader, extended on chain: back where it was.
                        let leaf =
                            fr_from_bytes_le(&member.rate_commitment).context("stored leaf")?;
                        let leaf_index = member.leaf_index;
                        member.active = true;
                        member.removed = None;
                        member.removed_at_lez_block_id = None;
                        member.removed_lez_tx_hash = None;
                        self.stats.reinserted += 1;
                        self.tree
                            .set(leaf_index as usize, leaf)
                            .with_context(|| format!("set leaf {leaf_index} again"))?;
                        changes.push(Change::insert(leaf_index, member.rate_commitment.to_vec()));
                    }
                }
                RlnEvent::Erased(Erased { id_commitment, .. }) => {
                    self.stats.erased += 1;
                    changes.extend(self.end(
                        id_commitment,
                        Removed::Erased,
                        block.block_id(),
                        *tx_hash,
                    )?);
                }
                RlnEvent::Slashed(Slashed { id_commitment, .. }) => {
                    self.stats.slashed += 1;
                    changes.extend(self.end(
                        id_commitment,
                        Removed::Slashed,
                        block.block_id(),
                        *tx_hash,
                    )?);
                }
            }
        }
        // The expiry step: every active leaf expired at the block's timestamp, in leaf order.
        let mut expired: Vec<_> = self
            .members
            .iter()
            .filter(|(_, m)| m.active && block.timestamp_ms() >= m.expiry_timestamp_ms)
            .map(|(id, m)| (m.leaf_index, *id))
            .collect();
        expired.sort();
        for (_, id) in expired {
            let member = self.members.get_mut(&id).expect("listed above");
            member.active = false;
            member.removed = Some(Removed::Expired);
            member.removed_at_lez_block_id = Some(block.block_id());
            member.removed_lez_tx_hash = None;
            self.stats.removed_expired += 1;
            let leaf_index = member.leaf_index;
            self.tree
                .delete(leaf_index as usize)
                .with_context(|| format!("delete leaf {leaf_index}"))?;
            changes.push(Change::remove(leaf_index));
        }
        self.lez_block_id = block.block_id();
        self.last_block_hash = Some(block.hash());
        self.lez_timestamp_ms = block.timestamp_ms();
        if !changes.is_empty() {
            self.roots_by_block.insert(block.block_id(), self.root());
        }
        Ok(changes)
    }

    /// The tree's root right after LEZ block `block_id`: the root after the last block up to it
    /// that changed the tree, the empty tree's before any. `None` past the block applied last.
    pub fn root_at(&self, block_id: u64) -> Option<[u8; 32]> {
        if block_id > self.lez_block_id {
            return None;
        }
        Some(
            self.roots_by_block
                .range(..=block_id)
                .next_back()
                .map_or(*EMPTY_ROOT, |(_, root)| *root),
        )
    }

    /// Slashed or erased: the membership is over; its leaf leaves the tree if still there.
    fn end(
        &mut self,
        id_commitment: &[u8; 32],
        cause: Removed,
        block_id: u64,
        tx_hash: [u8; 32],
    ) -> Result<Option<Change>> {
        let Some(member) = self.members.get_mut(id_commitment) else {
            warn!(
                "{cause:?} {} without a registration; ignored",
                short(id_commitment)
            );
            return Ok(None);
        };
        if member.ended() {
            return Ok(None);
        }
        let was_active = member.active;
        member.active = false;
        member.removed = Some(cause);
        member.removed_at_lez_block_id = Some(block_id);
        member.removed_lez_tx_hash = Some(tx_hash);
        if !was_active {
            return Ok(None);
        }
        self.stats.removed_ended += 1;
        let leaf_index = member.leaf_index;
        self.tree
            .delete(leaf_index as usize)
            .with_context(|| format!("delete leaf {leaf_index}"))?;
        Ok(Some(Change::remove(leaf_index)))
    }

    /// The Merkle proof path of the leaf at `leaf_index` at this reader's root, if the leaf is
    /// set. The API serves paths at the newest valid root instead (`ReplayTree::proof_path`).
    #[cfg(test)]
    pub fn proof_path(&self, leaf_index: u64) -> Option<ProofPath> {
        proof_path_at(&self.tree, leaf_index)
    }

    /// The membership of an identity commitment, active or not.
    pub fn member(&self, id_commitment: &[u8; 32]) -> Option<Member> {
        self.members.get(id_commitment).copied()
    }

    /// The membership at a leaf index, active or not: `(id_commitment, member)`.
    pub fn member_at(&self, leaf_index: u64) -> Option<([u8; 32], Member)> {
        self.members
            .iter()
            .find(|(_, m)| m.leaf_index == leaf_index)
            .map(|(id, m)| (*id, *m))
    }
}

/// Write `bytes` to `path` through a temporary file renamed over it, so a crash leaves either
/// the old file or the new one, never a torn one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} over {}", tmp.display(), path.display()))
}

/// The Merkle proof path of the leaf at `leaf_index` in `tree`, if the leaf is set: the sibling
/// elements from the leaf up, and the side of each (RLN-API form).
pub fn proof_path_at<T>(tree: &T, leaf_index: u64) -> Option<ProofPath>
where
    T: ZerokitMerkleTree<Hasher = PoseidonHash>,
    T::Proof: ZerokitMerkleProof<Hasher = PoseidonHash, Index = u8>,
{
    let index = usize::try_from(leaf_index).ok()?;
    let leaf = tree.get(index).ok()?;
    if leaf == Fr::default() {
        return None;
    }
    let proof = tree.proof(index).ok()?;
    Some(ProofPath {
        rate_commitment: fr_to_bytes_le(&leaf),
        path_elements: proof
            .get_path_elements()
            .iter()
            .map(fr_to_bytes_le)
            .collect(),
        identity_path_index: proof.get_path_index(),
    })
}

/// `poseidon(id_commitment, rate_limit)`, the leaf of a membership.
fn rate_commitment(id_commitment: &[u8; 32], rate_limit: u64) -> Result<Fr> {
    let id_commitment = fr_from_bytes_le(id_commitment).context("id_commitment")?;
    Ok(Hasher::<PoseidonHash>::hash_pair(
        id_commitment,
        Fr::from(rate_limit),
    ))
}

#[cfg(test)]
mod test {
    use rln::prelude::{
        IdentityKeys, RLNBuilder, RLNMerkleProof, RLNWitnessInput, default_graph_single,
        default_zkey_single, hash_to_field_le,
    };

    use super::*;

    fn id(n: u8) -> [u8; 32] {
        let mut id_commitment = [0u8; 32];
        id_commitment[0] = n;
        id_commitment
    }

    fn registered(n: u8, leaf_index: u64, expiry_timestamp_ms: u64) -> ProgramEvent {
        ProgramEvent::from(RlnEvent::Registered(Registered {
            tree_id: [0; 32],
            id_commitment: id(n),
            rate_limit: 100,
            expiry_timestamp_ms,
            leaf_index,
        }))
    }

    fn block(block_id: u64, timestamp_ms: u64, events: Vec<ProgramEvent>) -> Block {
        Block {
            header: crate::lez::BlockHeader {
                block_id,
                prev_block_hash: common::HashType([0; 32]),
                hash: common::HashType([0; 32]),
                timestamp: timestamp_ms,
            },
            events,
            private_transaction_count: 0,
        }
    }

    #[test]
    fn test_check_chain_refuses_gaps_disorder_and_a_changed_history() {
        let mut reader = Reader::open_temporary([0; 32], 0).unwrap();
        let chained = |id: u64, prev: u8, hash: u8| {
            let mut b = block(id, 1_000, vec![]);
            b.header.prev_block_hash = common::HashType([prev; 32]);
            b.header.hash = common::HashType([hash; 32]);
            b
        };
        // Nothing applied yet: the first block is taken on its id alone, the next must chain.
        let good = [chained(1, 0, 1), chained(2, 1, 2)];
        reader.check_chain(&good).unwrap();
        assert!(
            reader
                .check_chain(&[chained(2, 1, 2), chained(1, 0, 1)])
                .is_err()
        );
        assert!(
            reader
                .check_chain(&[chained(1, 0, 1), chained(3, 1, 3)])
                .is_err()
        );
        assert!(
            reader
                .check_chain(&[chained(1, 0, 1), chained(2, 9, 2)])
                .is_err()
        );
        for b in &good {
            reader.apply_block(b).unwrap();
        }
        // After block 2 (hash 2): block 3 must say so.
        reader.check_chain(&[chained(3, 2, 3)]).unwrap();
        assert!(matches!(
            reader.check_chain(&[chained(3, 1, 3)]),
            Err(BlockIngestError::BrokenChainLink { block_id: 3, .. })
        ));
        assert!(matches!(
            reader.check_chain(&[chained(2, 1, 2)]),
            Err(BlockIngestError::UnexpectedBlockId { got: 2, .. })
        ));
        assert_eq!(reader.last_block_hash(), Some([2; 32]));
    }

    /// A cloned reader is the store's tree in a temporary store: same root, memberships,
    /// history; applying to it leaves the store alone.
    #[test]
    fn test_clone_temporary_matches_the_store() {
        let mut reader = Reader::open_temporary([0; 32], 0).unwrap();
        reader
            .apply_block(&block(1, 1_000, vec![registered(1, 0, 5_000)]))
            .unwrap();
        reader
            .apply_block(&block(2, 2_000, vec![registered(2, 1, u64::MAX)]))
            .unwrap();
        let mut clone = reader.clone_temporary().unwrap();
        assert_eq!(clone.root(), reader.root());
        assert_eq!(clone.lez_block_id, 2);
        assert_eq!(clone.active_count(), 2);
        assert_eq!(clone.root_at(1), reader.root_at(1));
        assert_eq!(clone.last_block_hash(), reader.last_block_hash());
        // Block 5 expires leaf 0 on the clone only.
        clone.apply_block(&block(5, 5_000, vec![])).unwrap();
        assert_eq!(clone.active_count(), 1);
        assert_eq!(reader.active_count(), 2);
        assert_ne!(clone.root(), reader.root());
        assert!(clone.proof_path(1).is_some());
    }

    #[test]
    fn test_register_expire_and_proof_path() {
        let mut reader = Reader::open_temporary([0; 32], 0).unwrap();
        let empty = reader.root();
        let changes = reader
            .apply_block(&block(1, 1_000, vec![registered(1, 0, 5_000)]))
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_ne!(reader.root(), empty);
        let path = reader.proof_path(0).unwrap();
        assert_eq!(path.path_elements.len(), TREE_DEPTH);
        assert_eq!(path.identity_path_index.len(), TREE_DEPTH);
        assert!(reader.proof_path(1).is_none());
        // Expired at the block whose timestamp reaches the expiry: removed, root back to empty.
        let changes = reader.apply_block(&block(2, 5_000, vec![])).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(reader.root(), empty);
        assert!(reader.proof_path(0).is_none());
        assert_eq!(
            reader.member(&id(1)).and_then(|m| m.removed),
            Some(Removed::Expired)
        );
    }

    #[test]
    fn test_extend_after_expiry_puts_the_leaf_back_and_slash_ends_it() {
        let mut reader = Reader::open_temporary([0; 32], 0).unwrap();
        reader
            .apply_block(&block(1, 1_000, vec![registered(1, 0, 5_000)]))
            .unwrap();
        let with_leaf = reader.root();
        reader.apply_block(&block(2, 5_000, vec![])).unwrap();
        // The extension dated before the expiry lands late: the leaf comes back.
        let changes = reader
            .apply_block(&block(
                3,
                6_000,
                vec![
                    RlnEvent::Extended(Extended {
                        tree_id: [0; 32],
                        id_commitment: id(1),
                        expiry_timestamp_ms: 9_000,
                    })
                    .into(),
                ],
            ))
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(reader.root(), with_leaf);
        assert_eq!(reader.stats.reinserted, 1);
        // Slashed: removed for good; a later Extended changes nothing.
        let changes = reader
            .apply_block(&block(
                4,
                7_000,
                vec![
                    ProgramEvent {
                        tx_hash: [7; 32],
                        event: RlnEvent::Slashed(Slashed {
                            tree_id: [0; 32],
                            id_commitment: id(1),
                        }),
                    },
                    RlnEvent::Extended(Extended {
                        tree_id: [0; 32],
                        id_commitment: id(1),
                        expiry_timestamp_ms: 20_000,
                    })
                    .into(),
                ],
            ))
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            reader.member(&id(1)).and_then(|m| m.removed),
            Some(Removed::Slashed)
        );
        assert_eq!(
            reader.member(&id(1)).and_then(|m| m.removed_lez_tx_hash),
            Some([7; 32])
        );
        assert_eq!(reader.active_count(), 0);
    }

    #[test]
    fn test_reopen_continues_from_the_store() {
        let dir = std::env::temp_dir().join(format!("zone-sequencer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = {
            let mut reader = Reader::open(&dir, [0; 32], 0).unwrap();
            reader
                .apply_block(&block(
                    1,
                    1_000,
                    vec![registered(1, 0, 5_000), registered(2, 1, 9_000)],
                ))
                .unwrap();
            reader.apply_block(&block(2, 5_000, vec![])).unwrap();
            reader.persist().unwrap();
            let root = reader.root();
            reader.close().unwrap();
            root
        };
        {
            let reader = Reader::open(&dir, [0; 32], 0).unwrap();
            assert_eq!(reader.root(), root);
            assert_eq!(reader.lez_block_id, 2);
            assert_eq!(reader.active_count(), 1);
            assert_eq!(reader.removed_leaves().len(), 1);
            assert!(reader.proof_path(1).is_some());
        }
        // Another registry's reader refuses the store (sled holds one lock per store, so the
        // previous reader is dropped first).
        assert!(Reader::open(&dir, [9; 32], 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The tree (sled, written through) and the reader's state (`reader.json`, written by
    /// `persist`) are two writes: a crash between them leaves the tree a block ahead of the
    /// state. The next start applies that block again: the rules are idempotent on the tree
    /// (a leaf set twice, an empty leaf deleted), so the tree, the members and the root
    /// history come out as if the block had been applied once.
    #[test]
    fn test_a_block_applied_again_after_a_crash_leaves_the_same_tree() {
        let dir =
            std::env::temp_dir().join(format!("zone-reader-crash-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Block 2 registers leaf 1 and expires leaf 0 (registered in block 1 until 5_000).
        let block_2 = block(2, 6_000, vec![registered(2, 1, 9_000)]);
        let (root, members, roots) = {
            let mut reader = Reader::open(&dir, [0; 32], 0).unwrap();
            reader
                .apply_block(&block(1, 1_000, vec![registered(1, 0, 5_000)]))
                .unwrap();
            reader.persist().unwrap();
            // Applied, in the tree, but the crash comes before `persist`.
            reader.apply_block(&block_2).unwrap();
            let snapshot = (
                reader.root(),
                reader.active_leaves(),
                reader.roots_by_block.clone(),
            );
            reader.close().unwrap();
            snapshot
        };
        let mut reader = Reader::open(&dir, [0; 32], 0).unwrap();
        // The state is at block 1, the tree already at block 2.
        assert_eq!(reader.lez_block_id, 1);
        assert_eq!(reader.root(), root);
        assert_eq!(reader.active_count(), 1);
        reader.check_chain(std::slice::from_ref(&block_2)).unwrap();
        let changes = reader.apply_block(&block_2).unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(reader.root(), root);
        assert_eq!(reader.lez_block_id, 2);
        assert_eq!(reader.active_leaves().len(), 1);
        assert_eq!(
            reader.active_leaves()[0].1.leaf_index,
            members[0].1.leaf_index
        );
        assert_eq!(reader.roots_by_block, roots);
        assert_eq!(
            reader.member(&id(1)).unwrap().removed,
            Some(Removed::Expired)
        );
        assert!(reader.proof_path(0).is_none());
        assert!(reader.proof_path(1).is_some());
        reader.close().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second `Registered` for a membership that is not over replaces it: the old leaf leaves
    /// the tree (so the tree holds exactly the active leaves and a clone of the reader matches
    /// the store), the new one is the membership.
    #[test]
    fn test_registered_again_while_active_removes_the_old_leaf() {
        let mut reader = Reader::open_temporary([0; 32], 0).unwrap();
        reader
            .apply_block(&block(1, 1_000, vec![registered(1, 0, 9_000)]))
            .unwrap();
        let changes = reader
            .apply_block(&block(2, 2_000, vec![registered(1, 1, 9_000)]))
            .unwrap();
        let member = reader.member(&id(1)).unwrap();
        assert_eq!(
            changes,
            vec![
                Change::remove(0),
                Change::insert(1, member.rate_commitment.to_vec())
            ]
        );
        assert_eq!(reader.active_count(), 1);
        assert_eq!(member.leaf_index, 1);
        assert!(reader.proof_path(0).is_none());
        assert!(reader.proof_path(1).is_some());
        assert!(reader.clone_temporary().is_ok());
    }

    /// The proof path the reader serves proves under Zerokit: the path, its order and the
    /// little-endian form fit `RLNMerkleProof`, and the proof's root is the reader's.
    #[test]
    fn test_the_proof_path_proves_under_zerokit() {
        let keys =
            IdentityKeys::generate::<PoseidonHash, rand::rngs::ThreadRng>(&mut rand::thread_rng());
        let id_commitment = fr_to_bytes_le(&keys.id_commitment());
        let mut reader = Reader::open_temporary([0; 32], 0).unwrap();
        reader
            .apply_block(&block(
                1,
                1_000,
                vec![ProgramEvent::from(RlnEvent::Registered(Registered {
                    tree_id: [0; 32],
                    id_commitment,
                    rate_limit: 100,
                    expiry_timestamp_ms: 9_000,
                    leaf_index: 0,
                }))],
            ))
            .unwrap();
        let path = reader.proof_path(0).unwrap();
        let merkle_proof = RLNMerkleProof::new(
            path.path_elements
                .iter()
                .map(|e| fr_from_bytes_le(e).unwrap())
                .collect(),
            path.identity_path_index,
        );
        let x = hash_to_field_le(b"signal");
        let witness = RLNWitnessInput::new_single()
            .identity_secret(keys.identity_secret())
            .user_message_limit(Fr::from(100u64))
            .merkle_proof(merkle_proof)
            .x(x)
            .external_nullifier(hash_to_field_le(b"epoch"))
            .message_id(Fr::from(0u64))
            .build()
            .unwrap();
        let rln = RLNBuilder::stateless()
            .graph(default_graph_single().clone())
            .zkey(default_zkey_single().clone())
            .build();
        let (proof, values) = rln.generate_proof(&witness).unwrap();
        assert_eq!(fr_to_bytes_le(&values.root()), reader.root());
        assert!(
            rln.verify_with_roots(&proof, &values, &x, &[values.root()])
                .unwrap()
        );
    }

    /// Corners of the rules: a store missing one of its two files is refused; `Slashed` on an
    /// expired leaf records the cause and the transaction without a change; events for an
    /// unknown commitment change nothing; two leaves expiring in one block go in leaf order;
    /// `root_at` past the last block is `None`.
    #[test]
    fn test_reader_corners() {
        let dir = std::env::temp_dir().join(format!("zone-reader-corners-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut reader = Reader::open(&dir, [0; 32], 0).unwrap();
            reader.persist().unwrap();
            reader.close().unwrap();
        }
        std::fs::remove_file(dir.join(READER_FILE)).unwrap();
        assert!(Reader::open(&dir, [0; 32], 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(READER_FILE), b"{}").unwrap();
        assert!(Reader::open(&dir, [0; 32], 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);

        let mut reader = Reader::open_temporary([0; 32], 0).unwrap();
        reader
            .apply_block(&block(
                1,
                1_000,
                vec![registered(2, 1, 5_000), registered(1, 0, 5_000)],
            ))
            .unwrap();
        let expired = reader.apply_block(&block(2, 5_000, vec![])).unwrap();
        assert_eq!(expired, vec![Change::remove(0), Change::remove(1)]);
        let slashed = ProgramEvent {
            tx_hash: [7; 32],
            event: RlnEvent::Slashed(Slashed {
                tree_id: [0; 32],
                id_commitment: id(1),
            }),
        };
        let none = reader.apply_block(&block(3, 6_000, vec![slashed])).unwrap();
        assert!(none.is_empty());
        let member = reader.member(&id(1)).unwrap();
        assert_eq!(member.removed, Some(Removed::Slashed));
        assert_eq!(member.removed_lez_tx_hash, Some([7; 32]));
        assert_eq!(member.removed_at_lez_block_id, Some(3));
        assert_eq!(reader.stats.removed_ended, 0);
        let unknown = vec![
            ProgramEvent::from(RlnEvent::Extended(Extended {
                tree_id: [0; 32],
                id_commitment: id(9),
                expiry_timestamp_ms: 9_000,
            })),
            ProgramEvent::from(RlnEvent::Erased(Erased {
                tree_id: [0; 32],
                id_commitment: id(9),
            })),
        ];
        assert!(
            reader
                .apply_block(&block(4, 7_000, unknown))
                .unwrap()
                .is_empty()
        );
        assert_eq!((reader.stats.extended, reader.stats.erased), (1, 1));
        assert!(reader.member(&id(9)).is_none());
        assert_eq!(reader.root_at(4), Some(*EMPTY_ROOT));
        assert!(reader.root_at(5).is_none());
    }
}
