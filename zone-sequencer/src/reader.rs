//! A reader (RFC "Derivation Rules"): holds the tree and applies the rules to the registration
//! program's events of every LEZ block. The Zone sequencer is a reader; so is every Zone
//! indexer, which is why they hold the same roots. The tree is a sled-backed Zerokit
//! `PmTree`; the reader's state beside it (memberships, counters, the block it is at) and
//! whatever its owner wants kept with it live in the tree store's metadata, so a restart
//! opens the store and continues.
//!
//! Rules, per event:
//! - `Registered` inserts `poseidon(id_commitment, rate_limit)` at `leaf_index`, with the
//!   membership's expiry (the end of its grace period: a membership in its grace period is
//!   still in the tree and still proves).
//! - `Extended` keeps the new expiry; the tree does not change, unless the reader had already
//!   expired the leaf (the chain accepts a clock claim a little behind its clock), in which
//!   case the leaf goes back where it was.
//! - `Slashed` and `Erased` end the membership and remove its leaf if it is still active
//!   (`Erase` needs an expired membership, so the expiry step has normally removed it).
//! - After a block's events, every active leaf whose expiry is at or before the block's
//!   timestamp is removed, in leaf order.

use std::{collections::HashMap, path::Path};

use anyhow::{Context as _, Result, anyhow};
use rln::prelude::{
    CanonicalDeserialize, CanonicalSerialize, Fr, Hasher, PmTree, PmTreeSledConfig, PoseidonHash,
    SledDB,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zerokit_utils::merkle_tree::{ZerokitMerkleProof, ZerokitMerkleTree};

use crate::{
    batch::Change,
    lez::{Block, Event},
};

pub const TREE_DEPTH: usize = 20;

/// 32-byte little-endian representation of a field element (RLN-API Appendix B form).
pub fn fr_to_bytes_le(fr: &Fr) -> [u8; 32] {
    let mut out = [0u8; 32];
    fr.serialize_compressed(&mut out[..])
        .expect("an Fr is 32 bytes");
    out
}

/// The field element 32 little-endian bytes encode, if they are below the modulus.
pub fn fr_from_bytes_le(bytes: &[u8; 32]) -> Result<Fr, String> {
    Fr::deserialize_compressed(&bytes[..]).map_err(|e| format!("not a field element: {e}"))
}

/// Why a leaf left the tree.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Removed {
    /// Its expiry passed on the LEZ clock; the membership itself is not over (an `Extend`
    /// may still put it back).
    #[serde(alias = "Expired")]
    Expired,
    #[serde(alias = "Slashed")]
    Slashed,
    #[serde(alias = "Erased")]
    Erased,
}

/// A membership as the reader tracks it, active or not.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Member {
    pub leaf_index: u64,
    pub expiry_timestamp_ms: u64,
    /// The leaf value, `poseidon(id_commitment, rate_limit)`.
    #[serde(with = "hex::serde")]
    pub rate_commitment: [u8; 32],
    /// Whether its leaf is in the tree (it proves while it is).
    #[serde(alias = "live")]
    pub active: bool,
    /// Why the leaf is not in the tree, when it is not.
    pub removed: Option<Removed>,
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

/// What the tree store's metadata holds: the reader's state and its owner's `extra`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Meta {
    tree_id: String,
    applied_block_id: u64,
    applied_timestamp_ms: u64,
    stats: Stats,
    /// By `id_commitment`, hex.
    members: HashMap<String, Member>,
    #[serde(default)]
    extra: Value,
}

pub struct Reader {
    /// The registry this reader follows, stamped on the store so another registry's
    /// sequencer cannot open it.
    tree_id: [u8; 32],
    tree: PmTree<SledDB, PoseidonHash>,
    /// Memberships by identity commitment, the latest per commitment.
    members: HashMap<[u8; 32], Member>,
    pub stats: Stats,
    /// Last LEZ `block_id` applied.
    pub applied_block_id: u64,
    /// `timestamp_ms` of the last LEZ block applied: the clock this reader's tree is at.
    pub applied_timestamp_ms: u64,
}

