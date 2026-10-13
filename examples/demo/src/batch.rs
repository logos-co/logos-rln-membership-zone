//! Zone batch wire format (RFC "Batch format"), protobuf via `prost` derive macros.

use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct Batch {
    #[prost(uint64, tag = "1")]
    pub from_lez_block_id: u64,
    #[prost(uint64, tag = "2")]
    pub to_lez_block_id: u64,
    #[prost(message, repeated, tag = "3")]
    pub changes: Vec<Change>,
    #[prost(bytes = "vec", tag = "4")]
    pub root_after: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Change {
    #[prost(oneof = "Kind", tags = "1, 2")]
    pub kind: Option<Kind>,
}

/// The two tree operations of lez-rln's Merkle program (`MerkleOpcode::Insert`, `Remove`).
#[derive(Clone, PartialEq, prost::Oneof)]
pub enum Kind {
    #[prost(message, tag = "1")]
    Insert(Insert),
    #[prost(message, tag = "2")]
    Remove(Remove),
}

#[derive(Clone, PartialEq, Message)]
pub struct Insert {
    #[prost(uint64, tag = "1")]
    pub leaf_index: u64,
    /// 32 bytes, little-endian.
    #[prost(bytes = "vec", tag = "2")]
    pub rate_commitment: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Remove {
    #[prost(uint64, tag = "1")]
    pub leaf_index: u64,
}

impl Change {
    pub fn insert(leaf_index: u64, rate_commitment: Vec<u8>) -> Self {
        Self {
            kind: Some(Kind::Insert(Insert {
                leaf_index,
                rate_commitment,
            })),
        }
    }

    pub fn remove(leaf_index: u64) -> Self {
        Self {
            kind: Some(Kind::Remove(Remove { leaf_index })),
        }
    }

    /// One-line description for logs.
    pub fn describe(&self) -> String {
        match &self.kind {
            Some(Kind::Insert(i)) => format!(
                "Insert leaf {} ({}..)",
                i.leaf_index,
                hex::encode(&i.rate_commitment[..4])
            ),
            Some(Kind::Remove(r)) => format!("Remove leaf {}", r.leaf_index),
            None => "empty change".into(),
        }
    }
}
