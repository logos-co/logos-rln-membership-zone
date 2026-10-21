//! In-memory stand-in for LEZ and the registration program (RFC "LEZ Requirements").
//!
//! Only what the Zone needs is modelled: blocks with a timestamp, the events the registration
//! program emitted in each block, membership accounts and balances. A block and its events are
//! readable as soon as the block is produced, as the RFC trusts the LEZ Sequencer to serve
//! closed blocks, with their events, without ever changing them.
//!
//! Types follow lez-rln: ids are 32 bytes, amounts `u128`, counts `u64`, timestamps Unix-epoch
//! milliseconds; durations are milliseconds too, as RLN-API requires (lez-rln uses seconds).

use std::collections::HashMap;

use rln::prelude::{Hasher, PoseidonHash};

use crate::reader::{TREE_DEPTH, fr_from_bytes_le, fr_to_bytes_le};

/// `first_lez_block_id` of the registry: the LEZ block that created `ConfigState`. The mock
/// creates it before its first block.
pub const FIRST_LEZ_BLOCK_ID: u64 = 1;

/// A LEZ account id. The mock derives it from a name, so logs stay readable.
pub type AccountId = [u8; 32];

pub fn account(name: &str) -> AccountId {
    let mut id = [0u8; 32];
    id[..name.len()].copy_from_slice(name.as_bytes());
    id
}

pub fn account_name(id: &AccountId) -> String {
    String::from_utf8_lossy(id)
        .trim_end_matches('\0')
        .to_owned()
}

/// `ConfigState`, the configuration account of the registry.
#[derive(Debug, Clone)]
pub struct ConfigState {
    pub zone_channel_id: [u8; 32],
    pub operator_account_id: AccountId,
    pub registration_fee: u128,
    pub epoch_size_sec: u64,
    pub min_rate_limit: u64,
    pub max_rate_limit: u64,
    pub min_active_duration_ms: u64,
    pub max_active_duration_ms: u64,
    pub tree_id: [u8; 32],
    pub next_index: u64,
    pub price_per_unit: u128,
    pub treasury_account_id: AccountId,
    pub grace_period_duration_ms: u64,
    pub withdrawal_delay_ms: u64,
}

/// `MembershipState`, the membership account of one identity commitment. The grace period and
/// the withdrawal delay are copied from `ConfigState` at `Register`, so a later configuration
/// change cannot touch an open membership.
#[derive(Debug, Clone)]
pub struct MembershipState {
    pub expiry_timestamp_ms: u64,
    pub withdrawal_delay_ms: u64,
    pub leaf_index: u64,
    pub rate_limit: u64,
    pub grace_period_duration_ms: u64,
    pub holder: AccountId,
    pub deposit_amount: u128,
}

/// A membership account as LEZ shows it. A LEZ account never returns to its default
/// state, so `Erase` and `Slash` leave it closed; which one closed it is read from LEZ.
#[derive(Debug, Clone, Copy)]
pub enum MembershipAccount<'a> {
    Default,
    Open(&'a MembershipState),
    ClosedByErase,
    ClosedBySlash,
}

/// What a membership account holds once it exists.
#[derive(Debug, Clone)]
enum Slot {
    Open(MembershipState),
    ClosedByErase,
    ClosedBySlash,
}

/// The registration program's instructions (RFC "LEZ Requirements / Actions"), as lez-rln
/// lays them out: `tree_id` first, ids as 32 bytes. The signer is the transaction's.
#[derive(Debug, Clone)]
pub enum Instruction {
    Register {
        tree_id: [u8; 32],
        id_commitment: [u8; 32],
        rate_limit: u64,
        expiry_timestamp_ms: u64,
    },
    Extend {
        tree_id: [u8; 32],
        id_commitment: [u8; 32],
        expiry_timestamp_ms: u64,
    },
    Erase {
        tree_id: [u8; 32],
        id_commitment: [u8; 32],
    },
    Slash {
        tree_id: [u8; 32],
        identity_secret: [u8; 32],
    },
}

/// A transaction as the mock executes it: the signing account and its instruction.
#[derive(Debug, Clone)]
pub struct Transaction {
    pub signer: AccountId,
    pub instruction: Instruction,
}

/// The events the registration program emits, one per successful action (RFC "LEZ
/// Requirements / Events"): the only input of the derivation rules.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Registered {
        tree_id: [u8; 32],
        id_commitment: [u8; 32],
        rate_limit: u64,
        expiry_timestamp_ms: u64,
        leaf_index: u64,
    },
    Extended {
        tree_id: [u8; 32],
        id_commitment: [u8; 32],
        expiry_timestamp_ms: u64,
    },
    Erased {
        tree_id: [u8; 32],
        id_commitment: [u8; 32],
    },
    Slashed {
        tree_id: [u8; 32],
        id_commitment: [u8; 32],
    },
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Registered { .. } => "Registered",
            Self::Extended { .. } => "Extended",
            Self::Erased { .. } => "Erased",
            Self::Slashed { .. } => "Slashed",
        }
    }
}

