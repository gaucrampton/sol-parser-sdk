//! PumpFun instruction parser
//!
//! Parse PumpFun instructions using discriminator pattern matching

use super::program_ids;
use super::utils::*;
use crate::core::events::*;
use solana_sdk::{pubkey::Pubkey, signature::Signature};

/// PumpFun discriminator constants
pub mod discriminators {
    /// Buy instruction: buy tokens with SOL
    pub const BUY: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
    /// Sell instruction: sell tokens for SOL
    pub const SELL: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
    /// Create instruction: create a new bonding curve
    pub const CREATE: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];
    /// CreateV2 instruction: SPL-22 / Mayhem mode (idl create_v2)
    pub const CREATE_V2: [u8; 8] = [214, 144, 76, 236, 95, 139, 49, 180];
    /// buy_exact_sol_in: Given a budget of spendable SOL, buy at least min_tokens_out
    pub const BUY_EXACT_SOL_IN: [u8; 8] = [56, 252, 116, 8, 158, 223, 205, 95];
    /// Migrate event log discriminator (CPI)
    pub const MIGRATE_EVENT_LOG: [u8; 8] = [189, 233, 93, 185, 92, 148, 234, 148];
    /// `migrate_bonding_curve_creator` 外层 ix（`idls/pumpfun.json`）
    pub const MIGRATE_BONDING_CURVE_CREATOR: [u8; 8] =
        [87, 124, 52, 191, 52, 38, 214, 232];
}

/// PumpFun Program ID
pub const PROGRAM_ID_PUBKEY: Pubkey = program_ids::PUMPFUN_PROGRAM_ID;

/// Main PumpFun instruction parser
///
/// Outer instructions (8-byte discriminator): CREATE, CREATE_V2 从指令解析并返回事件；
/// BUY/SELL 仍以 log 为主。Inner CPI: MIGRATE_EVENT_LOG 仅在此解析。
pub fn parse_instruction(
    instruction_data: &[u8],
    accounts: &[Pubkey],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    block_time_us: Option<i64>,
    grpc_recv_us: i64,
) -> Option<DexEvent> {
    if instruction_data.len() < 8 {
        return None;
    }
    let outer_disc: [u8; 8] = instruction_data[0..8].try_into().ok()?;
    let data = &instruction_data[8..];

    // 外层指令：Create / CreateV2（与 solana-streamer 功能对齐）
    if outer_disc == discriminators::CREATE_V2 {
        return parse_create_v2_instruction(
            data,
            accounts,
            signature,
            slot,
            tx_index,
            block_time_us,
            grpc_recv_us,
        );
    }
    if outer_disc == discriminators::CREATE {
        return parse_create_instruction(
            data,
            accounts,
            signature,
            slot,
            tx_index,
            block_time_us,
            grpc_recv_us,
        );
    }

    // Inner CPI：仅 MIGRATE 在此解析
    if instruction_data.len() >= 16 {
        let cpi_disc: [u8; 8] = instruction_data[8..16].try_into().ok()?;
        if cpi_disc == discriminators::MIGRATE_EVENT_LOG {
            return parse_migrate_log_instruction(
                &instruction_data[16..],
                accounts,
                signature,
                slot,
                tx_index,
                block_time_us,
                grpc_recv_us,
            );
        }
    }
    None
}

/// Parse buy/buy_exact_sol_in instruction
///
/// Account indices (from pump.json IDL), 15 个固定账户:
/// 0: global, 1: fee_recipient, 2: mint, 3: bonding_curve,
/// 4: associated_bonding_curve, 5: associated_user, 6: user,
/// 7: system_program, 8: token_program, 9: creator_vault,
/// 10: event_authority, 11: program, 12: global_volume_accumulator,
/// 13: user_volume_accumulator, 14: fee_config.
/// remaining_accounts 可能含 bonding_curve_v2 等。
#[allow(dead_code)]
fn parse_buy_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    block_time_us: Option<i64>,
    grpc_recv_us: i64,
) -> Option<DexEvent> {
    if accounts.len() < 7 {
        return None;
    }

    // Parse args: amount/spendable_sol_in (u64), max_sol_cost/min_tokens_out (u64)
    let (sol_amount, token_amount) = if data.len() >= 16 {
        (read_u64_le(data, 0).unwrap_or(0), read_u64_le(data, 8).unwrap_or(0))
    } else {
        (0, 0)
    };

    let mint = get_account(accounts, 2)?;
    let metadata =
        create_metadata(signature, slot, tx_index, block_time_us.unwrap_or_default(), grpc_recv_us);

    Some(DexEvent::PumpFunTrade(PumpFunTradeEvent {
        metadata,
        mint,
        is_buy: true,
        bonding_curve: get_account(accounts, 3).unwrap_or_default(),
        user: get_account(accounts, 6).unwrap_or_default(),
        sol_amount,
        token_amount,
        fee_recipient: get_account(accounts, 1).unwrap_or_default(),
        ..Default::default()
    }))
}

