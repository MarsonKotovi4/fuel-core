use crate::{
    database::{
        database_description::gas_price::GasPriceDatabase,
        Database,
        RegularStage,
    },
    service::{
        adapters::ConsensusParametersProvider,
        Config,
    },
};

use fuel_core_gas_price_service::{
    fuel_gas_price_updater::{
        da_source_adapter::{
            dummy_costs::DummyDaBlockCosts,
            DaBlockCostsProvider,
            DaBlockCostsSharedState,
        },
        fuel_core_storage_adapter::{
            get_block_info,
            storage::GasPriceMetadata,
            FuelL2BlockSource,
            GasPriceSettings,
            GasPriceSettingsProvider,
        },
        Algorithm,
        AlgorithmUpdater,
        AlgorithmUpdaterV0,
        BlockInfo,
        FuelGasPriceUpdater,
        MetadataStorage,
        UpdaterMetadata,
        V0Metadata,
    },
    ports::L2Data,
    GasPriceService,
    SharedGasPriceAlgo,
};
use fuel_core_services::{
    stream::BoxStream,
    RunnableService,
    Service,
    StateWatcher,
};
use fuel_core_storage::{
    not_found,
    structured_storage::StructuredStorage,
    transactional::{
        AtomicView,
        HistoricalView,
    },
    StorageAsRef,
};
use fuel_core_types::{
    fuel_types::BlockHeight,
    services::block_importer::SharedImportResult,
};

type Updater = FuelGasPriceUpdater<
    FuelL2BlockSource<ConsensusParametersProvider>,
    MetadataStorageAdapter,
    DaBlockCostsSharedState,
>;

pub struct InitializeTask<L2DataStoreView> {
    pub config: Config,
    pub genesis_block_height: BlockHeight,
    pub settings: ConsensusParametersProvider,
    pub gas_price_db: Database<GasPriceDatabase, RegularStage<GasPriceDatabase>>,
    pub on_chain_db: L2DataStoreView,
    pub block_stream: BoxStream<SharedImportResult>,
    pub shared_algo: SharedGasPriceAlgo<Algorithm>,
    pub da_block_costs_provider: DaBlockCostsProvider<DummyDaBlockCosts>,
}

type MetadataStorageAdapter =
    StructuredStorage<Database<GasPriceDatabase, RegularStage<GasPriceDatabase>>>;

type Task = GasPriceService<Algorithm, Updater>;

impl<L2DataStore, L2DataStoreView> InitializeTask<L2DataStoreView>
where
    L2DataStore: L2Data,
    L2DataStoreView: AtomicView<LatestView = L2DataStore>,
{
    pub fn new(
        config: Config,
        genesis_block_height: BlockHeight,
        settings: ConsensusParametersProvider,
        block_stream: BoxStream<SharedImportResult>,
        gas_price_db: Database<GasPriceDatabase, RegularStage<GasPriceDatabase>>,
        on_chain_db: L2DataStoreView,
    ) -> anyhow::Result<Self> {
        let view = on_chain_db.latest_view()?;
        let latest_block_height =
            view.latest_height().unwrap_or(genesis_block_height).into();
        let default_metadata = get_default_metadata(&config, latest_block_height);
        let algo = get_best_algo(&gas_price_db, default_metadata)?;
        let shared_algo = SharedGasPriceAlgo::new_with_algorithm(algo);
        // there's no use of this source yet, so we can safely return an error
        let da_block_costs_source =
            DummyDaBlockCosts::new(Err(anyhow::anyhow!("Not used")));
        let da_block_costs_provider =
            DaBlockCostsProvider::new(da_block_costs_source, None);

        let task = Self {
            config,
            genesis_block_height,
            settings,
            gas_price_db,
            on_chain_db,
            block_stream,
            shared_algo,
            da_block_costs_provider,
        };
        Ok(task)
    }
}

