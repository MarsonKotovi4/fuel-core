use std::collections::HashMap;

use fuel_core_types::{
    fuel_tx::{
        consensus_parameters::gas,
        field::BlobId,
        Transaction,
        TxId,
    },
    fuel_vm::checked_transaction::Checked,
    services::txpool::PoolTransaction,
};
use tracing::instrument;

use crate::{
    collision_manager::CollisionManager,
    config::Config,
    error::Error,
    ports::{
        AtomicView,
        TxPoolPersistentStorage,
    },
    selection_algorithms::{
        Constraints,
        SelectionAlgorithm,
    },
    storage::Storage,
    verifications::FullyVerifiedTx,
};

/// The pool is the main component of the txpool service. It is responsible for storing transactions
/// and allowing the selection of transactions for inclusion in a block.
pub struct Pool<PSProvider, S: Storage, CM, SA> {
    /// Configuration of the pool.
    pub config: Config,
    /// The storage of the pool.
    storage: S,
    /// The collision manager of the pool.
    collision_manager: CM,
    /// The selection algorithm of the pool.
    selection_algorithm: SA,
    /// The persistent storage of the pool.
    persistent_storage_provider: PSProvider,
    /// Mapping from tx_id to storage_id.
    tx_id_to_storage_id: HashMap<TxId, S::StorageIndex>,
}

impl<PSProvider, S: Storage, CM, SA> Pool<PSProvider, S, CM, SA> {
    /// Create a new pool.
    pub fn new(
        persistent_storage_provider: PSProvider,
        storage: S,
        collision_manager: CM,
        selection_algorithm: SA,
        config: Config,
    ) -> Self {
        Pool {
            storage,
            collision_manager,
            selection_algorithm,
            persistent_storage_provider,
            config,
            tx_id_to_storage_id: HashMap::new(),
        }
    }
}