/// Parse sell instruction
///
/// Account indices (from pump.json IDL), 14 个固定账户:
/// 0: global, 1: fee_recipient, 2: mint, 3: bonding_curve,
/// 4: associated_bonding_curve, 5: associated_user, 6: user,
/// 7: system_program, 8: creator_vault, 9: token_program,
/// 10: event_authority, 11: program, 12: fee_config, 13: fee_program.
/// remaining_accounts 可能含 user_volume_accumulator（返现）、bonding_curve_v2 等。
#[allow(dead_code)]
fn parse_sell_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    block_time_us: Option<i64>,
    grpc_recv_us: i64,
) -> Option<DexEvent> {
    if accounts.len() < 7 {
        return None;
    }

    // Parse args: amount (u64), min_sol_output (u64)
    let (token_amount, sol_amount) = if data.len() >= 16 {
        (read_u64_le(data, 0).unwrap_or(0), read_u64_le(data, 8).unwrap_or(0))
    } else {
        (0, 0)
    };

    let mint = get_account(accounts, 2)?;
    let metadata =
        create_metadata(signature, slot, tx_index, block_time_us.unwrap_or_default(), grpc_recv_us);

    Some(DexEvent::PumpFunTrade(PumpFunTradeEvent {
        metadata,
        mint,
        is_buy: false,
        bonding_curve: get_account(accounts, 3).unwrap_or_default(),
        user: get_account(accounts, 6).unwrap_or_default(),
        sol_amount,
        token_amount,
        fee_recipient: get_account(accounts, 1).unwrap_or_default(),
        ..Default::default()
    }))
}

/// Parse create instruction (legacy)
///
/// Account indices (from pump.json):
/// 0: mint, 1: mint_authority, 2: bonding_curve, 3: associated_bonding_curve,
/// 4: global, 5: mpl_token_metadata, 6: metadata, 7: user. 共至少 8 个账户。
fn parse_create_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    block_time_us: Option<i64>,
    grpc_recv_us: i64,
) -> Option<DexEvent> {
    if accounts.len() < 8 {
        return None;
    }

    let mut offset = 0;

    // Parse args: name (string), symbol (string), uri (string), creator (pubkey)
    // String format: 4-byte length prefix + content
    let name = if let Some((s, len)) = read_str_unchecked(data, offset) {
        offset += len;
        s.to_string()
    } else {
        String::new()
    };

    let symbol = if let Some((s, len)) = read_str_unchecked(data, offset) {
        offset += len;
        s.to_string()
    } else {
        String::new()
    };

    let uri = if let Some((s, len)) = read_str_unchecked(data, offset) {
        offset += len;
        s.to_string()
    } else {
        String::new()
    };

    // 读取 mint, bonding_curve, user, creator (在 name, symbol, uri 之后)
    if data.len() < offset + 32 + 32 + 32 + 32 {
        return None;
    }

    let mint = read_pubkey(data, offset).unwrap_or_default();
    offset += 32;

    let bonding_curve = read_pubkey(data, offset).unwrap_or_default();
    offset += 32;

    let user = read_pubkey(data, offset).unwrap_or_default();
    offset += 32;

    let creator = read_pubkey(data, offset).unwrap_or_default();

    let metadata =
        create_metadata(signature, slot, tx_index, block_time_us.unwrap_or_default(), grpc_recv_us);

    Some(DexEvent::PumpFunCreate(PumpFunCreateTokenEvent {
        metadata,
        name,
        symbol,
        uri,
        mint,
        bonding_curve,
        user,
        creator,
        ..Default::default()
    }))
}