fn get_default_metadata(config: &Config, latest_block_height: u32) -> UpdaterMetadata {
    UpdaterMetadata::V0(V0Metadata {
        new_exec_price: config.starting_gas_price.max(config.min_gas_price),
        min_exec_gas_price: config.min_gas_price,
        exec_gas_price_change_percent: config.gas_price_change_percent,
        l2_block_height: latest_block_height,
        l2_block_fullness_threshold_percent: config.gas_price_threshold_percent,
    })
}

fn get_best_algo(
    gas_price_db: &Database<GasPriceDatabase, RegularStage<GasPriceDatabase>>,
    default_metadata: UpdaterMetadata,
) -> anyhow::Result<Algorithm> {
    let best_metadata: UpdaterMetadata =
        if let Some(height) = gas_price_db.latest_height() {
            gas_price_db
                .storage::<GasPriceMetadata>()
                .get(&height)?
                .map(|m| m.into_owned())
                .unwrap_or(default_metadata)
        } else {
            default_metadata
        };
    let updater: AlgorithmUpdater = best_metadata.into();
    let algo = updater.algorithm();
    Ok(algo)
}
#[async_trait::async_trait]
impl<L2DataStore, L2DataStoreView> RunnableService for InitializeTask<L2DataStoreView>
where
    L2DataStore: L2Data,
    L2DataStoreView: AtomicView<LatestView = L2DataStore>,
{
    const NAME: &'static str = "GasPriceUpdater";
    type SharedData = SharedGasPriceAlgo<Algorithm>;
    type Task = Task;
    type TaskParams = ();

    fn shared_data(&self) -> Self::SharedData {
        self.shared_algo.clone()
    }

    async fn into_task(
        self,
        _state_watcher: &StateWatcher,
        _params: Self::TaskParams,
    ) -> anyhow::Result<Self::Task> {
        let view = self.on_chain_db.latest_view()?;
        let starting_height = view.latest_height().unwrap_or(self.genesis_block_height);

        let updater = get_synced_gas_price_updater(
            self.config,
            self.genesis_block_height,
            self.settings,
            self.gas_price_db,
            &self.on_chain_db,
            self.block_stream,
            self.da_block_costs_provider.shared_state,
        )?;

        self.da_block_costs_provider
            .service
            .start_and_await()
            .await?;
        let inner_service =
            GasPriceService::new(starting_height, updater, self.shared_algo).await;
        Ok(inner_service)
    }
}

pub fn get_synced_gas_price_updater<L2DataStore, L2DataStoreView>(
    config: Config,
    genesis_block_height: BlockHeight,
    settings: ConsensusParametersProvider,
    mut gas_price_db: Database<GasPriceDatabase, RegularStage<GasPriceDatabase>>,
    on_chain_db: &L2DataStoreView,
    block_stream: BoxStream<SharedImportResult>,
    da_block_costs: DaBlockCostsSharedState,
) -> anyhow::Result<Updater>
where
    L2DataStore: L2Data,
    L2DataStoreView: AtomicView<LatestView = L2DataStore>,
{
    let mut first_run = false;
    let latest_block_height: u32 = on_chain_db
        .latest_view()?
        .latest_height()
        .unwrap_or(genesis_block_height)
        .into();

    let maybe_metadata_height = gas_price_db.latest_height();
    let mut metadata_height = if let Some(metadata_height) = maybe_metadata_height {
        metadata_height.into()
    } else {
        first_run = true;
        latest_block_height
    };
    let default_metadata = get_default_metadata(&config, latest_block_height);

    if metadata_height > latest_block_height {
        revert_gas_price_db_to_height(&mut gas_price_db, latest_block_height.into())?;
        metadata_height = gas_price_db
            .latest_height()
            .ok_or(anyhow::anyhow!(
                "Metadata DB height should match the latest block height"
            ))?
            .into();
    }

    let mut metadata_storage = StructuredStorage::new(gas_price_db);
    let l2_block_source =
        FuelL2BlockSource::new(genesis_block_height, settings.clone(), block_stream);

    if BlockHeight::from(latest_block_height) == genesis_block_height || first_run {
        let updater = FuelGasPriceUpdater::new(
            default_metadata.into(),
            l2_block_source,
            metadata_storage,
            da_block_costs,
        );
        Ok(updater)
    } else {
        if latest_block_height > metadata_height {
            sync_metadata_storage_with_on_chain_storage(
                &settings,
                &mut metadata_storage,
                on_chain_db,
                metadata_height,
                latest_block_height,
            )?;
        }

        FuelGasPriceUpdater::init(
            latest_block_height.into(),
            l2_block_source,
            metadata_storage,
            da_block_costs,
            config.min_gas_price,
            config.gas_price_change_percent,
            config.gas_price_threshold_percent,
        )
        .map_err(|e| anyhow::anyhow!("Could not initialize gas price updater: {e:?}"))
    }
}

