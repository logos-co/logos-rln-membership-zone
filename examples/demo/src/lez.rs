//! In-memory stand-in for LEZ and the registration program (RFC "Registration Program").
//!
//! Only what the Zone needs is modelled: blocks with a timestamp, the successful
//! registration-program transactions of each block, membership accounts and balances. Every
//! block is treated as finalized as soon as it is produced; a real deployment follows only
//! finalized LEZ blocks (Invariant 4).

use std::collections::HashMap;

use rln::prelude::{Fr, Hasher, PoseidonHash};

use crate::membership_set::{TREE_DEPTH, fr_to_bytes_le};

/// Share of a forfeited deposit paid to the slasher, in percent (RFC Q13 `reward_portion`).
const REWARD_PORTION: u128 = 10;

/// `ConfigState`, the configuration account of the registry (durations in milliseconds).
#[derive(Debug, Clone)]
pub struct ConfigState {
    pub min_rate_limit: u64,
    pub max_rate_limit: u64,
    pub price_per_unit: u128,
    pub reg_fee: u128,
    pub min_active_duration_ms: u64,
    pub max_active_duration_ms: u64,
    pub grace_period_duration_ms: u64,
    pub withdrawal_delay_ms: u64,
    pub treasury_account_id: String,
    pub operator_account_id: String,
}

/// `MembershipState`, the membership account of one identity commitment.
#[derive(Debug, Clone)]
pub struct MembershipState {
    // `leaf_index` and `rate_limit` mirror the RFC; the Zone reads both from `Register`.
    #[allow(dead_code)]
    pub leaf_index: u64,
    #[allow(dead_code)]
    pub rate_limit: u64,
    pub expiry_timestamp_ms: u64,
    pub holder: String,
    pub deposit_amount: u128,
}

/// A membership account as LEZ shows it. A LEZ account never returns to its default
/// state, so `Erase` and `Slash` leave it `Emptied`.
#[derive(Debug, Clone, Copy)]
pub enum MembershipAccount<'a> {
    Default,
    Open(&'a MembershipState),
    Emptied,
}

/// A registration-program instruction. Only successful ones are recorded in blocks.
#[derive(Debug, Clone)]
pub enum Instruction {
    Register {
        payer: String,
        id_commitment: Fr,
        rate_limit: u64,
        expiry_timestamp_ms: u64,
        /// Assigned by the registration program on success; the value passed in is ignored.
        leaf_index: u64,
    },
    Extend {
        holder: String,
        id_commitment: Fr,
        expiry_timestamp_ms: u64,
    },
    Erase {
        holder: String,
        id_commitment: Fr,
    },
    Slash {
        slasher: String,
        identity_secret: Fr,
    },
}

/// A LEZ block as the Zone sees it: `block_id`, `timestamp`, registration-program transactions.
#[derive(Debug, Clone)]
pub struct Block {
    pub block_id: u64,
    pub timestamp: u64,
    pub transactions: Vec<Instruction>,
}

pub struct MockLez {
    pub config: ConfigState,
    blocks: Vec<Block>,
    pending: Vec<Instruction>,
    /// `CLOCK_01` timestamp: the timestamp of the last block.
    timestamp: u64,
    /// `ConfigState.next_index`: next free leaf index.
    next_index: u64,
    /// `None` once emptied by `Erase` or `Slash`.
    memberships: HashMap<[u8; 32], Option<MembershipState>>,
    balances: HashMap<String, u128>,
}

impl MockLez {
    pub fn new(config: ConfigState, funded: &[(&str, u128)]) -> Self {
        let balances = funded.iter().map(|(a, b)| ((*a).to_owned(), *b)).collect();
        Self {
            config,
            blocks: vec![],
            pending: vec![],
            timestamp: 0,
            next_index: 0,
            memberships: HashMap::new(),
            balances,
        }
    }

    /// `now` of the registration program: the `CLOCK_01` timestamp.
    pub fn now(&self) -> u64 {
        self.timestamp
    }

    pub fn last_finalized_block_id(&self) -> u64 {
        self.blocks.last().map_or(0, |b| b.block_id)
    }

    pub fn balance(&self, account: &str) -> u128 {
        self.balances.get(account).copied().unwrap_or(0)
    }

