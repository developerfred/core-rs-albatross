use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::collections::btree_set::BTreeSet;
use std::mem;
use std::sync::Arc;

use beserial::{Deserialize, DeserializeWithLength, ReadBytesExt, Serialize, SerializeWithLength, SerializingError, WriteBytesExt};
use bls::bls12_381::CompressedPublicKey as BlsPublicKey;
use keys::Address;
use primitives::{policy, coin::Coin};
use primitives::slot::{Slots, SlotsBuilder};
use transaction::{SignatureProof, Transaction};
use transaction::account::staking_contract::{StakingTransactionData, StakingTransactionType};
use vrf::{VrfSeed, VrfUseCase, AliasMethod};

use crate::{Account, AccountError, AccountTransactionInteraction, AccountType};
use crate::inherent::{AccountInherentInteraction, Inherent, InherentType};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveStake {
    pub staker_address: Address,
    pub balance: Coin,
    pub validator_key: BlsPublicKey, // TODO Share validator keys eventually and if required
    pub reward_address: Option<Address>,
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

impl ActiveStake {
    pub fn with_balance(&self, balance: Coin) -> Self {
        ActiveStake {
            staker_address: self.staker_address.clone(),
            balance,
            validator_key: self.validator_key.clone(),
            reward_address: self.reward_address.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct InactiveStake {
    pub balance: Coin,
    pub retire_time: u32,
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

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
struct UnparkReceipt {
    current_epoch: bool,
    previous_epoch: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
struct SlashReceipt {
    newly_slashed: bool,
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
    pub current_epoch_parking: HashSet<Address>,
    pub previous_epoch_parking: HashSet<Address>,
}

impl StakingContract {
    pub fn get_balance(&self, staker_address: &Address) -> Coin {
        self.get_active_balance(staker_address) + self.get_inactive_balance(staker_address)
    }

    pub fn get_active_balance(&self, staker_address: &Address) -> Coin {
        self.active_stake_by_address.get(staker_address).map(|stake| stake.balance).unwrap_or(Coin::ZERO)
    }

    pub fn get_inactive_balance(&self, staker_address: &Address) -> Coin {
        self.inactive_stake_by_address.get(staker_address).map(|stake| stake.balance).unwrap_or(Coin::ZERO)
    }

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

    /// Removes a staker from the parking lists.
    fn unpark_sender(&mut self, staker_address: &Address, total_value: Coin, fee: Coin) -> Result<(), AccountError> {
        self.balance = Account::balance_sub(self.balance, total_value)?;

        let active_stake = self.active_stake_by_address.remove(staker_address)
            .ok_or(AccountError::InvalidForSender)?;

        self.active_stake_sorted.remove(&active_stake);

        // Check total value.
        if active_stake.balance != total_value {
            return Err(AccountError::InvalidForSender);
        }

        // Then deduct fee.
        let new_active_stake = Arc::new(active_stake.with_balance(Account::balance_sub(active_stake.balance, fee)?));

        self.active_stake_sorted.insert(Arc::clone(&new_active_stake));
        self.active_stake_by_address.insert(staker_address.clone(), new_active_stake);

        Ok(())
    }

    /// Reverts the sender side from an unparking transaction.
    fn revert_unpark_sender(&mut self, staker_address: &Address, total_value: Coin, fee: Coin) -> Result<(), AccountError> {
        self.balance = Account::balance_add(self.balance, total_value)?;

        let active_stake = self.active_stake_by_address.remove(staker_address)
            .ok_or(AccountError::InvalidForSender)?;

        self.active_stake_sorted.remove(&active_stake);

        // Then deduct fee.
        let new_active_stake = Arc::new(active_stake.with_balance(Account::balance_add(active_stake.balance, fee)?));

        // Check total value.
        if new_active_stake.balance != total_value {
            return Err(AccountError::InvalidForSender);
        }

        self.active_stake_sorted.insert(Arc::clone(&new_active_stake));
        self.active_stake_by_address.insert(staker_address.clone(), new_active_stake);

        Ok(())
    }

    /// Removes a staker from the unparking lists.
    fn unpark_recipient(&mut self, staker_address: &Address, value: Coin) -> Result<UnparkReceipt, AccountError> {
        self.balance = Account::balance_add(self.balance, value)?;

        let current_epoch = self.current_epoch_parking.remove(staker_address);
        let previous_epoch = self.previous_epoch_parking.remove(staker_address);

        if !current_epoch && !previous_epoch {
            return Err(AccountError::InvalidForRecipient);
        }

        Ok(UnparkReceipt {
            current_epoch,
            previous_epoch,
        })
    }

    /// Reverts the recipient side of an unparking transaction.
    fn revert_unpark_recipient(&mut self, staker_address: &Address, value: Coin, receipt: UnparkReceipt) -> Result<(), AccountError> {
        self.balance = Account::balance_sub(self.balance, value)?;

        if receipt.current_epoch {
            self.current_epoch_parking.insert(staker_address.clone());
        }

        if receipt.previous_epoch {
            self.previous_epoch_parking.insert(staker_address.clone());
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

    pub fn select_validators(&self, seed: &VrfSeed) -> Slots {
        // TODO: Depending on the circumstances and parameters, it might be more efficient to store active stake in an unsorted Vec.
        // Then, we would not need to create the Vec here. But then, removal of stake is a O(n) operation.
        // Assuming that validator selection happens less frequently than stake removal, the current implementation might be ok.
        let mut potential_validators = Vec::with_capacity(self.active_stake_sorted.len());
        let mut weights: Vec<u64> = Vec::with_capacity(self.active_stake_sorted.len());

        trace!("Select validators: num_slots = {}", policy::SLOTS);

        // NOTE: `active_stake_sorted` is sorted from highest to lowest stake. `LookupTable`
        // expects the reverse ordering.
        for validator in self.active_stake_sorted.iter() {
            potential_validators.push(Arc::clone(validator));
            weights.push(validator.balance.into());
        }

        let mut slots_builder = SlotsBuilder::default();
        let lookup = AliasMethod::new(weights);
        let mut rng = seed.rng(VrfUseCase::ValidatorSelection, 0);

        for _ in 0 .. policy::SLOTS {
            let index = lookup.sample(&mut rng);

            let active_stake = &potential_validators[index];

            slots_builder.push(
                active_stake.validator_key.clone(),
                active_stake.staker_address.clone(),
                active_stake.reward_address.clone()
            );
        }

        slots_builder.build()
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

    fn check_incoming_transaction(transaction: &Transaction, _: u32) -> Result<(), AccountError> {
        // Do all static checks here.
        if transaction.sender != transaction.recipient {
            // Stake transaction.
            StakingTransactionData::parse(transaction)?;
        } else {
            // For retire & unpark transactions, we need to check a valid flag in the data field.
            let ty: StakingTransactionType = Deserialize::deserialize(&mut &transaction.data[..])?;

            if transaction.data.len() != ty.serialized_size() {
                return Err(AccountError::InvalidForTarget);
            }
        }
        Ok(())
    }

    fn commit_incoming_transaction(&mut self, transaction: &Transaction, block_height: u32) -> Result<Option<Vec<u8>>, AccountError> {
        if transaction.sender != transaction.recipient {
            // Stake transaction
            let data = StakingTransactionData::parse(transaction)?;
            Ok(self.stake(&transaction.sender, transaction.value, data.validator_key, data.reward_address)?
                .map(|receipt| receipt.serialize_to_vec()))
        } else {
            let ty: StakingTransactionType = Deserialize::deserialize(&mut &transaction.data[..])?;
            // XXX Get staker address from transaction proof. This violates the model that only the
            // sender account should evaluate the proof. However, retire/unpark are self transactions, so
            // this contract is both sender and receiver.
            let staker_address = Self::get_signer(transaction)?;

            match ty {
                StakingTransactionType::Retire => {
                    // Retire transaction.
                    Ok(self.retire_recipient(&staker_address, transaction.value, block_height)?
                           .map(|receipt| receipt.serialize_to_vec()))
                },
                StakingTransactionType::Unpark => {
                    Ok(Some(self.unpark_recipient(&staker_address, transaction.value)?.serialize_to_vec()))
                },
            }
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
            let ty: StakingTransactionType = Deserialize::deserialize(&mut &transaction.data[..])?;
            let staker_address = Self::get_signer(transaction)?;

            match ty {
                StakingTransactionType::Retire => {
                    // Retire transaction.
                    let receipt = match receipt {
                        Some(v) => Some(Deserialize::deserialize_from_vec(v)?),
                        _ => None
                    };
                    self.revert_retire_recipient(&staker_address, transaction.value, receipt)
                },
                StakingTransactionType::Unpark => {
                    let receipt = Deserialize::deserialize_from_vec(receipt.ok_or(AccountError::InvalidReceipt)?)?;
                    self.revert_unpark_recipient(&staker_address, transaction.value, receipt)
                },
            }
        }
    }

    fn check_outgoing_transaction(&self, transaction: &Transaction, block_height: u32) -> Result<(), AccountError> {
        let staker_address = Self::get_signer(transaction)?;
        if transaction.sender != transaction.recipient {
            // Unstake transaction
            let inactive_stake = self.inactive_stake_by_address.get(&staker_address)
                .ok_or(AccountError::InvalidForSender)?;

            // Check unstake delay.
            if block_height < policy::macro_block_after(inactive_stake.retire_time) + policy::UNSTAKING_DELAY {
                return Err(AccountError::InvalidForSender);
            }

            Account::balance_sufficient(inactive_stake.balance, transaction.total_value()?)
        } else {
            let ty: StakingTransactionType = Deserialize::deserialize(&mut &transaction.data[..])?;

            let active_stake = self.active_stake_by_address.get(&staker_address)
                .ok_or(AccountError::InvalidForSender)?;

            match ty {
                StakingTransactionType::Retire => {
                    // Retire transaction.
                    Account::balance_sufficient(active_stake.balance, transaction.total_value()?)
                },
                StakingTransactionType::Unpark => {
                    if active_stake.balance != transaction.total_value()? {
                        return Err(AccountError::InvalidForSender);
                    }

                    if !self.current_epoch_parking.contains(&staker_address) && !self.previous_epoch_parking.contains(&staker_address) {
                        return Err(AccountError::InvalidForSender);
                    }
                    Ok(())
                },
            }
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
            let ty: StakingTransactionType = Deserialize::deserialize(&mut &transaction.data[..])?;

            match ty {
                StakingTransactionType::Retire => {
                    // Retire transaction.
                    Ok(self.retire_sender(&staker_address, transaction.total_value()?, block_height)?
                        .map(|receipt| receipt.serialize_to_vec()))
                },
                StakingTransactionType::Unpark => {
                    self.unpark_sender(&staker_address, transaction.total_value()?, transaction.fee)?;
                    Ok(None)
                },
            }
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
            let ty: StakingTransactionType = Deserialize::deserialize(&mut &transaction.data[..])?;

            match ty {
                StakingTransactionType::Retire => {
                    // Retire transaction.
                    let receipt = match receipt {
                        Some(v) => Some(Deserialize::deserialize_from_vec(v)?),
                        _ => None
                    };
                    self.revert_retire_sender(&staker_address, transaction.total_value()?, receipt)
                },
                StakingTransactionType::Unpark => {
                    self.revert_unpark_sender(&staker_address, transaction.total_value()?, transaction.fee)
                },
            }
        }
    }
}

impl AccountInherentInteraction for StakingContract {
    fn check_inherent(&self, inherent: &Inherent, _block_height: u32) -> Result<(), AccountError> {
        trace!("check inherent: {:?}", inherent);
        // Inherent slashes nothing
        if inherent.value != Coin::ZERO {
            return Err(AccountError::InvalidInherent);
        }

        match inherent.ty {
            InherentType::Slash => {
                // Invalid data length
                if inherent.data.len() != Address::SIZE {
                    return Err(AccountError::InvalidInherent);
                }

                // Address doesn't exist in contract
                let staker_address: Address = Deserialize::deserialize(&mut &inherent.data[..])?;
                if !self.active_stake_by_address.contains_key(&staker_address) && !self.inactive_stake_by_address.contains_key(&staker_address) {
                    return Err(AccountError::InvalidInherent);
                }

                Ok(())
            },
            InherentType::FinalizeEpoch => {
                // Invalid data length
                if !inherent.data.is_empty() {
                    return Err(AccountError::InvalidInherent);
                }

                Ok(())
            },
            InherentType::Reward => Err(AccountError::InvalidForTarget)
        }
    }

    fn commit_inherent(&mut self, inherent: &Inherent, block_height: u32) -> Result<Option<Vec<u8>>, AccountError> {
        self.check_inherent(inherent, block_height)?;

        match &inherent.ty {
            InherentType::Slash => {
                // Simply add staker address to parking.
                let staker_address: Address = Deserialize::deserialize(&mut &inherent.data[..])?;
                // TODO: The inherent might have originated from a fork proof for the previous epoch.
                // Right now, we don't care and start the parking period in the epoch the proof has been submitted.
                let newly_slashed = self.current_epoch_parking.insert(staker_address);
                let receipt = SlashReceipt { newly_slashed };
                Ok(Some(receipt.serialize_to_vec()))
            },
            InherentType::FinalizeEpoch => {
                // Swap lists around.
                let current_epoch = mem::replace(&mut self.current_epoch_parking, HashSet::new());
                let old_epoch = mem::replace(&mut self.previous_epoch_parking, current_epoch);

                // Remove all parked stakers.
                for address in old_epoch {
                    let balance = self.get_active_balance(&address);
                    // We do not remove stakers from the parking list if they send a retire transaction.
                    // Instead, we simply skip these here.
                    // This saves space in the receipts of retire transactions as they happen much more often
                    // than stakers are added to the parking lists.
                    if balance > Coin::ZERO {
                        self.retire_sender(&address, balance, block_height)?;
                        self.retire_recipient(&address, balance, block_height)?;
                    }
                }

                // Since finalized epochs cannot be reverted, we don't need any receipts.
                Ok(None)
            },
            _ => unreachable!(),
        }
    }

    fn revert_inherent(&mut self, inherent: &Inherent, _block_height: u32, receipt: Option<&Vec<u8>>) -> Result<(), AccountError> {
        match &inherent.ty {
            InherentType::Slash => {
                let receipt: SlashReceipt = Deserialize::deserialize_from_vec(&receipt.ok_or(AccountError::InvalidReceipt)?)?;
                let staker_address: Address = Deserialize::deserialize(&mut &inherent.data[..])?;

                // Only remove if it was not already slashed.
                // I kept this in two nested if's for clarity.
                if receipt.newly_slashed {
                    let has_been_removed = self.current_epoch_parking.remove(&staker_address);
                    if !has_been_removed {
                        return Err(AccountError::InvalidInherent);
                    }
                }
            },
            InherentType::FinalizeEpoch => {
                // We should not be able to revert finalized epochs!
                return Err(AccountError::InvalidForTarget);
            },
            _ => unreachable!(),
        }

        Ok(())
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

        size += SerializeWithLength::serialize::<u32, _>(&self.current_epoch_parking, writer)?;
        size += SerializeWithLength::serialize::<u32, _>(&self.previous_epoch_parking, writer)?;

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

        size += SerializeWithLength::serialized_size::<u32>(&self.current_epoch_parking);
        size += SerializeWithLength::serialized_size::<u32>(&self.previous_epoch_parking);

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

            if let Some(stake) = inactive_stake {
                inactive_stake_by_address.insert(active_stake.staker_address.clone(), stake);
            }
        }

        let num_inactive_stakes: u32 = Deserialize::deserialize(reader)?;
        for _ in 0..num_inactive_stakes {
            let staker_address = Deserialize::deserialize(reader)?;
            let inactive_stake = Deserialize::deserialize(reader)?;
            inactive_stake_by_address.insert(staker_address, inactive_stake);
        }

        let current_epoch_parking: HashSet<Address> = DeserializeWithLength::deserialize::<u32, _>(reader)?;
        let last_epoch_parking: HashSet<Address> = DeserializeWithLength::deserialize::<u32, _>(reader)?;

        Ok(StakingContract {
            balance,
            active_stake_sorted,
            active_stake_by_address,
            inactive_stake_by_address,
            current_epoch_parking,
            previous_epoch_parking: last_epoch_parking
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
            inactive_stake_by_address: HashMap::new(),
            current_epoch_parking: HashSet::new(),
            previous_epoch_parking: HashSet::new(),
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
