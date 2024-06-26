//! Write log is temporary storage for modifications performed by a transaction.
//! before they are committed to the ledger's storage.

use std::collections::{BTreeMap, BTreeSet};

use itertools::Itertools;
use namada_core::address::{Address, EstablishedAddressGen};
use namada_core::collections::{HashMap, HashSet};
use namada_core::hash::Hash;
use namada_core::ibc::IbcEvent;
use namada_core::storage;
use namada_gas::{MEMORY_ACCESS_GAS_PER_BYTE, STORAGE_WRITE_GAS_PER_BYTE};
use thiserror::Error;

#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum Error {
    #[error("Storage error applying a write log: {0}")]
    StorageError(crate::Error),
    #[error("Trying to update a temporary value")]
    UpdateTemporaryValue,
    #[error(
        "Trying to update a validity predicate that a new account that's not \
         yet committed to storage"
    )]
    UpdateVpOfNewAccount,
    #[error("Trying to delete a validity predicate")]
    DeleteVp,
    #[error("Trying to write a temporary value after deleting")]
    WriteTempAfterDelete,
    #[error("Trying to write a temporary value after writing")]
    WriteTempAfterWrite,
    #[error("Replay protection key: {0}")]
    ReplayProtection(String),
}

/// Result for functions that may fail
pub type Result<T> = std::result::Result<T, Error>;

/// A storage modification
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageModification {
    /// Write a new value
    Write {
        /// Value bytes
        value: Vec<u8>,
    },
    /// Delete an existing key-value
    Delete,
    /// Initialize a new account with established address and a given validity
    /// predicate hash. The key for `InitAccount` inside the [`WriteLog`] must
    /// point to its validity predicate.
    InitAccount {
        /// Validity predicate hash bytes
        vp_code_hash: Hash,
    },
}

/// The write log storage
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteLog {
    /// The generator of established addresses
    pub(crate) address_gen: Option<EstablishedAddressGen>,
    /// All the storage modification accepted by validity predicates are stored
    /// in block write-log, before being committed to the storage
    pub(crate) block_write_log: HashMap<storage::Key, StorageModification>,
    /// The storage modifications for the current transaction
    pub(crate) tx_write_log: HashMap<storage::Key, StorageModification>,
    /// Temporary key-values for the current transaction that are dropped after
    /// tx and its verifying VPs execution is done
    pub(crate) tx_temp_log: HashMap<storage::Key, Vec<u8>>,
    /// A precommit bucket for the `tx_write_log`. This is useful for
    /// validation when a clean `tx_write_log` is needed without committing any
    /// modification already in there. These modifications can be temporarily
    /// stored here and then discarded or committed to the `block_write_log`,
    /// together with th content of `tx_write_log`. No direct key
    /// write/update/delete should ever happen on this field, this log should
    /// only be populated through a dump of the `tx_write_log` and should be
    /// cleaned either when committing or dumping the `tx_write_log`
    pub(crate) tx_precommit_write_log:
        HashMap<storage::Key, StorageModification>,
    /// The IBC events for the current transaction
    pub(crate) ibc_events: BTreeSet<IbcEvent>,
    /// Storage modifications for the replay protection storage, always
    /// committed regardless of the result of the transaction
    pub(crate) replay_protection: HashSet<Hash>,
}

/// Write log prefix iterator
#[derive(Debug)]
pub struct PrefixIter {
    /// The concrete iterator for modifications sorted by storage keys
    pub iter:
        std::collections::btree_map::IntoIter<String, StorageModification>,
}

impl Iterator for PrefixIter {
    type Item = (String, StorageModification);

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next()
    }
}

impl Default for WriteLog {
    fn default() -> Self {
        Self {
            address_gen: None,
            block_write_log: HashMap::with_capacity(100_000),
            tx_write_log: HashMap::with_capacity(100),
            tx_temp_log: HashMap::with_capacity(1),
            tx_precommit_write_log: HashMap::with_capacity(100),
            ibc_events: BTreeSet::new(),
            replay_protection: HashSet::with_capacity(1_000),
        }
    }
}

impl WriteLog {
    /// Read a non-temp value at the given key and return the value and the gas
    /// cost, returns [`None`] if the key is not present in the write log
    pub fn read(
        &self,
        key: &storage::Key,
    ) -> (Option<&StorageModification>, u64) {
        // try to read from tx write log first
        match self
            .tx_write_log
            .get(key)
            .or_else(|| {
                // If not found, then try to read from tx precommit write log
                self.tx_precommit_write_log.get(key)
            })
            .or_else(|| {
                // if not found, then try to read from block write log
                self.block_write_log.get(key)
            }) {
            Some(v) => {
                let gas = match v {
                    StorageModification::Write { ref value } => {
                        key.len() + value.len()
                    }
                    StorageModification::Delete => key.len(),
                    StorageModification::InitAccount { ref vp_code_hash } => {
                        key.len() + vp_code_hash.len()
                    }
                };
                (Some(v), gas as u64 * MEMORY_ACCESS_GAS_PER_BYTE)
            }
            None => (None, key.len() as u64 * MEMORY_ACCESS_GAS_PER_BYTE),
        }
    }

