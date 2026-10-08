//! Zone batch wire format (RFC "Zone Batches / Batch format"), protobuf via `prost` derives.

use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct Batch {
    #[prost(uint64, tag = "1")]
    pub lez_from: u64,
    #[prost(uint64, tag = "2")]
    pub lez_to: u64,
    #[prost(message, repeated, tag = "3")]
    pub changes: Vec<Change>,
    #[prost(bytes = "vec", tag = "4")]
    pub root_after: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Change {
    #[prost(oneof = "Kind", tags = "1, 2, 3")]
    pub kind: Option<Kind>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum Kind {
    #[prost(message, tag = "1")]
    Insert(Insert),
    #[prost(message, tag = "2")]
    Update(Update),
    #[prost(message, tag = "3")]
    Remove(Remove),
}

#[derive(Clone, PartialEq, Message)]
pub struct Insert {
    #[prost(bytes = "vec", tag = "1")]
    pub id_commitment: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub rate_limit: u64,
    #[prost(uint64, tag = "3")]
    pub expiry_timestamp_ms: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Update {
    #[prost(uint64, tag = "1")]
    pub leaf_index: u64,
    #[prost(uint64, tag = "2")]
    pub expiry_timestamp_ms: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Remove {
    #[prost(uint64, tag = "1")]
    pub leaf_index: u64,
    #[prost(enumeration = "Cause", tag = "2")]
    pub cause: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, prost::Enumeration)]
#[repr(i32)]
pub enum Cause {
    Expired = 0,
    Slashed = 1,
    Erased = 2,
}

impl Change {
    pub fn insert(id_commitment: Vec<u8>, rate_limit: u64, expiry_timestamp_ms: u64) -> Self {
        Self {
            kind: Some(Kind::Insert(Insert {
                id_commitment,
                rate_limit,
                expiry_timestamp_ms,
            })),
        }
    }

    pub fn update(leaf_index: u64, expiry_timestamp_ms: u64) -> Self {
        Self {
            kind: Some(Kind::Update(Update {
                leaf_index,
                expiry_timestamp_ms,
            })),
        }
    }

    pub fn remove(leaf_index: u64, cause: Cause) -> Self {
        Self {
            kind: Some(Kind::Remove(Remove {
                leaf_index,
                cause: cause as i32,
            })),
        }
    }

    pub fn is_slash(&self) -> bool {
        matches!(&self.kind, Some(Kind::Remove(r)) if r.cause == Cause::Slashed as i32)
    }

    /// One-line description for logs.
    pub fn describe(&self) -> String {
        match &self.kind {
            Some(Kind::Insert(i)) => format!(
                "Insert {}.. rate {} expiry {}ms",
                hex::encode(&i.id_commitment[..4]),
                i.rate_limit,
                i.expiry_timestamp_ms
            ),
            Some(Kind::Update(u)) => format!(
                "Update leaf {} expiry {}ms",
                u.leaf_index, u.expiry_timestamp_ms
            ),
            Some(Kind::Remove(r)) => {
                let cause = Cause::try_from(r.cause).map_or("?".into(), |c| format!("{c:?}"));
                format!("Remove leaf {} ({cause})", r.leaf_index)
            }
            None => "empty change".into(),
        }
    }
}
