use crate::fuel_gas_price_updater::{
    Result,
    UpdaterMetadata,
};
use fuel_core_storage::Result as StorageResult;
use fuel_core_types::{
    blockchain::block::Block,
    fuel_tx::Transaction,
    fuel_types::BlockHeight,
};

pub trait L2Data: Send + Sync {
    fn latest_height(&self) -> StorageResult<BlockHeight>;
    fn get_block(
        &self,
        height: &BlockHeight,
    ) -> StorageResult<Option<Block<Transaction>>>;
}

pub trait MetadataStorage: Send + Sync {
    fn get_metadata(&self, block_height: &BlockHeight)
        -> Result<Option<UpdaterMetadata>>;
    fn set_metadata(&mut self, metadata: &UpdaterMetadata) -> Result<()>;
}

/// To be linked with the GasPriceDatabase: Database<GasPriceDatabase>
pub trait GasPriceData: Send + Sync {
    fn latest_height(&self) -> Option<BlockHeight>;
}
