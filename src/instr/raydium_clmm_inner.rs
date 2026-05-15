use crate::core::events::{DexEvent, EventMetadata};

pub mod discriminators {
    pub const SWAP: [u8; 16] =
        [248, 198, 158, 145, 225, 117, 135, 200, 155, 167, 108, 32, 122, 76, 173, 64];
    pub const INCREASE_LIQUIDITY: [u8; 16] =
        [133, 29, 89, 223, 69, 238, 176, 10, 155, 167, 108, 32, 122, 76, 173, 64];
    pub const DECREASE_LIQUIDITY: [u8; 16] =
        [160, 38, 208, 111, 104, 91, 44, 1, 155, 167, 108, 32, 122, 76, 173, 64];
    pub const CREATE_POOL: [u8; 16] =
        [233, 146, 209, 142, 207, 104, 64, 188, 155, 167, 108, 32, 122, 76, 173, 64];
    pub const COLLECT_FEE: [u8; 16] =
        [164, 152, 207, 99, 187, 104, 171, 119, 155, 167, 108, 32, 122, 76, 173, 64];
}

#[inline]
pub fn parse_raydium_clmm_inner_instruction(
    discriminator: &[u8; 16],
    data: &[u8],
    metadata: EventMetadata,
) -> Option<DexEvent> {
    match discriminator {
        &discriminators::SWAP => crate::logs::raydium_clmm::parse_swap_from_data(data, metadata),
        &discriminators::INCREASE_LIQUIDITY => {
            crate::logs::raydium_clmm::parse_increase_liquidity_from_data(data, metadata)
        }
        &discriminators::DECREASE_LIQUIDITY => {
            crate::logs::raydium_clmm::parse_decrease_liquidity_from_data(data, metadata)
        }
        &discriminators::CREATE_POOL => {
            crate::logs::raydium_clmm::parse_create_pool_from_data(data, metadata)
        }
        &discriminators::COLLECT_FEE => {
            crate::logs::raydium_clmm::parse_collect_fee_from_data(data, metadata)
        }
        _ => None,
    }
}