impl Reader {
    /// Open the tree store at `dir` (created when missing). `applied_block_id` is where a
    /// fresh reader starts; a store that already holds a reader keeps its own. Returns the
    /// `extra` its owner persisted last.
    pub fn open(dir: &Path, tree_id: [u8; 32], applied_block_id: u64) -> Result<(Self, Value)> {
        let config = PmTreeSledConfig::new()
            .path(dir.to_path_buf())
            .temporary(false)
            .build()
            .with_context(|| format!("tree store at {}", dir.display()))?;
        Self::open_with(config, tree_id, applied_block_id)
    }

    /// Only the owner's `extra` of the store at `dir`, read off the raw metadata so a store
    /// written by an older layout still gives it up (for a rebuild).
    pub fn take_extra(dir: &Path) -> Result<Value> {
        let config = PmTreeSledConfig::new()
            .path(dir.to_path_buf())
            .temporary(false)
            .build()
            .with_context(|| format!("tree store at {}", dir.display()))?;
        let mut tree = <PmTree<SledDB, PoseidonHash> as ZerokitMerkleTree>::new(
            TREE_DEPTH,
            Fr::default(),
            config,
        )
        .context("open the tree store")?;
        let bytes = tree.metadata().unwrap_or_default();
        tree.close().context("close the tree store")?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        let meta: Value = serde_json::from_slice(&bytes).context("tree store metadata")?;
        Ok(meta.get("extra").cloned().unwrap_or(Value::Null))
    }

    /// A throwaway store, for tests.
    #[cfg(test)]
    pub fn open_temporary(tree_id: [u8; 32], applied_block_id: u64) -> Result<(Self, Value)> {
        let config = PmTreeSledConfig::new().temporary(true).build()?;
        Self::open_with(config, tree_id, applied_block_id)
    }

    fn open_with(
        config: PmTreeSledConfig,
        tree_id: [u8; 32],
        applied_block_id: u64,
    ) -> Result<(Self, Value)> {
        let tree = <PmTree<SledDB, PoseidonHash> as ZerokitMerkleTree>::new(
            TREE_DEPTH,
            Fr::default(),
            config,
        )
        .context("open the tree store")?;
        let mut reader = Self {
            tree_id,
            tree,
            members: HashMap::new(),
            stats: Stats::default(),
            applied_block_id,
            applied_timestamp_ms: 0,
        };
        let bytes = reader.tree.metadata().unwrap_or_default();
        if bytes.is_empty() {
            return Ok((reader, Value::Null));
        }
        let meta: Meta = serde_json::from_slice(&bytes).context("tree store metadata")?;
        if meta.tree_id != hex::encode(tree_id) {
            return Err(anyhow!(
                "the tree store follows registry {}, not {}",
                meta.tree_id,
                hex::encode(tree_id)
            ));
        }
        for (id, member) in &meta.members {
            reader.members.insert(parse_32(id)?, *member);
        }
        reader.stats = meta.stats;
        reader.applied_block_id = meta.applied_block_id;
        reader.applied_timestamp_ms = meta.applied_timestamp_ms;
        Ok((reader, meta.extra))
    }