    /// Read a value before the latest tx execution at the given key and return
    /// the value and the gas cost, returns [`None`] if the key is not present
    /// in the write log
    pub fn read_pre(
        &self,
        key: &storage::Key,
    ) -> (Option<&StorageModification>, u64) {
        match self.block_write_log.get(key) {
            Some(v) => {
                let gas = match v {
                    StorageModification::Write { ref value } => {
                        key.len() + value.len()
                    }
                    StorageModification::Delete => key.len(),
                    StorageModification::InitAccount { ref vp_code_hash } => {
                        key.len() + vp_code_hash.len()
                    }
                };
                (Some(v), gas as u64 * MEMORY_ACCESS_GAS_PER_BYTE)
            }
            None => (None, key.len() as u64 * MEMORY_ACCESS_GAS_PER_BYTE),
        }
    }

    /// Read a temp value at the given key and return the value and the gas
    /// cost, returns [`None`] if the key is not present in the temp write
    /// log
    pub fn read_temp(&self, key: &storage::Key) -> (Option<&Vec<u8>>, u64) {
        // try to read from tx write log first
        match self.tx_temp_log.get(key) {
            Some(value) => {
                let gas = key.len() + value.len();

                (Some(value), gas as u64 * MEMORY_ACCESS_GAS_PER_BYTE)
            }
            None => (None, key.len() as u64 * MEMORY_ACCESS_GAS_PER_BYTE),
        }
    }

    /// Write a key and a value and return the gas cost and the size difference
    /// Fails with [`Error::UpdateVpOfNewAccount`] when attempting to update a
    /// validity predicate of a new account that's not yet committed to storage.
    /// Fails with [`Error::UpdateTemporaryValue`] when attempting to update a
    /// temporary value.
    pub fn write(
        &mut self,
        key: &storage::Key,
        value: Vec<u8>,
    ) -> Result<(u64, i64)> {
        let len = value.len();
        let gas = key.len() + len;
        if self.tx_temp_log.contains_key(key) {
            return Err(Error::UpdateTemporaryValue);
        }
        let size_diff = match self
            .tx_write_log
            .get(key)
            .or_else(|| self.tx_precommit_write_log.get(key))
        {
            Some(prev) => match prev {
                StorageModification::Write { ref value } => {
                    len as i64 - value.len() as i64
                }
                StorageModification::Delete => len as i64,
                StorageModification::InitAccount { .. } => {
                    // NOTE: errors from host functions force a shudown of the
                    // wasm environment without the need for cooperation from
                    // the wasm code (tx or vp), so there's no need to return
                    // gas in case of an error because execution will terminate
                    // anyway and this cannot be exploited to run the vm forever
                    return Err(Error::UpdateVpOfNewAccount);
                }
            },
            // set just the length of the value because we don't know if
            // the previous value exists on the storage
            None => len as i64,
        };

        self.tx_write_log
            .insert(key.clone(), StorageModification::Write { value });

        Ok((gas as u64 * STORAGE_WRITE_GAS_PER_BYTE, size_diff))
    }

    /// Write a key and a value.
    /// Fails with [`Error::UpdateVpOfNewAccount`] when attempting to update a
    /// validity predicate of a new account that's not yet committed to storage.
    /// Fails with [`Error::UpdateTemporaryValue`] when attempting to update a
    /// temporary value.
    pub fn protocol_write(
        &mut self,
        key: &storage::Key,
        value: Vec<u8>,
    ) -> Result<()> {
        if self.tx_temp_log.contains_key(key) {
            return Err(Error::UpdateTemporaryValue);
        }
        if let Some(prev) = self
            .block_write_log
            .insert(key.clone(), StorageModification::Write { value })
        {
            match prev {
                StorageModification::InitAccount { .. } => {
                    return Err(Error::UpdateVpOfNewAccount);
                }
                StorageModification::Write { .. }
                | StorageModification::Delete => {}
            }
        }
        Ok(())
    }