impl<PS, View, S, CM, SA> Pool<PS, S, CM, SA>
where
    PS: AtomicView<LatestView = View>,
    View: TxPoolPersistentStorage,
    S: Storage,
    CM: CollisionManager<Storage = S, StorageIndex = S::StorageIndex>,
    SA: SelectionAlgorithm<Storage = S, StorageIndex = S::StorageIndex>,
{
    /// Insert transactions into the pool.
    /// Returns a list of results for each transaction.
    /// Each result is a list of transactions that were removed from the pool
    /// because of the insertion of the new transaction.
    #[instrument(skip(self))]
    pub fn insert(&mut self, tx: PoolTransaction) -> Result<Vec<PoolTransaction>, Error> {
        let latest_view = self
            .persistent_storage_provider
            .latest_view()
            .map_err(|e| Error::Database(format!("{:?}", e)))?;
        let tx_id = tx.id();
        self.check_pool_is_not_full()?;
        self.config.black_list.check_blacklisting(&tx)?;
        Self::check_blob_does_not_exist(&tx, &latest_view)?;
        let collisions = self
            .collision_manager
            .collect_colliding_transactions(&tx, &self.storage)?;
        let dependencies = self.storage.validate_inputs_and_collect_dependencies(
            &tx,
            collisions.reasons,
            &latest_view,
            self.config.utxo_validation,
        )?;
        let has_dependencies = !dependencies.is_empty();
        let (storage_id, removed_transactions) = self.storage.store_transaction(
            tx,
            &dependencies,
            &collisions.colliding_txs,
        )?;
        self.tx_id_to_storage_id.insert(tx_id, storage_id);
        // No dependencies directly in the graph and the sorted transactions
        if !has_dependencies {
            self.selection_algorithm
                .new_executable_transactions(vec![storage_id], &self.storage)?;
        }
        self.update_components_and_caches_on_removal(&removed_transactions)?;
        let tx = Storage::get(&self.storage, &storage_id)?;
        self.collision_manager
            .on_stored_transaction(&tx.transaction, storage_id)?;
        Ok(removed_transactions)
    }

    /// Check if a transaction can be inserted into the pool.
    pub fn can_insert_transaction(&self, tx: &PoolTransaction) -> Result<(), Error> {
        let persistent_storage = self
            .persistent_storage_provider
            .latest_view()
            .map_err(|e| Error::Database(format!("{:?}", e)))?;
        self.check_pool_is_not_full()?;
        self.config.black_list.check_blacklisting(tx)?;
        Self::check_blob_does_not_exist(tx, &persistent_storage)?;
        let collisions = self
            .collision_manager
            .collect_colliding_transactions(tx, &self.storage)?;
        let dependencies = self.storage.validate_inputs_and_collect_dependencies(
            tx,
            collisions.reasons,
            &persistent_storage,
            self.config.utxo_validation,
        )?;
        self.storage
            .can_store_transaction(tx, &dependencies, &collisions.colliding_txs);
        Ok(())
    }

    // TODO: Use block space also (https://github.com/FuelLabs/fuel-core/issues/2133)
    /// Extract transactions for a block.
    /// Returns a list of transactions that were selected for the block
    /// based on the constraints given in the configuration and the selection algorithm used.
    pub fn extract_transactions_for_block(
        &mut self,
        max_gas: u64,
    ) -> Result<Vec<PoolTransaction>, Error> {
        self.selection_algorithm
            .gather_best_txs(Constraints { max_gas }, &self.storage)?
            .into_iter()
            .map(|storage_id| {
                let storage_data = self.storage.remove_transaction(storage_id)?;
                self.collision_manager
                    .on_removed_transaction(&storage_data.transaction)?;
                self.selection_algorithm
                    .on_removed_transaction(&storage_data.transaction)?;
                self.tx_id_to_storage_id
                    .remove(&storage_data.transaction.id());
                Ok(storage_data.transaction)
            })
            .collect()
    }

    pub fn find_one(&self, tx_id: &TxId) -> Option<&PoolTransaction> {
        Storage::get(&self.storage, self.tx_id_to_storage_id.get(tx_id)?)
            .map(|data| &data.transaction)
            .ok()
    }

    /// Remove transaction but keep its dependents.
    /// The dependents become exeuctables.
    pub fn remove_committed_txs(&mut self, tx_ids: Vec<TxId>) -> Result<(), Error> {
        for tx_id in tx_ids {
            if let Some(storage_id) = self.tx_id_to_storage_id.remove(&tx_id) {
                let dependents = self.storage.get_dependents(storage_id)?;
                let storage_data = self.storage.remove_transaction(storage_id)?;
                self.selection_algorithm
                    .new_executable_transactions(dependents, &self.storage)?;
                self.update_components_and_caches_on_removal(
                    &[storage_data.transaction],
                )?;
            }
        }
        Ok(())
    }

    fn check_pool_is_not_full(&self) -> Result<(), Error> {
        if self.storage.count() >= self.config.max_txs as usize {
            return Err(Error::NotInsertedLimitHit);
        }
        Ok(())
    }

    fn check_blob_does_not_exist(
        tx: &PoolTransaction,
        persistent_storage: &impl TxPoolPersistentStorage,
    ) -> Result<(), Error> {
        if let PoolTransaction::Blob(checked_tx, _) = &tx {
            let blob_id = checked_tx.transaction().blob_id();
            if persistent_storage
                .blob_exist(blob_id)
                .map_err(|e| Error::Database(format!("{:?}", e)))?
            {
                return Err(Error::NotInsertedBlobIdAlreadyTaken(*blob_id))
            }
        }
        Ok(())
    }

    fn update_components_and_caches_on_removal(
        &mut self,
        removed_transactions: &[PoolTransaction],
    ) -> Result<(), Error> {
        for tx in removed_transactions {
            self.collision_manager.on_removed_transaction(tx)?;
            self.selection_algorithm.on_removed_transaction(tx)?;
            self.tx_id_to_storage_id.remove(&tx.id());
        }
        Ok(())
    }
}
