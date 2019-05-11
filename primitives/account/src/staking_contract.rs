use std::cmp::{Ordering, min};
use std::collections::btree_set::BTreeSet;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use beserial::{Deserialize, ReadBytesExt, Serialize, SerializingError, WriteBytesExt};
use bls::bls12_381::PublicKey as BlsPublicKey;
use bls::bls12_381::Signature as BlsSignature;
use collections::SegmentTree;
use keys::Address;
use hash::{Blake2bHasher, Hasher};
use primitives::coin::Coin;
use primitives::policy;
use transaction::{SignatureProof, Transaction};
use transaction::account::staking_contract::StakingTransactionData;

use crate::{Account, AccountError, AccountTransactionInteraction, AccountType};
use crate::inherent::{AccountInherentInteraction, Inherent, InherentType};


#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveStake {
    staker_address: Address,
    balance: Coin,
    validator_key: BlsPublicKey, // TODO Share validator keys eventually and if required
    reward_address: Option<Address>,
}

impl PartialEq for ActiveStake {
    fn eq(&self, other: &ActiveStake) -> bool {
        self.balance == other.balance
            && self.staker_address == other.staker_address
    }
}

impl Eq for ActiveStake {}

impl PartialOrd for ActiveStake {
    fn partial_cmp(&self, other: &ActiveStake) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ActiveStake {
    // Highest to low balances
    fn cmp(&self, other: &Self) -> Ordering {
        other.balance.cmp(&self.balance)
            .then_with(|| self.staker_address.cmp(&other.staker_address))
    }
}

pub struct ValidatorSet {
    pub active: Vec<ActiveValidator>,
    pub min_required_stake: Coin,
}

pub struct ActiveValidator {
    pub staking_address: Address,
    pub reward_address_opt: Option<Address>,
}

impl ActiveValidator {
    #[inline]
    pub fn reward_address(&self) -> &Address {
        if let Some(ref addr) = self.reward_address_opt {
            addr
        } else {
            &self.staking_address
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct InactiveStake {
    balance: Coin,
    retire_time: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct ActiveStakeReceipt {
    validator_key: BlsPublicKey,
    reward_address: Option<Address>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
struct InactiveStakeReceipt {
    retire_time: u32,
}

/**
 Here's an explanation of how the different transactions work.
 1. Stake:
    - Transaction from staking address to contract
    - Transfers value into a new or existing entry in the active_stake list
    - Existing entries are updated with potentially new validator_key and reward_address
    - Normal transaction, signed by staking/sender address
 2. Retire:
    - Transaction from staking contract to itself
    - Removes a balance (the transaction value) from the active stake of a staker
      (may remove staker from active stake list entirely)
    - Puts the balance into the inactive_stake list, recording the retire_time.
    - If a staker retires multiple times, balance is added to the existing entry and
      retire_time is reset.
    - Signed by staking/sender address
 3. Unstake:
    - Transaction from the contract to an external address
    - If condition of block_height ≥ next_macro_block_after(retire_time) + UNSTAKE_DELAY is met,
      transfers value from inactive_validators entry/entries
    - Signed by staking/sender address

  Reverting transactions:
  Since transactions need to be revertable, the with_{incoming,outgoing}_transaction functions
  may also return binary data (Vec<u8>) containing additional information to that transaction.
  Internally, this data can be serialized/deserialized.

  Objects:
  ActiveStake: Stake considered for validator selection, characterized by the tuple
    (staker_address, balance, validator_key, optional reward_address).
  InactiveStake: Stake ignored for validator selection, represented by the tuple
    (balance, retire_time).

  Internal lookups required:
  - Stake requires a way to get from a staker address to an ActiveStake object
  - Retire requires a way to get from a staker address to an ActiveStake object
    and from a staker address to the list of InactiveStake objects.
  - Unstake requires a way to get from a staker address to the list of InactiveStake objects.
  - Retrieving the list of active stakes that are actually considered for the selection
    requires a list of ActiveStake objects ordered by its balance.
 */
#[derive(Clone, Debug)]
pub struct StakingContract {
    pub balance: Coin,
    pub active_stake_sorted: BTreeSet<Arc<ActiveStake>>, // A list might be sufficient.
    pub active_stake_by_address: HashMap<Address, Arc<ActiveStake>>,
    pub inactive_stake_by_address: HashMap<Address, InactiveStake>,
}

impl StakingContract {
    /// Adds funds to stake of `address`.
    /// XXX This is public to fill the genesis staking contract
    pub fn stake(&mut self, staker_address: &Address, value: Coin, validator_key: BlsPublicKey, reward_address: Option<Address>) -> Result<Option<ActiveStakeReceipt>, AccountError> {
        self.balance = Account::balance_add(self.balance, value)?;

        if let Some(active_stake) = self.active_stake_by_address.remove(staker_address) {
            let new_active_stake = Arc::new(ActiveStake {
                staker_address: active_stake.staker_address.clone(),
                balance: Account::balance_add(active_stake.balance, value)?,
                validator_key,
                reward_address
            });

            self.active_stake_sorted.remove(&active_stake);
            self.active_stake_sorted.insert(Arc::clone(&new_active_stake));
            self.active_stake_by_address.insert(staker_address.clone(), new_active_stake);

            Ok(Some(ActiveStakeReceipt {
                validator_key: active_stake.validator_key.clone(),
                reward_address: active_stake.reward_address.clone(),
            }))
        } else {
            let stake = Arc::new(ActiveStake {
                staker_address: staker_address.clone(),
                balance: value,
                validator_key,
                reward_address,
            });
            self.active_stake_sorted.insert(Arc::clone(&stake));
            self.active_stake_by_address.insert(staker_address.clone(), stake);

            Ok(None)
        }
    }

    /// Reverts a stake transaction.
    fn revert_stake(&mut self, staker_address: &Address, value: Coin, receipt: Option<ActiveStakeReceipt>) -> Result<(), AccountError> {
        self.balance = Account::balance_sub(self.balance, value)?;

        let active_stake = self.active_stake_by_address.get(&staker_address)
            .ok_or(AccountError::InvalidForRecipient)?;

        if active_stake.balance > value {
            let receipt = receipt.ok_or(AccountError::InvalidReceipt)?;
            let new_active_stake = Arc::new(ActiveStake {
                staker_address: active_stake.staker_address.clone(),
                balance: Account::balance_sub(active_stake.balance, value)?,
                validator_key: receipt.validator_key,
                reward_address: receipt.reward_address,
            });

            self.active_stake_sorted.remove(active_stake);
            self.active_stake_sorted.insert(Arc::clone(&new_active_stake));
            self.active_stake_by_address.insert(staker_address.clone(), new_active_stake);
        } else {
            assert_eq!(active_stake.balance, value);
            if receipt.is_some() {
                return Err(AccountError::InvalidReceipt);
            }

            self.active_stake_sorted.remove(active_stake);
            self.active_stake_by_address.remove(staker_address);
        }
        Ok(())
    }

    /// Removes stake from the active stake list.
    fn retire_sender(&mut self, staker_address: &Address, total_value: Coin, _block_height: u32) -> Result<Option<ActiveStakeReceipt>, AccountError> {
        self.balance = Account::balance_sub(self.balance, total_value)?;

        let active_stake = self.active_stake_by_address.remove(staker_address)
            .ok_or(AccountError::InvalidForSender)?;

        self.active_stake_sorted.remove(&active_stake);

        if active_stake.balance > total_value {
            let new_active_stake = Arc::new(ActiveStake {
                staker_address: staker_address.clone(),
                balance: Account::balance_sub(active_stake.balance, total_value)?,
                validator_key: active_stake.validator_key.clone(),
                reward_address: active_stake.reward_address.clone(),
            });

            self.active_stake_sorted.insert(Arc::clone(&new_active_stake));
            self.active_stake_by_address.insert(staker_address.clone(), new_active_stake);

            Ok(None)
        } else {
            assert_eq!(active_stake.balance, total_value);
            Ok(Some(ActiveStakeReceipt {
                validator_key: active_stake.validator_key.clone(),
                reward_address: active_stake.reward_address.clone(),
            }))
        }
    }

    /// Reverts the sender side of a retire transaction.
    fn revert_retire_sender(&mut self, staker_address: &Address, total_value: Coin, receipt: Option<ActiveStakeReceipt>) -> Result<(), AccountError> {
        self.balance = Account::balance_add(self.balance, total_value)?;

        if let Some(active_stake) = self.active_stake_by_address.remove(staker_address) {
            if receipt.is_some() {
                return Err(AccountError::InvalidReceipt);
            }

            let new_active_stake = Arc::new(ActiveStake {
                staker_address: staker_address.clone(),
                balance: Account::balance_add(active_stake.balance, total_value)?,
                validator_key: active_stake.validator_key.clone(),
                reward_address: active_stake.reward_address.clone(),
            });

            self.active_stake_sorted.remove(&active_stake);
            self.active_stake_sorted.insert(Arc::clone(&new_active_stake));
            self.active_stake_by_address.insert(staker_address.clone(), new_active_stake);
        } else {
            let receipt = receipt.ok_or(AccountError::InvalidReceipt)?;
            let new_active_stake = Arc::new(ActiveStake {
                staker_address: staker_address.clone(),
                balance: total_value,
                validator_key: receipt.validator_key,
                reward_address: receipt.reward_address,
            });

            self.active_stake_sorted.insert(Arc::clone(&new_active_stake));
            self.active_stake_by_address.insert(staker_address.clone(), new_active_stake);
        }
        Ok(())
    }

    /// Adds state to the inactive stake list.
    fn retire_recipient(&mut self, staker_address: &Address, value: Coin, block_height: u32) -> Result<Option<InactiveStakeReceipt>, AccountError> {
        self.balance = Account::balance_add(self.balance, value)?;

        if let Some(inactive_stake) = self.inactive_stake_by_address.remove(staker_address) {
            let new_inactive_stake = InactiveStake {
                balance: Account::balance_add(inactive_stake.balance, value)?,
                retire_time: block_height,
            };
            self.inactive_stake_by_address.insert(staker_address.clone(), new_inactive_stake);

            Ok(Some(InactiveStakeReceipt {
                retire_time: inactive_stake.retire_time,
            }))
        } else {
            let new_inactive_stake = InactiveStake {
                balance: value,
                retire_time: block_height,
            };
            self.inactive_stake_by_address.insert(staker_address.clone(), new_inactive_stake);

            Ok(None)
        }
    }

    /// Reverts a retire transaction.
    fn revert_retire_recipient(&mut self, staker_address: &Address, value: Coin, receipt: Option<InactiveStakeReceipt>) -> Result<(), AccountError> {
        self.balance = Account::balance_sub(self.balance, value)?;

        let inactive_stake = self.inactive_stake_by_address.remove(staker_address)
            .ok_or(AccountError::InvalidForRecipient)?;

        if inactive_stake.balance > value {
            let receipt = receipt.ok_or(AccountError::InvalidReceipt)?;
            let new_inactive_stake = InactiveStake {
                balance: Account::balance_sub(inactive_stake.balance, value)?,
                retire_time: receipt.retire_time,
            };
            self.inactive_stake_by_address.insert(staker_address.clone(), new_inactive_stake);
        } else if receipt.is_some() {
            return Err(AccountError::InvalidReceipt)
        }
        Ok(())
    }

    /// Removes stake from the inactive stake list.
    fn unstake(&mut self, staker_address: &Address, total_value: Coin) -> Result<Option<InactiveStakeReceipt>, AccountError> {
        self.balance = Account::balance_sub(self.balance, total_value)?;

        let inactive_stake = self.inactive_stake_by_address.remove(staker_address)
            .ok_or(AccountError::InvalidForSender)?;

        if inactive_stake.balance > total_value {
            let new_inactive_stake = InactiveStake {
                balance: Account::balance_sub(inactive_stake.balance, total_value)?,
                retire_time: inactive_stake.retire_time,
            };
            self.inactive_stake_by_address.insert(staker_address.clone(), new_inactive_stake);

            Ok(None)
        } else {
            assert_eq!(inactive_stake.balance, total_value);
            Ok(Some(InactiveStakeReceipt {
                retire_time: inactive_stake.retire_time,
            }))
        }
    }

    /// Reverts a unstake transaction.
    fn revert_unstake(&mut self, staker_address: &Address, total_value: Coin, receipt: Option<InactiveStakeReceipt>) -> Result<(), AccountError> {
        self.balance = Account::balance_add(self.balance, total_value)?;

        if let Some(inactive_stake) = self.inactive_stake_by_address.remove(staker_address) {
            if receipt.is_some() {
                return Err(AccountError::InvalidReceipt);
            }

            let new_inactive_stake = InactiveStake {
                balance: Account::balance_add(inactive_stake.balance, total_value)?,
                retire_time: inactive_stake.retire_time,
            };
            self.inactive_stake_by_address.insert(staker_address.clone(), new_inactive_stake);
        } else {
            let receipt = receipt.ok_or(AccountError::InvalidReceipt)?;
            let new_inactive_stake = InactiveStake {
                balance: total_value,
                retire_time: receipt.retire_time,
            };
            self.inactive_stake_by_address.insert(staker_address.clone(), new_inactive_stake);
        }
        Ok(())
    }

    /// Retrieves the size-bounded list of active validators.
    // FIXME naming
    pub fn build_validator_set(&self, seed: &BlsSignature, max_considered: usize, max_picked: u32) -> ValidatorSet {
        let mut potential_validators = Vec::new();
        let mut min_stake = Coin::ZERO;
        let mut total_stake = Coin::ZERO;

        let n_considered = min(self.active_stake_by_address.len(), max_considered);
        let n_picked = min(n_considered, max_picked as usize);

        // Build potential validator set and find minimum stake.
        // Iterate from highest balance to lowest.
        for validator in self.active_stake_sorted.iter() {
            if validator.balance <= min_stake
                || potential_validators.len() == n_considered {
                break;
            }

            total_stake += validator.balance;
            min_stake = Coin::from_u64_unchecked(u64::from(total_stake) / n_considered as u64);
            potential_validators.push(Arc::clone(validator));
        }

        // Build lookup tree of all potential validators
        let mut weights: Vec<(Address, u64)> = potential_validators.iter()
            .map(|stake| (stake.staker_address.clone(), stake.balance.into())).collect();
        let lookup = SegmentTree::new(&mut weights);

        // Build active validator set: Use the VRF to pick validators
        let mut validator_keys = Vec::<ActiveValidator>::with_capacity(n_picked as usize);
        for i in 0..n_picked {
            // Hash seed and index
            let mut hash_state = Blake2bHasher::new();
            seed.serialize(&mut hash_state);
            hash_state.write(&i.to_be_bytes());
            let hash = hash_state.finish();

            // Get number from first 8 bytes
            let mut num_bytes = [0u8; 8];
            num_bytes.copy_from_slice(&hash.as_bytes()[..8]);
            let num = u64::from_be_bytes(num_bytes);

            let index = num % u64::from(lookup.range());
            let staking_address = lookup.find(index).unwrap();
            let active_stake = &self.active_stake_by_address[&staking_address];
            validator_keys.push(ActiveValidator {
                staking_address:    active_stake.staker_address.clone(),
                reward_address_opt: active_stake.reward_address.as_ref().map(|s| s.clone())
            });
        }
        ValidatorSet {
            active: validator_keys,
            min_required_stake: min_stake,
        }
    }

    fn get_signer(transaction: &Transaction) -> Result<Address, AccountError> {
        let signature_proof: SignatureProof = Deserialize::deserialize(&mut &transaction.proof[..])?;
        Ok(signature_proof.compute_signer())
    }
}

impl AccountTransactionInteraction for StakingContract {
    fn new_contract(_: AccountType, _: Coin, _: &Transaction, _: u32) -> Result<Self, AccountError> {
        Err(AccountError::InvalidForRecipient)
    }

    fn create(_: Coin, _: &Transaction, _: u32) -> Result<Self, AccountError> {
        Err(AccountError::InvalidForRecipient)
    }

    fn check_incoming_transaction(&self, _: &Transaction, _: u32) -> Result<(), AccountError> {
        Ok(())
    }

    fn commit_incoming_transaction(&mut self, transaction: &Transaction, block_height: u32) -> Result<Option<Vec<u8>>, AccountError> {
        if transaction.sender != transaction.recipient {
            // Stake transaction
            let data = StakingTransactionData::parse(transaction)?;
            Ok(self.stake(&transaction.sender, transaction.value, data.validator_key, data.reward_address)?
                .map(|receipt| receipt.serialize_to_vec()))
        } else {
            // Retire transaction
            // XXX Get staker address from transaction proof. This violates the model that only the
            // sender account should evaluate the proof. However, retire is a self transaction, so
            // this contract is both sender and receiver.
            let staker_address = Self::get_signer(transaction)?;
            Ok(self.retire_recipient(&staker_address, transaction.value, block_height)?
                   .map(|receipt| receipt.serialize_to_vec()))
        }
    }

    fn revert_incoming_transaction(&mut self, transaction: &Transaction, _block_height: u32, receipt: Option<&Vec<u8>>) -> Result<(), AccountError> {
        if transaction.sender != transaction.recipient {
            // Stake transaction
            let receipt = match receipt {
                Some(v) => Some(Deserialize::deserialize_from_vec(v)?),
                _ => None
            };
            self.revert_stake(&transaction.sender, transaction.value, receipt)
        } else {
            // Retire transaction
            let staker_address = Self::get_signer(transaction)?;
            let receipt = match receipt {
                Some(v) => Some(Deserialize::deserialize_from_vec(v)?),
                _ => None
            };
            self.revert_retire_recipient(&staker_address, transaction.value, receipt)
        }
    }

    fn check_outgoing_transaction(&self, transaction: &Transaction, block_height: u32) -> Result<(), AccountError> {
        let staker_address = Self::get_signer(transaction)?;
        if transaction.sender != transaction.recipient {
            // Unstake transaction
            let inactive_stake = self.inactive_stake_by_address.get(&staker_address)
                .ok_or(AccountError::InvalidForSender)?;

            // Check unstake delay.
            if block_height < policy::next_macro_block(inactive_stake.retire_time) + policy::UNSTAKE_DELAY {
                return Err(AccountError::InvalidForSender);
            }

            Account::balance_sufficient(inactive_stake.balance, transaction.total_value()?)
        } else {
            // Retire transaction
            let active_stake = self.active_stake_by_address.get(&staker_address)
                .ok_or(AccountError::InvalidForSender)?;

            Account::balance_sufficient(active_stake.balance, transaction.total_value()?)
        }
    }

    fn commit_outgoing_transaction(&mut self, transaction: &Transaction, block_height: u32) -> Result<Option<Vec<u8>>, AccountError> {
        self.check_outgoing_transaction(transaction, block_height)?;

        let staker_address = Self::get_signer(transaction)?;
        if transaction.sender != transaction.recipient {
            // Unstake transaction
            Ok(self.unstake(&staker_address, transaction.total_value()?)?
                .map(|receipt| receipt.serialize_to_vec()))
        } else {
            // Retire transaction
            Ok(self.retire_sender(&staker_address, transaction.total_value()?, block_height)?
                .map(|receipt| receipt.serialize_to_vec()))
        }
    }

    fn revert_outgoing_transaction(&mut self, transaction: &Transaction, _block_height: u32, receipt: Option<&Vec<u8>>) -> Result<(), AccountError> {
        let staker_address = Self::get_signer(transaction)?;

        if transaction.sender != transaction.recipient {
            // Unstake transaction
            let receipt = match receipt {
                Some(v) => Some(Deserialize::deserialize_from_vec(v)?),
                _ => None
            };
            self.revert_unstake(&staker_address, transaction.total_value()?, receipt)
        } else {
            // Retire transaction
            let receipt = match receipt {
                Some(v) => Some(Deserialize::deserialize_from_vec(v)?),
                _ => None
            };
            self.revert_retire_sender(&staker_address, transaction.total_value()?, receipt)
        }
    }
}

impl AccountInherentInteraction for StakingContract {
    fn check_inherent(&self, inherent: &Inherent) -> Result<(), AccountError> {
        match inherent.ty {
            InherentType::Slash => {
                unimplemented!()
            },
            InherentType::Reward => Err(AccountError::InvalidInherent)
        }
    }

    fn commit_inherent(&mut self, inherent: &Inherent) -> Result<Option<Vec<u8>>, AccountError> {
        unimplemented!()
    }

    fn revert_inherent(&mut self, inherent: &Inherent, receipt: Option<&Vec<u8>>) -> Result<(), AccountError> {
        unimplemented!()
    }
}

impl Serialize for StakingContract {
    fn serialize<W: WriteBytesExt>(&self, writer: &mut W) -> Result<usize, SerializingError> {
        let mut size = 0;
        size += Serialize::serialize(&self.balance, writer)?;

        size += Serialize::serialize(&(self.active_stake_sorted.len() as u32), writer)?;
        for active_stake in self.active_stake_sorted.iter() {
            let inactive_stake = self.inactive_stake_by_address.get(&active_stake.staker_address);
            size += Serialize::serialize(active_stake, writer)?;
            size += Serialize::serialize(&inactive_stake, writer)?;
        }

        // Collect remaining inactive stakes.
        let mut inactive_stakes = Vec::new();
        for (staker_address, inactive_stake) in self.inactive_stake_by_address.iter() {
            if !self.active_stake_by_address.contains_key(staker_address) {
                inactive_stakes.push((staker_address, inactive_stake));
            }
        }
        inactive_stakes.sort_by(|a, b|a.0.cmp(b.0)
            .then_with(|| a.1.balance.cmp(&b.1.balance))
            .then_with(|| a.1.retire_time.cmp(&b.1.retire_time)));

        size += Serialize::serialize(&(inactive_stakes.len() as u32), writer)?;
        for (staker_address, inactive_stake) in inactive_stakes {
            size += Serialize::serialize(staker_address, writer)?;
            size += Serialize::serialize(inactive_stake, writer)?;
        }

        Ok(size)
    }

    fn serialized_size(&self) -> usize {
        let mut size = 0;
        size += Serialize::serialized_size(&self.balance);

        size += Serialize::serialized_size(&0u32);
        for active_stake in self.active_stake_sorted.iter() {
            let inactive_stake = self.inactive_stake_by_address.get(&active_stake.staker_address);
            size += Serialize::serialized_size(active_stake);
            size += Serialize::serialized_size(&inactive_stake);
        }

        size += Serialize::serialized_size(&0u32);
        for (staker_address, inactive_stake) in self.inactive_stake_by_address.iter() {
            if !self.active_stake_by_address.contains_key(staker_address) {
                size += Serialize::serialized_size(staker_address);
                size += Serialize::serialized_size(inactive_stake);
            }
        }

        size
    }
}

impl Deserialize for StakingContract {
    fn deserialize<R: ReadBytesExt>(reader: &mut R) -> Result<Self, SerializingError> {
        let balance = Deserialize::deserialize(reader)?;

        let mut active_stake_sorted = BTreeSet::new();
        let mut active_stake_by_address = HashMap::new();
        let mut inactive_stake_by_address = HashMap::new();

        let num_active_stakes: u32 = Deserialize::deserialize(reader)?;
        for _ in 0..num_active_stakes {
            let active_stake: Arc<ActiveStake> = Deserialize::deserialize(reader)?;
            let inactive_stake: Option<InactiveStake> = Deserialize::deserialize(reader)?;

            active_stake_sorted.insert(Arc::clone(&active_stake));
            active_stake_by_address.insert(active_stake.staker_address.clone(), Arc::clone(&active_stake));

            if inactive_stake.is_some() {
                inactive_stake_by_address.insert(active_stake.staker_address.clone(), inactive_stake.unwrap());
            }
        }

        let num_inactive_stakes: u32 = Deserialize::deserialize(reader)?;
        for _ in 0..num_inactive_stakes {
            let staker_address = Deserialize::deserialize(reader)?;
            let inactive_stake = Deserialize::deserialize(reader)?;
            inactive_stake_by_address.insert(staker_address, inactive_stake);
        }

        Ok(StakingContract {
            balance,
            active_stake_sorted,
            active_stake_by_address,
            inactive_stake_by_address,
        })
    }
}

// Not really useful traits for StakingContracts.
// FIXME Assume a single staking contract for now, i.e. all staking contracts are equal.
impl PartialEq for StakingContract {
    fn eq(&self, _other: &StakingContract) -> bool {
        true
    }
}

impl Eq for StakingContract {}

impl PartialOrd for StakingContract {
    fn partial_cmp(&self, other: &StakingContract) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StakingContract {
    fn cmp(&self, _other: &Self) -> Ordering {
        Ordering::Equal
    }
}

impl Default for StakingContract {
    fn default() -> Self {
        StakingContract {
            balance: Coin::ZERO,
            active_stake_sorted: BTreeSet::new(),
            active_stake_by_address: HashMap::new(),
            inactive_stake_by_address: HashMap::new()
        }
    }
}


#[test]
fn it_can_de_serialize_an_active_stake_receipt() {
    const ACTIVE_STAKE_RECEIPT: &str = "96b94e8a2fa79cb3d96bfde5ed2fa693aa6bec225e944b23c96b1c83dda67b34b62d105763bdf3cd378de9e4d8809fb00f815e309ec94126f22d77ef81fe00fa3a51a6c750349efda2133ca2f0e1b04094c4e2ce08b73c72fccedc33e127259f010303030303030303030303030303030303030303";
    const BLS_PUBLIC_KEY: &str = "96b94e8a2fa79cb3d96bfde5ed2fa693aa6bec225e944b23c96b1c83dda67b34b62d105763bdf3cd378de9e4d8809fb00f815e309ec94126f22d77ef81fe00fa3a51a6c750349efda2133ca2f0e1b04094c4e2ce08b73c72fccedc33e127259f";

    let bytes: Vec<u8> = hex::decode(ACTIVE_STAKE_RECEIPT).unwrap();
    let asr: ActiveStakeReceipt = Deserialize::deserialize(&mut &bytes[..]).unwrap();
    let bls_bytes: Vec<u8> = hex::decode(BLS_PUBLIC_KEY).unwrap();
    let bls_pubkey: BlsPublicKey = Deserialize::deserialize(&mut &bls_bytes[..]).unwrap();
    assert_eq!(asr.validator_key, bls_pubkey);

    assert_eq!(hex::encode(asr.serialize_to_vec()), ACTIVE_STAKE_RECEIPT);
}