    /// Write a key and a value and return the gas cost and the size difference
    /// Fails with [`Error::WriteTempAfterWrite`] when attempting to update a
    /// temporary value after writing.
    /// Fails with [`Error::UpdateVpOfNewAccount`] when attempting to update a
    /// validity predicate of a new account that's not yet committed to storage.
    /// Fails with [`Error::WriteTempAfterDelete`] when attempting to update a
    /// temporary value after deleting.
    pub fn write_temp(
        &mut self,
        key: &storage::Key,
        value: Vec<u8>,
    ) -> Result<(u64, i64)> {
        if let Some(prev) = self
            .tx_write_log
            .get(key)
            .or_else(|| self.tx_precommit_write_log.get(key))
        {
            match prev {
                StorageModification::Write { .. } => {
                    // Cannot overwrite a write request with a temporary one
                    return Err(Error::WriteTempAfterWrite);
                }
                StorageModification::Delete => {
                    return Err(Error::WriteTempAfterDelete);
                }
                StorageModification::InitAccount { .. } => {
                    return Err(Error::UpdateVpOfNewAccount);
                }
            }
        }

        let len = value.len();
        let gas = key.len() + len;
        let size_diff = match self.tx_temp_log.get(key) {
            Some(prev) => len as i64 - prev.len() as i64,
            // set just the length of the value because we don't know if
            // the previous value exists on the storage
            None => len as i64,
        };

        self.tx_temp_log.insert(key.clone(), value);

        // Temp writes are not propagated to db so just charge the cost of
        // accessing storage
        Ok((gas as u64 * MEMORY_ACCESS_GAS_PER_BYTE, size_diff))
    }

    /// Delete a key and its value, and return the gas cost and the size
    /// difference.
    /// Fails with [`Error::DeleteVp`] for a validity predicate key, which are
    /// not possible to delete.
    pub fn delete(&mut self, key: &storage::Key) -> Result<(u64, i64)> {
        if key.is_validity_predicate().is_some() {
            return Err(Error::DeleteVp);
        }
        let size_diff = match self
            .tx_write_log
            .get(key)
            .or_else(|| self.tx_precommit_write_log.get(key))
        {
            Some(prev) => match prev {
                StorageModification::Write { ref value } => value.len() as i64,
                StorageModification::Delete => 0,
                StorageModification::InitAccount { .. } => {
                    return Err(Error::DeleteVp);
                }
            },
            // set 0 because we don't know if the previous value exists on the
            // storage
            None => 0,
        };

        self.tx_write_log
            .insert(key.clone(), StorageModification::Delete);
        let gas = key.len() + size_diff as usize;
        Ok((gas as u64 * STORAGE_WRITE_GAS_PER_BYTE, -size_diff))
    }

    /// Delete a key and its value.
    /// Fails with [`Error::DeleteVp`] for a validity predicate key, which are
    /// not possible to delete.
    pub fn protocol_delete(&mut self, key: &storage::Key) -> Result<()> {
        if key.is_validity_predicate().is_some() {
            return Err(Error::DeleteVp);
        }
        if let Some(prev) = self
            .block_write_log
            .insert(key.clone(), StorageModification::Delete)
        {
            match prev {
                StorageModification::InitAccount { .. } => {
                    return Err(Error::DeleteVp);
                }
                StorageModification::Write { .. }
                | StorageModification::Delete => {}
            }
        };
        Ok(())
    }

    /// Initialize a new account and return the gas cost.
    pub fn init_account(
        &mut self,
        storage_address_gen: &EstablishedAddressGen,
        vp_code_hash: Hash,
        entropy_source: &[u8],
    ) -> (Address, u64) {
        // If we've previously generated a new account, we use the local copy of
        // the generator. Otherwise, we create a new copy from the storage
        let address_gen = self
            .address_gen
            .get_or_insert_with(|| storage_address_gen.clone());
        let addr = address_gen.generate_address(entropy_source);
        let key = storage::Key::validity_predicate(&addr);
        let gas = (key.len() + vp_code_hash.len()) as u64
            * STORAGE_WRITE_GAS_PER_BYTE;
        self.tx_write_log
            .insert(key, StorageModification::InitAccount { vp_code_hash });
        (addr, gas)
    }

    /// Set an IBC event and return the gas cost.
    pub fn emit_ibc_event(&mut self, event: IbcEvent) -> u64 {
        let len = event
            .attributes
            .iter()
            .fold(0, |acc, (k, v)| acc + k.len() + v.len());
        self.ibc_events.insert(event);
        len as u64 * MEMORY_ACCESS_GAS_PER_BYTE
    }

    /// Get the non-temporary storage keys changed and accounts keys initialized
    /// in the current transaction. The account keys point to the validity
    /// predicates of the newly created accounts. The keys in the precommit are
    /// not included in the result of this function.
    pub fn get_keys(&self) -> BTreeSet<storage::Key> {
        self.tx_write_log
            .iter()
            .map(|(key, _modification)| key.clone())
            .collect()
    }

    /// Get the storage keys changed and accounts keys initialized in the
    /// current transaction and precommit. The account keys point to the
    /// validity predicates of the newly created accounts.
    pub fn get_keys_with_precommit(&self) -> BTreeSet<storage::Key> {
        self.tx_precommit_write_log
            .keys()
            .chain(self.tx_write_log.keys())
            .cloned()
            .collect()
    }