/// A LEZ block as a reader sees it: `block_id`, `timestamp`, and the registration program's
/// events, served with the block.
#[derive(Debug, Clone)]
pub struct Block {
    pub block_id: u64,
    pub timestamp: u64,
    pub events: Vec<Event>,
}

pub struct MockLez {
    pub config: ConfigState,
    blocks: Vec<Block>,
    pending: Vec<Event>,
    /// `CLOCK_01` timestamp: the timestamp of the last block.
    timestamp: u64,
    memberships: HashMap<[u8; 32], Slot>,
    balances: HashMap<AccountId, u128>,
}

impl MockLez {
    pub fn new(config: ConfigState, funded: &[(&str, u128)]) -> Self {
        let balances = funded.iter().map(|(a, b)| (account(a), *b)).collect();
        Self {
            config,
            blocks: vec![],
            pending: vec![],
            timestamp: 0,
            memberships: HashMap::new(),
            balances,
        }
    }

    /// `now` of the registration program: the `CLOCK_01` timestamp.
    pub fn now(&self) -> u64 {
        self.timestamp
    }

    pub fn last_block_id(&self) -> u64 {
        self.blocks.last().map_or(0, |b| b.block_id)
    }

    pub fn balance(&self, name: &str) -> u128 {
        self.balances.get(&account(name)).copied().unwrap_or(0)
    }