    pub fn membership_account(&self, id_commitment: &Fr) -> MembershipAccount<'_> {
        match self.memberships.get(&fr_to_bytes_le(id_commitment)) {
            None => MembershipAccount::Default,
            Some(Some(membership)) => MembershipAccount::Open(membership),
            Some(None) => MembershipAccount::Emptied,
        }
    }

    pub fn membership(&self, id_commitment: &Fr) -> Option<&MembershipState> {
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

    /// Execute a transaction against the registration program, with the RFC checks in order.
    /// On success it is queued for the next block.
    pub fn submit(&mut self, mut instruction: Instruction) -> Result<(), String> {
        let config = self.config.clone();
        let now = self.timestamp;
        match &instruction {
            Instruction::Register {
                payer,
                id_commitment,
                rate_limit,
                expiry_timestamp_ms,
                ..
            } => {
                // Bounds.
                if !(config.min_rate_limit..=config.max_rate_limit).contains(rate_limit) {
                    return Err("rate_limit out of bounds".into());
                }
                let active_duration_ms = expiry_timestamp_ms.saturating_sub(now);
                if !(config.min_active_duration_ms..=config.max_active_duration_ms)
                    .contains(&active_duration_ms)
                {
                    return Err("active duration out of bounds".into());
                }
                // Uniqueness: the membership account must still be a default account.
                let key = fr_to_bytes_le(id_commitment);
                if self.memberships.contains_key(&key) {
                    return Err("membership account already used".into());
                }
                // Capacity.
                if self.next_index >= 1 << TREE_DEPTH {
                    return Err("tree full: every leaf index has been used".into());
                }
                // Payment.
                let deposit_amount = u128::from(*rate_limit) * config.price_per_unit;
                self.debit(payer, deposit_amount + config.reg_fee)?;
                self.credit(&config.operator_account_id, config.reg_fee);
                // Accept.
                let membership = MembershipState {
                    leaf_index: self.next_index,
                    rate_limit: *rate_limit,
                    expiry_timestamp_ms: *expiry_timestamp_ms,
                    holder: payer.clone(),
                    deposit_amount,
                };
                self.memberships.insert(key, Some(membership));
            }
            Instruction::Extend {
                holder,
                id_commitment,
                expiry_timestamp_ms,
            } => {
                let membership = self.open_membership(id_commitment, Some(holder))?;
                // Grace period.
                let expiry = membership.expiry_timestamp_ms;
                if !(expiry.saturating_sub(config.grace_period_duration_ms)..expiry).contains(&now)
                {
                    return Err("membership is not in its grace period".into());
                }
                // Monotone.
                if *expiry_timestamp_ms <= expiry
                    || expiry_timestamp_ms.saturating_sub(now) > config.max_active_duration_ms
                {
                    return Err(
                        "expiry_timestamp_ms must grow and stay within max_active_duration_ms"
                            .into(),
                    );
                }
                // Accept.
                self.memberships
                    .get_mut(&fr_to_bytes_le(id_commitment))
                    .and_then(Option::as_mut)
                    .expect("checked")
                    .expiry_timestamp_ms = *expiry_timestamp_ms;
            }
            Instruction::Erase {
                holder,
                id_commitment,
            } => {
                let membership = self.open_membership(id_commitment, Some(holder))?;
                // Withdrawal delay.
                if now < membership.expiry_timestamp_ms + config.withdrawal_delay_ms {
                    return Err("withdrawal delay not elapsed".into());
                }
                // Accept: refund the holder and empty the membership account.
                self.memberships.insert(fr_to_bytes_le(id_commitment), None);
                self.credit(&membership.holder, membership.deposit_amount);
            }
            Instruction::Slash {
                slasher,
                identity_secret,
            } => {
                // Membership.
                let id_commitment = Hasher::<PoseidonHash>::hash_single(*identity_secret);
                let membership = self.open_membership(&id_commitment, None)?;
                // Accept: `reward_portion` to the slasher, the rest to the treasury.
                let reward = membership.deposit_amount * REWARD_PORTION / 100;
                self.memberships
                    .insert(fr_to_bytes_le(&id_commitment), None);
                self.credit(slasher, reward);
                self.credit(
                    &config.treasury_account_id,
                    membership.deposit_amount - reward,
                );
            }
        }
        // Record the assigned leaf index in the executed transaction, where the Zone reads it.
        if let Instruction::Register { leaf_index, .. } = &mut instruction {
            *leaf_index = self.next_index;
            self.next_index += 1;
        }
        self.pending.push(instruction);
        Ok(())
    }

    /// Close the current block: advance the timestamp and record the queued transactions.
    /// The clock program runs last, so transactions in a block see the previous timestamp.
    pub fn produce_block(&mut self, step_ms: u64) -> &Block {
        self.timestamp += step_ms;
        let block = Block {
            block_id: self.last_finalized_block_id() + 1,
            timestamp: self.timestamp,
            transactions: std::mem::take(&mut self.pending),
        };
        self.blocks.push(block);
        self.blocks.last().expect("just pushed")
    }

    fn open_membership(
        &self,
        id_commitment: &Fr,
        holder: Option<&String>,
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

    fn debit(&mut self, account: &str, amount: u128) -> Result<(), String> {
        let balance = self.balances.entry(account.to_owned()).or_default();
        *balance = balance.checked_sub(amount).ok_or("insufficient balance")?;
        Ok(())
    }

    fn credit(&mut self, account: &str, amount: u128) {
        *self.balances.entry(account.to_owned()).or_default() += amount;
    }
}

#[cfg(test)]
pub(crate) mod test {
    use rand::{rngs::ThreadRng, thread_rng};
    use rln::prelude::IdentityKeys;

    use super::*;

    /// Demo parameters: grace period 4000 ms, withdrawal delay 6000 ms,
    /// active durations 4000-60000 ms.
    pub(crate) fn new_lez() -> MockLez {
        let config = ConfigState {
            min_rate_limit: 100,
            max_rate_limit: 600,
            price_per_unit: 10,
            reg_fee: 5,
            min_active_duration_ms: 4_000,
            max_active_duration_ms: 60_000,
            grace_period_duration_ms: 4_000,
            withdrawal_delay_ms: 6_000,
            treasury_account_id: "treasury".into(),
            operator_account_id: "operator".into(),
        };
        MockLez::new(config, &[("alice", 10_000), ("bob", 10_000)])
    }

    pub(crate) fn new_keys() -> IdentityKeys {
        IdentityKeys::generate::<PoseidonHash, ThreadRng>(&mut thread_rng())
    }

    pub(crate) fn register(
        payer: &str,
        keys: &IdentityKeys,
        expiry_timestamp_ms: u64,
    ) -> Instruction {
        Instruction::Register {
            payer: payer.into(),
            id_commitment: keys.id_commitment(),
            rate_limit: 100,
            expiry_timestamp_ms,
            leaf_index: 0,
        }
    }

    #[test]
    fn test_register_reverts_when_tree_is_full() {
        let mut lez = new_lez();
        lez.next_index = 1 << TREE_DEPTH;
        let err = lez
            .submit(register("alice", &new_keys(), 4_000))
            .unwrap_err();
        assert!(err.starts_with("tree full"));
    }

    #[test]
    fn test_extend_only_in_grace_period() {
        let mut lez = new_lez();
        let keys = new_keys();
        lez.submit(register("alice", &keys, 8_000)).unwrap();
        let extend = |expiry_timestamp_ms| Instruction::Extend {
            holder: "alice".into(),
            id_commitment: keys.id_commitment(),
            expiry_timestamp_ms,
        };
        // now 0: grace period is [4000, 8000).
        assert!(lez.submit(extend(12_000)).is_err());
        lez.produce_block(2_000);
        lez.produce_block(2_000);
        // now 4000: in the grace period.
        lez.submit(extend(12_000)).unwrap();
        lez.produce_block(4_000);
        lez.produce_block(4_000);
        // now 16000: expired, cannot be extended any more.
        assert!(lez.submit(extend(20_000)).is_err());
    }

    #[test]
    fn test_erased_commitment_cannot_register_again() {
        let mut lez = new_lez();
        let keys = new_keys();
        lez.submit(register("alice", &keys, 4_000)).unwrap();
        lez.produce_block(10_000);
        lez.submit(Instruction::Erase {
            holder: "alice".into(),
            id_commitment: keys.id_commitment(),
        })
        .unwrap();
        assert!(matches!(
            lez.membership_account(&keys.id_commitment()),
            MembershipAccount::Emptied
        ));
        let err = lez.submit(register("alice", &keys, 20_000)).unwrap_err();
        assert_eq!(err, "membership account already used");
    }
}