    /// Get the storage keys changed in the current transaction (left) and
    /// the addresses of accounts initialized in the current transaction
    /// (right). The first vector excludes keys of validity predicates of
    /// newly initialized accounts, but may include keys of other data
    /// written into newly initialized accounts.
    pub fn get_partitioned_keys(
        &self,
    ) -> (BTreeSet<&storage::Key>, HashSet<&Address>) {
        use itertools::Either;
        self.tx_write_log.iter().partition_map(|(key, value)| {
            match (key.is_validity_predicate(), value) {
                (Some(address), StorageModification::InitAccount { .. }) => {
                    Either::Right(address)
                }
                _ => Either::Left(key),
            }
        })
    }

    /// Get the addresses of accounts initialized in the current transaction.
    pub fn get_initialized_accounts(&self) -> Vec<Address> {
        self.tx_write_log
            .iter()
            .filter_map(|(key, value)| {
                match (key.is_validity_predicate(), value) {
                    (
                        Some(address),
                        StorageModification::InitAccount { .. },
                    ) => Some(address.clone()),
                    _ => None,
                }
            })
            .collect()
    }

    /// Take the IBC event of the current transaction
    pub fn take_ibc_events(&mut self) -> BTreeSet<IbcEvent> {
        std::mem::take(&mut self.ibc_events)
    }

    /// Get the IBC event of the current transaction
    pub fn get_ibc_events(&self) -> &BTreeSet<IbcEvent> {
        &self.ibc_events
    }

    /// Add the entire content of the tx write log to the precommit one. The tx
    /// log gets reset in the process.
    pub fn precommit_tx(&mut self) {
        let tx_log = std::mem::replace(
            &mut self.tx_write_log,
            HashMap::with_capacity(100),
        );

        self.tx_precommit_write_log.extend(tx_log)
    }

    /// Commit the current transaction's write log and precommit log to the
    /// block when it's accepted by all the triggered validity predicates.
    /// Starts a new transaction write log a clears the temp write log.
    pub fn commit_tx(&mut self) {
        // First precommit everything
        self.precommit_tx();

        // Then commit to block
        let tx_precommit_write_log = std::mem::replace(
            &mut self.tx_precommit_write_log,
            HashMap::with_capacity(100),
        );

        self.block_write_log.extend(tx_precommit_write_log);
        self.tx_temp_log.clear();
        self.take_ibc_events();
    }

    /// Drop the current transaction's write log and IBC events and precommit
    /// when it's declined by any of the triggered validity predicates.
    /// Starts a new transaction write log a clears the temp write log.
    pub fn drop_tx(&mut self) {
        self.tx_precommit_write_log.clear();
        self.tx_write_log.clear();
        self.tx_temp_log.clear();
        self.ibc_events.clear();
    }

    /// Drop the current transaction's write log but keep the precommit one.
    /// This is useful only when a part of a transaction failed but it can still
    /// be valid and we want to keep the changes applied before the failed
    /// section.
    pub fn drop_tx_keep_precommit(&mut self) {
        self.tx_write_log.clear();
    }

    /// Get the verifiers set whose validity predicates should validate the
    /// current transaction changes and the storage keys that have been
    /// modified created, updated and deleted via the write log.
    ///
    /// Note that some storage keys may comprise of multiple addresses, in which
    /// case every address will be included in the verifiers set.
    pub fn verifiers_and_changed_keys(
        &self,
        verifiers_from_tx: &BTreeSet<Address>,
    ) -> (BTreeSet<Address>, BTreeSet<storage::Key>) {
        let changed_keys: BTreeSet<storage::Key> = self.get_keys();
        let initialized_accounts = self.get_initialized_accounts();
        let mut verifiers = verifiers_from_tx.clone();

        // get changed keys grouped by the address
        for key in changed_keys.iter() {
            if let Some(addr) = key.fst_address() {
                // We can skip insert when the address has been added from the
                // Tx above. Also skip if it's an address of a newly initialized
                // account, because anything can be written into an account's
                // storage in the same tx in which it's initialized (there is no
                // VP in the state prior to tx execution).
                if !verifiers_from_tx.contains(addr)
                    && !initialized_accounts.contains(addr)
                {
                    // Add the address as a verifier
                    verifiers.insert(addr.clone());
                }
            }
        }
        (verifiers, changed_keys)
    }

    /// Iterate modifications prior to the current transaction, whose storage
    /// key matches the given prefix, sorted by their storage key.
    pub fn iter_prefix_pre(&self, prefix: &storage::Key) -> PrefixIter {
        let mut matches = BTreeMap::new();

        for (key, modification) in &self.block_write_log {
            if key.split_prefix(prefix).is_some() {
                matches.insert(key.to_string(), modification.clone());
            }
        }

        let iter = matches.into_iter();
        PrefixIter { iter }
    }