    pub fn membership_account(&self, id_commitment: &[u8; 32]) -> MembershipAccount<'_> {
        match self.memberships.get(id_commitment) {
            None => MembershipAccount::Default,
            Some(Slot::Open(membership)) => MembershipAccount::Open(membership),
            Some(Slot::ClosedByErase) => MembershipAccount::ClosedByErase,
            Some(Slot::ClosedBySlash) => MembershipAccount::ClosedBySlash,
        }
    }

    pub fn membership(&self, id_commitment: &[u8; 32]) -> Option<&MembershipState> {
        match self.membership_account(id_commitment) {
            MembershipAccount::Open(membership) => Some(membership),
            _ => None,
        }
    }

    /// Blocks in `[from, to]`, inclusive (`block_id`s start at 1).
    pub fn blocks(&self, from: u64, to: u64) -> &[Block] {
        if from > to || from == 0 {
            return &[];
        }
        &self.blocks[(from - 1) as usize..to as usize]
    }

    /// Execute a transaction against the registration program, per the RFC "LEZ
    /// Requirements". On success its event is queued for the next block; a failed transaction
    /// emits nothing.
    pub fn submit(&mut self, tx: Transaction) -> Result<(), String> {
        let config = self.config.clone();
        let now = self.timestamp;
        let signer = tx.signer;
        let event = match tx.instruction {
            Instruction::Register {
                tree_id,
                id_commitment,
                rate_limit,
                expiry_timestamp_ms,
            } => {
                self.check_tree(tree_id)?;
                // Bounds.
                if !(config.min_rate_limit..=config.max_rate_limit).contains(&rate_limit) {
                    return Err("rate_limit out of bounds".into());
                }
                let active_duration_ms = expiry_timestamp_ms.saturating_sub(now);
                if !(config.min_active_duration_ms..=config.max_active_duration_ms)
                    .contains(&active_duration_ms)
                {
                    return Err("active duration out of bounds".into());
                }
                // Uniqueness: the membership account must still be a default account.
                if self.memberships.contains_key(&id_commitment) {
                    return Err("membership account already used".into());
                }
                // Capacity.
                if self.config.next_index >= 1 << TREE_DEPTH {
                    return Err("tree full: every leaf index has been used".into());
                }
                // Payment.
                let deposit_amount = u128::from(rate_limit) * config.price_per_unit;
                self.debit(&signer, deposit_amount + config.registration_fee)?;
                self.credit(&config.operator_account_id, config.registration_fee);
                // Accept: the next leaf index, and the durations copied from the config.
                let leaf_index = self.config.next_index;
                self.config.next_index += 1;
                let membership = MembershipState {
                    expiry_timestamp_ms,
                    withdrawal_delay_ms: config.withdrawal_delay_ms,
                    leaf_index,
                    rate_limit,
                    grace_period_duration_ms: config.grace_period_duration_ms,
                    holder: signer,
                    deposit_amount,
                };
                self.memberships
                    .insert(id_commitment, Slot::Open(membership));
                Event::Registered {
                    tree_id,
                    id_commitment,
                    rate_limit,
                    expiry_timestamp_ms,
                    leaf_index,
                }
            }
            Instruction::Extend {
                tree_id,
                id_commitment,
                expiry_timestamp_ms,
            } => {
                self.check_tree(tree_id)?;
                let membership = self.open_membership(&id_commitment, Some(&signer))?;
                // The membership's own grace period.
                let expiry = membership.expiry_timestamp_ms;
                if !(expiry.saturating_sub(membership.grace_period_duration_ms)..expiry)
                    .contains(&now)
                {
                    return Err("membership is not in its grace period".into());
                }
                // Monotone, within the current config's bound.
                if expiry_timestamp_ms <= expiry
                    || expiry_timestamp_ms.saturating_sub(now) > config.max_active_duration_ms
                {
                    return Err(
                        "expiry_timestamp_ms must grow and stay within max_active_duration_ms"
                            .into(),
                    );
                }
                // Accept.
                if let Some(Slot::Open(membership)) = self.memberships.get_mut(&id_commitment) {
                    membership.expiry_timestamp_ms = expiry_timestamp_ms;
                }
                Event::Extended {
                    tree_id,
                    id_commitment,
                    expiry_timestamp_ms,
                }
            }
            Instruction::Erase {
                tree_id,
                id_commitment,
            } => {
                self.check_tree(tree_id)?;
                let membership = self.open_membership(&id_commitment, Some(&signer))?;
                // The membership's own withdrawal delay.
                if now < membership.expiry_timestamp_ms + membership.withdrawal_delay_ms {
                    return Err("withdrawal delay not elapsed".into());
                }
                // Accept: refund the holder and close the membership account.
                self.memberships.insert(id_commitment, Slot::ClosedByErase);
                self.credit(&membership.holder, membership.deposit_amount);
                Event::Erased {
                    tree_id,
                    id_commitment,
                }
            }
            Instruction::Slash {
                tree_id,
                identity_secret,
            } => {
                self.check_tree(tree_id)?;
                // Membership: the one whose commitment the secret hashes to.
                let id_commitment = fr_to_bytes_le(&Hasher::<PoseidonHash>::hash_single(
                    fr_from_bytes_le(&identity_secret)?,
                ));
                let membership = self.open_membership(&id_commitment, None)?;
                // Accept: the whole deposit to the treasury (a slasher reward is RFC Q4).
                self.memberships.insert(id_commitment, Slot::ClosedBySlash);
                self.credit(&config.treasury_account_id, membership.deposit_amount);
                Event::Slashed {
                    tree_id,
                    id_commitment,
                }
            }
        };
        self.pending.push(event);
        Ok(())
    }

    /// Close the current block: advance the timestamp and record the queued events.
    /// The clock program runs last, so actions in a block see the previous timestamp.
    pub fn produce_block(&mut self, step_ms: u64) -> &Block {
        self.timestamp += step_ms;
        let block = Block {
            block_id: self.last_block_id() + 1,
            timestamp: self.timestamp,
            events: std::mem::take(&mut self.pending),
        };
        self.blocks.push(block);
        self.blocks.last().expect("just pushed")
    }

    fn check_tree(&self, tree_id: [u8; 32]) -> Result<(), String> {
        if tree_id != self.config.tree_id {
            return Err("no registry with this tree_id".into());
        }
        Ok(())
    }

    fn open_membership(
        &self,
        id_commitment: &[u8; 32],
        holder: Option<&AccountId>,
    ) -> Result<MembershipState, String> {
        let membership = self
            .membership(id_commitment)
            .cloned()
            .ok_or("no open membership account")?;
        if holder.is_some_and(|h| *h != membership.holder) {
            return Err("signer is not the holder".into());
        }
        Ok(membership)
    }

    fn debit(&mut self, account: &AccountId, amount: u128) -> Result<(), String> {
        let balance = self.balances.entry(*account).or_default();
        *balance = balance.checked_sub(amount).ok_or("insufficient balance")?;
        Ok(())
    }

    fn credit(&mut self, account: &AccountId, amount: u128) {
        *self.balances.entry(*account).or_default() += amount;
    }
}

#[cfg(test)]
pub(crate) mod test {
    use rand::{rngs::ThreadRng, thread_rng};
    use rln::prelude::IdentityKeys;

    use super::*;

    pub(crate) const TREE_ID: [u8; 32] = [0; 32];