    /// Write the reader's state and its owner's `extra` into the tree store.
    pub fn persist(&mut self, extra: Value) -> Result<()> {
        let meta = Meta {
            tree_id: hex::encode(self.tree_id),
            applied_block_id: self.applied_block_id,
            applied_timestamp_ms: self.applied_timestamp_ms,
            stats: self.stats,
            members: self
                .members
                .iter()
                .map(|(id, m)| (hex::encode(id), *m))
                .collect(),
            extra,
        };
        self.tree
            .set_metadata(&serde_json::to_vec(&meta)?)
            .context("write the tree store metadata")
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

    /// Active leaves by `leaf_index`: `(leaf_index, id_commitment, expiry_timestamp_ms)`.
    pub fn active_leaves(&self) -> Vec<(u64, [u8; 32], u64)> {
        let mut leaves: Vec<_> = self
            .members
            .iter()
            .filter(|(_, m)| m.active)
            .map(|(id, m)| (m.leaf_index, *id, m.expiry_timestamp_ms))
            .collect();
        leaves.sort();
        leaves
    }

    /// Memberships whose leaf is not in the tree, by `leaf_index`:
    /// `(leaf_index, id_commitment, cause)`.
    pub fn removed_leaves(&self) -> Vec<(u64, [u8; 32], Removed)> {
        let mut removed: Vec<_> = self
            .members
            .iter()
            .filter_map(|(id, m)| m.removed.map(|c| (m.leaf_index, *id, c)))
            .collect();
        removed.sort_by_key(|(leaf_index, _, _)| *leaf_index);
        removed
    }

    /// Apply the derivation rules to the events of one LEZ block and return the changes, in
    /// order.
    pub fn apply_block(&mut self, block: &Block) -> Result<Vec<Change>> {
        let mut changes = vec![];
        for event in &block.events {
            match event {
                Event::Registered {
                    id_commitment,
                    rate_limit,
                    expiry_timestamp_ms,
                    leaf_index,
                } => {
                    self.stats.registered += 1;
                    if self.members.get(id_commitment).is_some_and(|m| !m.ended()) {
                        println!(
                            "Registered {}.. again while its membership is not over; the new \
                             leaf {leaf_index} replaces it",
                            hex::encode(&id_commitment[..4])
                        );
                    }
                    let leaf = rate_commitment(id_commitment, *rate_limit)?;
                    self.tree
                        .set(*leaf_index as usize, leaf)
                        .with_context(|| format!("set leaf {leaf_index}"))?;
                    self.members.insert(
                        *id_commitment,
                        Member {
                            leaf_index: *leaf_index,
                            expiry_timestamp_ms: *expiry_timestamp_ms,
                            rate_commitment: fr_to_bytes_le(&leaf),
                            active: true,
                            removed: None,
                        },
                    );
                    changes.push(Change::insert(*leaf_index, fr_to_bytes_le(&leaf).to_vec()));
                }
                Event::Extended {
                    id_commitment,
                    expiry_timestamp_ms,
                } => {
                    self.stats.extended += 1;
                    let Some(member) = self.members.get_mut(id_commitment) else {
                        println!(
                            "Extended {}.. without a registration; ignored",
                            hex::encode(&id_commitment[..4])
                        );
                        continue;
                    };
                    if member.ended() {
                        println!(
                            "Extended {}.. after its membership ended; ignored",
                            hex::encode(&id_commitment[..4])
                        );
                        continue;
                    }
                    member.expiry_timestamp_ms = *expiry_timestamp_ms;
                    if !member.active {
                        // Expired by this reader, extended on chain: back where it was.
                        let leaf = fr_from_bytes_le(&member.rate_commitment)
                            .map_err(|e| anyhow!("stored leaf: {e}"))?;
                        let leaf_index = member.leaf_index;
                        member.active = true;
                        member.removed = None;
                        self.stats.reinserted += 1;
                        self.tree
                            .set(leaf_index as usize, leaf)
                            .with_context(|| format!("set leaf {leaf_index} again"))?;
                        changes.push(Change::insert(leaf_index, member.rate_commitment.to_vec()));
                    }
                }
                Event::Erased { id_commitment } => {
                    self.stats.erased += 1;
                    changes.extend(self.end(id_commitment, Removed::Erased)?);
                }
                Event::Slashed { id_commitment } => {
                    self.stats.slashed += 1;
                    changes.extend(self.end(id_commitment, Removed::Slashed)?);
                }
            }
        }
        // The expiry step: every active leaf expired at the block's timestamp, in leaf order.
        let mut expired: Vec<_> = self
            .members
            .iter()
            .filter(|(_, m)| m.active && block.timestamp_ms >= m.expiry_timestamp_ms)
            .map(|(id, m)| (m.leaf_index, *id))
            .collect();
        expired.sort();
        for (_, id) in expired {
            let member = self.members.get_mut(&id).expect("listed above");
            member.active = false;
            member.removed = Some(Removed::Expired);
            self.stats.removed_expired += 1;
            let leaf_index = member.leaf_index;
            self.tree
                .delete(leaf_index as usize)
                .with_context(|| format!("delete leaf {leaf_index}"))?;
            changes.push(Change::remove(leaf_index));
        }
        self.applied_block_id = block.block_id;
        self.applied_timestamp_ms = block.timestamp_ms;
        Ok(changes)
    }

    /// Slashed or erased: the membership is over; its leaf leaves the tree if still there.
    fn end(&mut self, id_commitment: &[u8; 32], cause: Removed) -> Result<Option<Change>> {
        let Some(member) = self.members.get_mut(id_commitment) else {
            println!(
                "{cause:?} {}.. without a registration; ignored",
                hex::encode(&id_commitment[..4])
            );
            return Ok(None);
        };
        if member.ended() {
            return Ok(None);
        }
        let was_active = member.active;
        member.active = false;
        member.removed = Some(cause);
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
    /// set: the sibling elements from the leaf up, and the side of each.
    pub fn proof_path(&self, leaf_index: u64) -> Option<ProofPath> {
        let index = usize::try_from(leaf_index).ok()?;
        let leaf = self.tree.get(index).ok()?;
        if leaf == Fr::default() {
            return None;
        }
        let proof = self.tree.proof(index).ok()?;
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

fn parse_32(hex_str: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_str).context("tree store metadata: hex")?;
    <[u8; 32]>::try_from(bytes).map_err(|_| anyhow!("tree store metadata: not 32 bytes"))
}

/// `poseidon(id_commitment, rate_limit)`, the leaf of a membership.
fn rate_commitment(id_commitment: &[u8; 32], rate_limit: u64) -> Result<Fr> {
    let id_commitment =
        fr_from_bytes_le(id_commitment).map_err(|e| anyhow!("id_commitment: {e}"))?;
    Ok(Hasher::<PoseidonHash>::hash_pair(
        id_commitment,
        Fr::from(rate_limit),
    ))
}

#[cfg(test)]
mod test {
    use super::*;

    fn id(n: u8) -> [u8; 32] {
        let mut id_commitment = [0u8; 32];
        id_commitment[0] = n;
        id_commitment
    }

    fn registered(n: u8, leaf_index: u64, expiry_timestamp_ms: u64) -> Event {
        Event::Registered {
            id_commitment: id(n),
            rate_limit: 100,
            expiry_timestamp_ms,
            leaf_index,
        }
    }

    fn block(block_id: u64, timestamp_ms: u64, events: Vec<Event>) -> Block {
        Block {
            block_id,
            timestamp_ms,
            events,
        }
    }

    #[test]
    fn test_register_expire_and_proof_path() {
        let (mut reader, _) = Reader::open_temporary([0; 32], 0).unwrap();
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
        let (mut reader, _) = Reader::open_temporary([0; 32], 0).unwrap();
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
                vec![Event::Extended {
                    id_commitment: id(1),
                    expiry_timestamp_ms: 9_000,
                }],
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
                    Event::Slashed {
                        id_commitment: id(1),
                    },
                    Event::Extended {
                        id_commitment: id(1),
                        expiry_timestamp_ms: 20_000,
                    },
                ],
            ))
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            reader.member(&id(1)).and_then(|m| m.removed),
            Some(Removed::Slashed)
        );
        assert_eq!(reader.active_count(), 0);
    }

    #[test]
    fn test_member_reads_the_previous_layout() {
        let member: Member = serde_json::from_str(
            r#"{"leaf_index":3,"expiry_timestamp_ms":5,"rate_commitment":"00000000000000000000000000000000000000000000000000000000000000ff","live":false,"removed":"Slashed"}"#,
        )
        .unwrap();
        assert!(!member.active);
        assert_eq!(member.removed, Some(Removed::Slashed));
        assert_eq!(
            serde_json::to_value(member.removed).unwrap(),
            serde_json::json!("slashed")
        );
    }

    #[test]
    fn test_reopen_continues_from_the_store() {
        let dir = std::env::temp_dir().join(format!("zone-sequencer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = {
            let (mut reader, extra) = Reader::open(&dir, [0; 32], 0).unwrap();
            assert!(extra.is_null());
            reader
                .apply_block(&block(
                    1,
                    1_000,
                    vec![registered(1, 0, 5_000), registered(2, 1, 9_000)],
                ))
                .unwrap();
            reader.apply_block(&block(2, 5_000, vec![])).unwrap();
            reader.persist(serde_json::json!({ "note": 7 })).unwrap();
            let root = reader.root();
            reader.close().unwrap();
            root
        };
        {
            let (reader, extra) = Reader::open(&dir, [0; 32], 0).unwrap();
            assert_eq!(reader.root(), root);
            assert_eq!(reader.applied_block_id, 2);
            assert_eq!(reader.active_count(), 1);
            assert_eq!(reader.removed_leaves().len(), 1);
            assert!(reader.proof_path(1).is_some());
            assert_eq!(extra["note"], 7);
        }
        // Another registry's reader refuses the store (sled holds one lock per store, so the
        // previous reader is dropped first).
        assert!(Reader::open(&dir, [9; 32], 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
