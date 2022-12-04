use super::{
    coin::CoinConfig,
    contract::ContractConfig,
    message::MessageConfig,
};
use crate::{
    serialization::HexNumber,
    ChainConfigDb,
};
use fuel_core_interfaces::model::BlockHeight;
use serde::{
    Deserialize,
    Serialize,
};
use serde_with::{
    serde_as,
    skip_serializing_none,
};

// TODO: do streaming deserialization to handle large state configs
#[serde_as]
#[skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub struct StateConfig {
    /// Spendable coins
    pub coins: Option<Vec<CoinConfig>>,
    /// Contract state
    pub contracts: Option<Vec<ContractConfig>>,
    /// Messages from Layer 1
    pub messages: Option<Vec<MessageConfig>>,
    /// Starting block height (useful for flattened fork networks)
    #[serde_as(as = "Option<HexNumber>")]
    #[serde(default)]
    pub height: Option<BlockHeight>,
}

impl StateConfig {
    pub fn generate_state_config<T>(db: T) -> anyhow::Result<Self>
    where
        T: ChainConfigDb,
    {
        Ok(StateConfig {
            coins: db.get_coin_config()?,
            contracts: db.get_contract_config()?,
            messages: db.get_message_config()?,
            height: db.get_block_height()?,
        })
    }
}