    /// Demo parameters: grace period 4000 ms, withdrawal delay 6000 ms,
    /// active durations 4000-60000 ms.
    pub(crate) fn new_lez() -> MockLez {
        let config = ConfigState {
            zone_channel_id: [1; 32],
            operator_account_id: account("operator"),
            registration_fee: 5,
            epoch_size_sec: 600,
            min_rate_limit: 100,
            max_rate_limit: 600,
            min_active_duration_ms: 4_000,
            max_active_duration_ms: 60_000,
            tree_id: TREE_ID,
            next_index: 0,
            price_per_unit: 10,
            treasury_account_id: account("treasury"),
            grace_period_duration_ms: 4_000,
            withdrawal_delay_ms: 6_000,
        };
        MockLez::new(config, &[("alice", 10_000), ("bob", 10_000)])
    }

    pub(crate) fn new_keys() -> IdentityKeys {
        IdentityKeys::generate::<PoseidonHash, ThreadRng>(&mut thread_rng())
    }

    pub(crate) fn id_commitment(keys: &IdentityKeys) -> [u8; 32] {
        fr_to_bytes_le(&keys.id_commitment())
    }

    pub(crate) fn tx(signer: &str, instruction: Instruction) -> Transaction {
        Transaction {
            signer: account(signer),
            instruction,
        }
    }

    pub(crate) fn register(
        payer: &str,
        keys: &IdentityKeys,
        expiry_timestamp_ms: u64,
    ) -> Transaction {
        tx(
            payer,
            Instruction::Register {
                tree_id: TREE_ID,
                id_commitment: id_commitment(keys),
                rate_limit: 100,
                expiry_timestamp_ms,
            },
        )
    }

    pub(crate) fn extend(
        holder: &str,
        keys: &IdentityKeys,
        expiry_timestamp_ms: u64,
    ) -> Transaction {
        tx(
            holder,
            Instruction::Extend {
                tree_id: TREE_ID,
                id_commitment: id_commitment(keys),
                expiry_timestamp_ms,
            },
        )
    }

    pub(crate) fn erase(holder: &str, keys: &IdentityKeys) -> Transaction {
        tx(
            holder,
            Instruction::Erase {
                tree_id: TREE_ID,
                id_commitment: id_commitment(keys),
            },
        )
    }

    pub(crate) fn slash(slasher: &str, keys: &IdentityKeys) -> Transaction {
        tx(
            slasher,
            Instruction::Slash {
                tree_id: TREE_ID,
                identity_secret: fr_to_bytes_le(&keys.identity_secret()),
            },
        )
    }

    #[test]
    fn test_register_reverts_when_tree_is_full() {
        let mut lez = new_lez();
        lez.config.next_index = 1 << TREE_DEPTH;
        let err = lez
            .submit(register("alice", &new_keys(), 4_000))
            .unwrap_err();
        assert!(err.starts_with("tree full"));
    }

    #[test]
    fn test_successful_actions_emit_events_in_order() {
        let mut lez = new_lez();
        let (a, b) = (new_keys(), new_keys());
        lez.submit(register("alice", &a, 8_000)).unwrap();
        assert!(lez.submit(register("alice", &a, 8_000)).is_err());
        lez.submit(register("bob", &b, 8_000)).unwrap();
        let block = lez.produce_block(2_000);
        assert_eq!(block.events.len(), 2, "a failed action emits nothing");
        assert!(matches!(
            block.events[1],
            Event::Registered {
                leaf_index: 1,
                rate_limit: 100,
                ..
            }
        ));
    }

    #[test]
    fn test_extend_only_in_grace_period() {
        let mut lez = new_lez();
        let keys = new_keys();
        lez.submit(register("alice", &keys, 8_000)).unwrap();
        // now 0: grace period is [4000, 8000).
        assert!(lez.submit(extend("alice", &keys, 12_000)).is_err());
        lez.produce_block(2_000);
        lez.produce_block(2_000);
        // now 4000: in the grace period.
        lez.submit(extend("alice", &keys, 12_000)).unwrap();
        lez.produce_block(4_000);
        lez.produce_block(4_000);
        // now 16000: expired, cannot be extended any more.
        assert!(lez.submit(extend("alice", &keys, 20_000)).is_err());
    }

    #[test]
    fn test_membership_keeps_its_own_withdrawal_delay() {
        let mut lez = new_lez();
        let keys = new_keys();
        lez.submit(register("alice", &keys, 4_000)).unwrap();
        // The configuration changes after the registration: the membership keeps 6000.
        lez.config.withdrawal_delay_ms = 60_000;
        lez.produce_block(10_000);
        lez.submit(erase("alice", &keys)).unwrap();
        assert!(matches!(
            lez.membership_account(&id_commitment(&keys)),
            MembershipAccount::ClosedByErase
        ));
        let err = lez.submit(register("alice", &keys, 20_000)).unwrap_err();
        assert_eq!(err, "membership account already used");
    }
}