/// Parse create_v2 instruction (SPL-22；Mayhem 由 **data** 中 `is_mayhem_mode` 决定，不要用 mayhem 程序账户是否非空推断)
///
/// Account indices (idl pumpfun.json create_v2): 0 mint, 1 mint_authority, 2 bonding_curve,
/// 3 associated_bonding_curve, 4 global, 5 user, 6 system_program, 7 token_program,
/// 8 associated_token_program, 9 mayhem_program_id, 10 global_params, 11 sol_vault,
/// 12 mayhem_state, 13 mayhem_token_vault, 14 event_authority, 15 program. 共 16 个账户。
/// Instruction args (after disc): name, symbol, uri, creator, is_mayhem_mode (`bool`), is_cashback_enabled (`OptionBool` = 1-byte bool on wire)。
/// Guard: return None when accounts.len() < 16 to avoid index out of bounds (e.g. ALT-loaded tx).
fn parse_create_v2_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    block_time_us: Option<i64>,
    grpc_recv_us: i64,
) -> Option<DexEvent> {
    const CREATE_V2_MIN_ACCOUNTS: usize = 16;
    if accounts.len() < CREATE_V2_MIN_ACCOUNTS {
        return None;
    }
    let acc = &accounts[0..CREATE_V2_MIN_ACCOUNTS];

    // IDL args: name, symbol, uri, creator, is_mayhem_mode, is_cashback_enabled — mint/bc/user 仅在 accounts
    let mut offset = 0usize;
    let name = if let Some((s, len)) = read_str_unchecked(data, offset) {
        offset += len;
        s.to_string()
    } else {
        String::new()
    };
    let symbol = if let Some((s, len)) = read_str_unchecked(data, offset) {
        offset += len;
        s.to_string()
    } else {
        String::new()
    };
    let uri = if let Some((s, len)) = read_str_unchecked(data, offset) {
        offset += len;
        s.to_string()
    } else {
        String::new()
    };
    if data.len() < offset + 32 + 1 {
        return None;
    }
    let creator = read_pubkey(data, offset)?;
    offset += 32;
    let is_mayhem_mode = read_bool(data, offset)?;
    offset += 1;
    let is_cashback_enabled = read_option_bool_idl(data, offset).unwrap_or(false);

    let mint = acc[0];
    let bonding_curve = acc[2];
    let user = acc[5];

    let metadata =
        create_metadata(signature, slot, tx_index, block_time_us.unwrap_or_default(), grpc_recv_us);

    Some(DexEvent::PumpFunCreateV2(PumpFunCreateV2TokenEvent {
        metadata,
        name,
        symbol,
        uri,
        mint,
        bonding_curve,
        user,
        creator,
        mint_authority: acc[1],
        associated_bonding_curve: acc[3],
        global: acc[4],
        system_program: acc[6],
        token_program: acc[7],
        associated_token_program: acc[8],
        mayhem_program_id: acc[9],
        global_params: acc[10],
        sol_vault: acc[11],
        mayhem_state: acc[12],
        mayhem_token_vault: acc[13],
        event_authority: acc[14],
        program: acc[15],
        is_mayhem_mode,
        is_cashback_enabled,
        ..Default::default()
    }))
}

/// Parse Migrate CPI instruction
#[allow(unused_variables)]
fn parse_migrate_log_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    block_time_us: Option<i64>,
    rpc_recv_us: i64,
) -> Option<DexEvent> {
    let mut offset = 0;

    // user (Pubkey - 32 bytes)
    let user = read_pubkey(data, offset)?;
    offset += 32;

    // mint (Pubkey - 32 bytes)
    let mint = read_pubkey(data, offset)?;
    offset += 32;

    // mintAmount (u64 - 8 bytes)
    let mint_amount = read_u64_le(data, offset)?;
    offset += 8;

    // solAmount (u64 - 8 bytes)
    let sol_amount = read_u64_le(data, offset)?;
    offset += 8;

    // poolMigrationFee (u64 - 8 bytes)
    let pool_migration_fee = read_u64_le(data, offset)?;
    offset += 8;

    // bondingCurve (Pubkey - 32 bytes)
    let bonding_curve = read_pubkey(data, offset)?;
    offset += 32;

    // timestamp (i64 - 8 bytes)
    let timestamp = read_u64_le(data, offset)? as i64;
    offset += 8;

    // pool (Pubkey - 32 bytes)
    let pool = read_pubkey(data, offset)?;

    let metadata =
        create_metadata(signature, slot, tx_index, block_time_us.unwrap_or_default(), rpc_recv_us);

    Some(DexEvent::PumpFunMigrate(PumpFunMigrateEvent {
        metadata,
        user,
        mint,
        mint_amount,
        sol_amount,
        pool_migration_fee,
        bonding_curve,
        timestamp,
        pool,
    }))
}