    /// Iterate modifications posterior of the current tx, whose storage key
    /// matches the given prefix, sorted by their storage key.
    pub fn iter_prefix_post(&self, prefix: &storage::Key) -> PrefixIter {
        let mut matches = BTreeMap::new();

        for bucket in [
            &self.block_write_log,
            &self.tx_precommit_write_log,
            &self.tx_write_log,
        ] {
            for (key, modification) in bucket {
                if key.split_prefix(prefix).is_some() {
                    matches.insert(key.to_string(), modification.clone());
                }
            }
        }

        let iter = matches.into_iter();
        PrefixIter { iter }
    }

    /// Check if the given tx hash has already been processed
    pub fn has_replay_protection_entry(&self, hash: &Hash) -> bool {
        self.replay_protection.contains(hash)
    }

    /// Write the transaction hash
    pub fn write_tx_hash(&mut self, hash: Hash) -> Result<()> {
        if !self.replay_protection.insert(hash) {
            // Cannot write an hash if it's already present in the set
            return Err(Error::ReplayProtection(format!(
                "Requested a write of hash {hash} which has already been \
                 processed"
            )));
        }

        Ok(())
    }

    /// Remove the transaction hash because redundant
    pub(crate) fn redundant_tx_hash(&mut self, hash: &Hash) -> Result<()> {
        if !self.replay_protection.swap_remove(hash) {
            return Err(Error::ReplayProtection(format!(
                "Requested a redundant modification on hash {hash} which is \
                 unknown"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use namada_core::address;
    use pretty_assertions::assert_eq;
    use proptest::prelude::*;

    use super::*;
    use crate::StateRead;

    #[test]
    fn test_crud_value() {
        let mut write_log = WriteLog::default();
        let key =
            storage::Key::parse("key").expect("cannot parse the key string");

        // read a non-existing key
        let (value, gas) = write_log.read(&key);
        assert!(value.is_none());
        assert_eq!(gas, (key.len() as u64) * MEMORY_ACCESS_GAS_PER_BYTE);

        // delete a non-existing key
        let (gas, diff) = write_log.delete(&key).unwrap();
        assert_eq!(gas, key.len() as u64 * STORAGE_WRITE_GAS_PER_BYTE);
        assert_eq!(diff, 0);

        // insert a value
        let inserted = "inserted".as_bytes().to_vec();
        let (gas, diff) = write_log.write(&key, inserted.clone()).unwrap();
        assert_eq!(
            gas,
            (key.len() + inserted.len()) as u64 * STORAGE_WRITE_GAS_PER_BYTE
        );
        assert_eq!(diff, inserted.len() as i64);

        // read the value
        let (value, gas) = write_log.read(&key);
        match value.expect("no read value") {
            StorageModification::Write { value } => {
                assert_eq!(*value, inserted)
            }
            _ => panic!("unexpected read result"),
        }
        assert_eq!(
            gas,
            ((key.len() + inserted.len()) as u64) * MEMORY_ACCESS_GAS_PER_BYTE
        );

        // update the value
        let updated = "updated".as_bytes().to_vec();
        let (gas, diff) = write_log.write(&key, updated.clone()).unwrap();
        assert_eq!(
            gas,
            (key.len() + updated.len()) as u64 * STORAGE_WRITE_GAS_PER_BYTE
        );
        assert_eq!(diff, updated.len() as i64 - inserted.len() as i64);

        // delete the key
        let (gas, diff) = write_log.delete(&key).unwrap();
        assert_eq!(
            gas,
            (key.len() + updated.len()) as u64 * STORAGE_WRITE_GAS_PER_BYTE
        );
        assert_eq!(diff, -(updated.len() as i64));

        // delete the deleted key again
        let (gas, diff) = write_log.delete(&key).unwrap();
        assert_eq!(gas, key.len() as u64 * STORAGE_WRITE_GAS_PER_BYTE);
        assert_eq!(diff, 0);

        // read the deleted key
        let (value, gas) = write_log.read(&key);
        match &value.expect("no read value") {
            StorageModification::Delete => {}
            _ => panic!("unexpected result"),
        }
        assert_eq!(gas, key.len() as u64 * MEMORY_ACCESS_GAS_PER_BYTE);

        // insert again
        let reinserted = "reinserted".as_bytes().to_vec();
        let (gas, diff) = write_log.write(&key, reinserted.clone()).unwrap();
        assert_eq!(
            gas,
            (key.len() + reinserted.len()) as u64 * STORAGE_WRITE_GAS_PER_BYTE
        );
        assert_eq!(diff, reinserted.len() as i64);
    }

    #[test]
    fn test_crud_account() {
        let mut write_log = WriteLog::default();
        let address_gen = EstablishedAddressGen::new("test");

        // init
        let init_vp = "initialized".as_bytes().to_vec();
        let vp_hash = Hash::sha256(init_vp);
        let (addr, gas) = write_log.init_account(&address_gen, vp_hash, &[]);
        let vp_key = storage::Key::validity_predicate(&addr);
        assert_eq!(
            gas,
            (vp_key.len() + vp_hash.len()) as u64 * STORAGE_WRITE_GAS_PER_BYTE
        );

        // read
        let (value, gas) = write_log.read(&vp_key);
        match value.expect("no read value") {
            StorageModification::InitAccount { vp_code_hash } => {
                assert_eq!(*vp_code_hash, vp_hash)
            }
            _ => panic!("unexpected result"),
        }
        assert_eq!(
            gas,
            (vp_key.len() + vp_hash.len()) as u64 * MEMORY_ACCESS_GAS_PER_BYTE
        );

        // get all
        let (_changed_keys, init_accounts) = write_log.get_partitioned_keys();
        assert!(init_accounts.contains(&&addr));
        assert_eq!(init_accounts.len(), 1);
    }

    #[test]
    fn test_update_initialized_account_should_fail() {
        let mut write_log = WriteLog::default();
        let address_gen = EstablishedAddressGen::new("test");

        let init_vp = "initialized".as_bytes().to_vec();
        let vp_hash = Hash::sha256(init_vp);
        let (addr, _) = write_log.init_account(&address_gen, vp_hash, &[]);
        let vp_key = storage::Key::validity_predicate(&addr);

        // update should fail
        let updated_vp = "updated".as_bytes().to_vec();
        let updated_vp_hash = Hash::sha256(updated_vp);
        let result = write_log
            .write(&vp_key, updated_vp_hash.to_vec())
            .unwrap_err();
        assert_matches!(result, Error::UpdateVpOfNewAccount);
    }

    #[test]
    fn test_delete_initialized_account_should_fail() {
        let mut write_log = WriteLog::default();
        let address_gen = EstablishedAddressGen::new("test");

        let init_vp = "initialized".as_bytes().to_vec();
        let vp_hash = Hash::sha256(init_vp);
        let (addr, _) = write_log.init_account(&address_gen, vp_hash, &[]);
        let vp_key = storage::Key::validity_predicate(&addr);

        // delete should fail
        let result = write_log.delete(&vp_key).unwrap_err();
        assert_matches!(result, Error::DeleteVp);
    }

    #[test]
    fn test_delete_vp_should_fail() {
        let mut write_log = WriteLog::default();
        let addr = address::testing::established_address_1();
        let vp_key = storage::Key::validity_predicate(&addr);

        // delete should fail
        let result = write_log.delete(&vp_key).unwrap_err();
        assert_matches!(result, Error::DeleteVp);
    }

    #[test]
    fn test_commit() {
        let mut state = crate::testing::TestState::default();
        let address_gen = EstablishedAddressGen::new("test");

        let key1 =
            storage::Key::parse("key1").expect("cannot parse the key string");
        let key2 =
            storage::Key::parse("key2").expect("cannot parse the key string");
        let key3 =
            storage::Key::parse("key3").expect("cannot parse the key string");
        let key4 =
            storage::Key::parse("key4").expect("cannot parse the key string");

        // initialize an account
        let vp1 = Hash::sha256("vp1".as_bytes());
        let (addr1, _) = state.write_log.init_account(&address_gen, vp1, &[]);
        state.write_log.commit_tx();

        // write values
        let val1 = "val1".as_bytes().to_vec();
        state.write_log.write(&key1, val1.clone()).unwrap();
        state.write_log.write(&key2, val1.clone()).unwrap();
        state.write_log.write(&key3, val1.clone()).unwrap();
        state.write_log.write_temp(&key4, val1.clone()).unwrap();
        state.write_log.commit_tx();

        // these values are not written due to drop_tx
        let val2 = "val2".as_bytes().to_vec();
        state.write_log.write(&key1, val2.clone()).unwrap();
        state.write_log.write(&key2, val2.clone()).unwrap();
        state.write_log.write(&key3, val2).unwrap();
        state.write_log.drop_tx();

        // deletes and updates values
        let val3 = "val3".as_bytes().to_vec();
        state.write_log.delete(&key2).unwrap();
        state.write_log.write(&key3, val3.clone()).unwrap();
        state.write_log.commit_tx();

        // commit a block
        state.commit_block().expect("commit failed");

        let (vp_code_hash, _gas) =
            state.validity_predicate(&addr1).expect("vp read failed");
        assert_eq!(vp_code_hash, Some(vp1));
        let (value, _) = state.db_read(&key1).expect("read failed");
        assert_eq!(value.expect("no read value"), val1);
        let (value, _) = state.db_read(&key2).expect("read failed");
        assert!(value.is_none());
        let (value, _) = state.db_read(&key3).expect("read failed");
        assert_eq!(value.expect("no read value"), val3);
        let (value, _) = state.db_read(&key4).expect("read failed");
        assert_eq!(value, None);
    }

    #[test]
    fn test_replay_protection_commit() {
        let mut state = crate::testing::TestState::default();

        {
            let write_log = state.write_log_mut();
            // write some replay protection keys
            write_log
                .write_tx_hash(Hash::sha256("tx1".as_bytes()))
                .unwrap();
            write_log
                .write_tx_hash(Hash::sha256("tx2".as_bytes()))
                .unwrap();
            write_log
                .write_tx_hash(Hash::sha256("tx3".as_bytes()))
                .unwrap();
        }

        // commit a block
        state.commit_block().expect("commit failed");

        assert!(state.write_log.replay_protection.is_empty());
        for tx in ["tx1", "tx2", "tx3"] {
            let hash = Hash::sha256(tx.as_bytes());
            assert!(
                state
                    .has_replay_protection_entry(&hash)
                    .expect("read failed")
            );
        }

        {
            let write_log = state.write_log_mut();
            // write some replay protection keys
            write_log
                .write_tx_hash(Hash::sha256("tx4".as_bytes()))
                .unwrap();
            write_log
                .write_tx_hash(Hash::sha256("tx5".as_bytes()))
                .unwrap();
            write_log
                .write_tx_hash(Hash::sha256("tx6".as_bytes()))
                .unwrap();

            // Mark one hash as redundant
            write_log
                .redundant_tx_hash(&Hash::sha256("tx4".as_bytes()))
                .unwrap();
        }

        // commit a block
        state.commit_block().expect("commit failed");

        assert!(state.write_log.replay_protection.is_empty());
        for tx in ["tx1", "tx2", "tx3", "tx5", "tx6"] {
            assert!(
                state
                    .has_replay_protection_entry(&Hash::sha256(tx.as_bytes()))
                    .expect("read failed")
            );
        }
        assert!(
            !state
                .has_replay_protection_entry(&Hash::sha256("tx4".as_bytes()))
                .expect("read failed")
        );
        {
            let write_log = state.write_log_mut();
            write_log
                .write_tx_hash(Hash::sha256("tx7".as_bytes()))
                .unwrap();

            // mark as redundant a missing hash and check that it fails
            assert!(
                state
                    .write_log
                    .redundant_tx_hash(&Hash::sha256("tx8".as_bytes()))
                    .is_err()
            );

            // Do not assert the state of replay protection because this
            // error will actually trigger a shut down of the node. Also, since
            // we write the values before validating them, the state would be
            // wrong
        }
    }

    // Test that writing a value on top of a temporary write is not allowed
    #[test]
    fn test_write_after_temp_disallowed() {
        let mut state = crate::testing::TestState::default();

        let key1 =
            storage::Key::parse("key1").expect("cannot parse the key string");
        let val1 = "val1".as_bytes().to_vec();
        // Test from tx_write_log
        state.write_log.write_temp(&key1, val1.clone()).unwrap();
        assert!(matches!(
            state.write_log.write(&key1, val1.clone()),
            Err(Error::UpdateTemporaryValue)
        ));

        // Test with a temporary write precommitted
        state.write_log.write_temp(&key1, val1.clone()).unwrap();
        state.write_log.precommit_tx();
        assert!(matches!(
            state.write_log.write(&key1, val1),
            Err(Error::UpdateTemporaryValue)
        ));
    }

    // Test that a temporary write on top of a write is not allowed
    #[test]
    fn test_write_temp_after_write_disallowed() {
        let mut state = crate::testing::TestState::default();

        let key1 =
            storage::Key::parse("key1").expect("cannot parse the key string");
        let val1 = "val1".as_bytes().to_vec();
        // Test from tx_write_log
        state.write_log.write(&key1, val1.clone()).unwrap();
        assert!(matches!(
            state.write_log.write_temp(&key1, val1.clone()),
            Err(Error::WriteTempAfterWrite)
        ));

        // Test with a temporary write precommitted
        state.write_log.write(&key1, val1.clone()).unwrap();
        state.write_log.precommit_tx();
        assert!(matches!(
            state.write_log.write_temp(&key1, val1),
            Err(Error::WriteTempAfterWrite)
        ));
    }

    // Test that a temporary write on top of a delete is not allowed
    #[test]
    fn test_write_temp_after_delete_disallowed() {
        let mut state = crate::testing::TestState::default();

        let key1 =
            storage::Key::parse("key1").expect("cannot parse the key string");
        let val1 = "val1".as_bytes().to_vec();
        // Test from tx_write_log
        state.write_log.delete(&key1).unwrap();
        assert!(matches!(
            state.write_log.write_temp(&key1, val1.clone()),
            Err(Error::WriteTempAfterDelete)
        ));

        // Test with a temporary write precommitted
        state.write_log.delete(&key1).unwrap();
        state.write_log.precommit_tx();
        assert!(matches!(
            state.write_log.write_temp(&key1, val1),
            Err(Error::WriteTempAfterDelete)
        ));
    }

    prop_compose! {
        fn arb_verifiers_changed_key_tx_all_key()
            (verifiers_from_tx in testing::arb_verifiers_from_tx())
            (tx_write_log in testing::arb_tx_write_log(verifiers_from_tx.clone()),
                verifiers_from_tx in Just(verifiers_from_tx))
        -> (BTreeSet<Address>, HashMap<storage::Key, StorageModification>) {
            (verifiers_from_tx, tx_write_log)
        }
    }

    proptest! {
        /// Test [`WriteLog::verifiers_changed_keys`] that:
        /// 1. Every address from `verifiers_from_tx` is included in the
        ///    verifiers set.
        /// 2. Every address included in the first segment of changed storage
        ///    keys is included in the verifiers set.
        /// 3. Addresses of newly initialized accounts are not verifiers, so
        ///    that anything can be written into an account's storage in the
        ///    same tx in which it's initialized.
        /// 4. The validity predicates of all the newly initialized accounts are
        ///    included in the changed keys set.
        #[test]
        fn verifiers_changed_key_tx_all_key(
            (verifiers_from_tx, tx_write_log) in arb_verifiers_changed_key_tx_all_key(),
        ) {
            let write_log = WriteLog { tx_write_log, ..WriteLog::default() };

            let (verifiers, changed_keys) = write_log.verifiers_and_changed_keys(&verifiers_from_tx);

            println!("verifiers_from_tx {:#?}", verifiers_from_tx);
            for verifier_from_tx in verifiers_from_tx {
                // Test for 1.
                assert!(verifiers.contains(&verifier_from_tx));
            }

            let (_changed_keys, initialized_accounts) = write_log.get_partitioned_keys();
            for key in changed_keys.iter() {
                if let Some(addr_from_key) = key.fst_address() {
                    if !initialized_accounts.contains(addr_from_key) {
                        // Test for 2.
                        assert!(verifiers.contains(addr_from_key));
                    }
                }
            }

            println!("verifiers {:#?}", verifiers);
            println!("changed_keys {:#?}", changed_keys);
            println!("initialized_accounts {:#?}", initialized_accounts);
            for initialized_account in initialized_accounts {
                // Test for 3.
                assert!(!verifiers.contains(initialized_account));
                // Test for 4.
                let vp_key = storage::Key::validity_predicate(initialized_account);
                assert!(changed_keys.contains(&vp_key));
            }
        }
    }
}

/// Helpers for testing with write log.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use namada_core::address::testing::arb_address;
    use namada_core::hash::HASH_LENGTH;
    use namada_core::storage::testing::arb_key;
    use proptest::collection;
    use proptest::prelude::{any, prop_oneof, Just, Strategy};

    use super::*;

    /// Generate an arbitrary tx write log of [`HashMap<storage::Key,
    /// StorageModification>`].
    pub fn arb_tx_write_log(
        verifiers_from_tx: BTreeSet<Address>,
    ) -> impl Strategy<Value = HashMap<storage::Key, StorageModification>> + 'static
    {
        arb_key().prop_flat_map(move |key| {
            // If the key is a validity predicate key and its owner is in the
            // verifier set, we must not generate `InitAccount` for it.
            let can_init_account = key
                .is_validity_predicate()
                .map(|owner| !verifiers_from_tx.contains(owner))
                .unwrap_or_default();
            collection::hash_map(
                Just(key),
                arb_storage_modification(can_init_account),
                0..100,
            )
            .prop_map(|map| map.into_iter().collect())
        })
    }

    /// Generate arbitrary verifiers from tx of [`BTreeSet<Address>`].
    pub fn arb_verifiers_from_tx() -> impl Strategy<Value = BTreeSet<Address>> {
        collection::btree_set(arb_address(), 0..10)
    }

    /// Generate an arbitrary [`StorageModification`].
    pub fn arb_storage_modification(
        can_init_account: bool,
    ) -> impl Strategy<Value = StorageModification> {
        if can_init_account {
            prop_oneof![
                any::<Vec<u8>>()
                    .prop_map(|value| StorageModification::Write { value }),
                Just(StorageModification::Delete),
                any::<[u8; HASH_LENGTH]>().prop_map(|hash| {
                    StorageModification::InitAccount {
                        vp_code_hash: Hash(hash),
                    }
                }),
            ]
            .boxed()
        } else {
            prop_oneof![
                any::<Vec<u8>>()
                    .prop_map(|value| StorageModification::Write { value }),
                Just(StorageModification::Delete),
            ]
            .boxed()
        }
    }
}