fn sync_metadata_storage_with_on_chain_storage<L2DataStore, L2DataStoreView>(
    settings: &ConsensusParametersProvider,
    metadata_storage: &mut StructuredStorage<
        Database<GasPriceDatabase, RegularStage<GasPriceDatabase>>,
    >,
    on_chain_db: &L2DataStoreView,
    metadata_height: u32,
    latest_block_height: u32,
) -> anyhow::Result<()>
where
    L2DataStore: L2Data,
    L2DataStoreView: AtomicView<LatestView = L2DataStore>,
{
    let metadata = metadata_storage
        .get_metadata(&metadata_height.into())?
        .ok_or(anyhow::anyhow!(
            "Expected metadata to exist for height: {metadata_height}"
        ))?;
    let mut inner: AlgorithmUpdater = metadata.into();
    match &mut inner {
        AlgorithmUpdater::V0(ref mut updater) => {
            sync_v0_metadata(
                settings,
                on_chain_db,
                metadata_height,
                latest_block_height,
                updater,
                metadata_storage,
            )?;
        }
        AlgorithmUpdater::V1(_) => {
            todo!() // TODO(#2140)
        }
    }
    Ok(())
}

fn sync_v0_metadata<L2DataStore, L2DataStoreView>(
    settings: &ConsensusParametersProvider,
    on_chain_db: &L2DataStoreView,
    metadata_height: u32,
    latest_block_height: u32,
    updater: &mut AlgorithmUpdaterV0,
    metadata_storage: &mut StructuredStorage<
        Database<GasPriceDatabase, RegularStage<GasPriceDatabase>>,
    >,
) -> anyhow::Result<()>
where
    L2DataStore: L2Data,
    L2DataStoreView: AtomicView<LatestView = L2DataStore>,
{
    let first = metadata_height.saturating_add(1);
    let view = on_chain_db.latest_view()?;
    for height in first..=latest_block_height {
        let block = view
            .get_block(&height.into())?
            .ok_or(not_found!("FullBlock"))?;
        let param_version = block.header().consensus_parameters_version;

        let GasPriceSettings {
            gas_price_factor,
            block_gas_limit,
        } = settings.settings(&param_version)?;
        let block_gas_capacity = block_gas_limit.try_into()?;

        let block_gas_used =
            match get_block_info(&block, gas_price_factor, block_gas_limit)? {
                BlockInfo::GenesisBlock => {
                    Err(anyhow::anyhow!("should not be genesis block"))?
                }
                BlockInfo::Block { gas_used, .. } => gas_used,
            };

        updater.update_l2_block_data(height, block_gas_used, block_gas_capacity)?;
        let metadata = AlgorithmUpdater::V0(updater.clone()).into();
        metadata_storage.set_metadata(metadata)?;
    }

    Ok(())
}

fn revert_gas_price_db_to_height(
    gas_price_db: &mut Database<GasPriceDatabase, RegularStage<GasPriceDatabase>>,
    height: BlockHeight,
) -> anyhow::Result<()> {
    if let Some(gas_price_db_height) = gas_price_db.latest_height() {
        let gas_price_db_height: u32 = gas_price_db_height.into();
        let height: u32 = height.into();
        let diff = gas_price_db_height.saturating_sub(height);
        for _ in 0..diff {
            gas_price_db.rollback_last_block()?;
        }
    }
    Ok(())
}
