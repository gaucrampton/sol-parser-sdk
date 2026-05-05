//! ShredStream 热路径：Pump.fun **外层**指令解析（无 inner CPI）。
//!
//! - 与 `client.rs` 解耦，便于维护与 `#[inline]` 边界优化。
//! - 避免每笔交易克隆整张 `static_account_keys`、避免 `Vec<IxRef>` 指令副本。

use std::collections::HashSet;

use solana_sdk::message::VersionedMessage;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::VersionedTransaction;

use crate::accounts::program_ids::SPL_TOKEN_2022_PROGRAM_ID;
use crate::core::events::{
    DexEvent, EventMetadata, PumpFunCreateTokenEvent, PumpFunCreateV2TokenEvent,
    PumpFunMigrateBondingCurveCreatorEvent, PumpFunTradeEvent,
};
use crate::instr::pump::discriminators;
use crate::instr::utils::{
    read_bool, read_option_bool_idl, read_pubkey, read_str_unchecked, read_u64_le,
};
use crate::instr::pump::PROGRAM_ID_PUBKEY;

#[inline(always)]
fn token_program_or_default(token_program: Pubkey) -> Pubkey {
    if token_program == Pubkey::default() {
        SPL_TOKEN_2022_PROGRAM_ID
    } else {
        token_program
    }
}

#[inline]
fn scan_create_mint_from_ix(
    program_id_index: u8,
    ix_accounts: &[u8],
    data: &[u8],
    static_keys: &[Pubkey],
    created_mints: &mut HashSet<Pubkey>,
    mayhem_mints: &mut HashSet<Pubkey>,
) {
    let Some(program_id) = static_keys.get(program_id_index as usize) else {
        return;
    };
    if *program_id != PROGRAM_ID_PUBKEY || data.len() < 8 {
        return;
    }
    let disc: [u8; 8] = data[0..8].try_into().unwrap_or_default();
    if disc != discriminators::CREATE && disc != discriminators::CREATE_V2 {
        return;
    }
    let Some(&mint_idx) = ix_accounts.first() else {
        return;
    };
    let Some(&mint) = static_keys.get(mint_idx as usize) else {
        return;
    };
    created_mints.insert(mint);
    if disc == discriminators::CREATE_V2 {
        let is_mayhem = crate::instr::utils::parse_create_v2_tail_fields(&data[8..])
            .map(|(_, m, _)| m)
            .unwrap_or(false);
        if is_mayhem {
            mayhem_mints.insert(mint);
        }
    }
}

/// 第一遍：收集本笔交易内 Pump Create/CreateV2 的 mint（**零指令副本**，直接引用 message 内 `CompiledInstruction`）。
#[inline]
fn detect_pumpfun_create_mints(
    message: &VersionedMessage,
    static_keys: &[Pubkey],
) -> (HashSet<Pubkey>, HashSet<Pubkey>) {
    let mut created_mints = HashSet::new();
    let mut mayhem_mints = HashSet::new();
    match message {
        VersionedMessage::Legacy(msg) => {
            for ix in &msg.instructions {
                scan_create_mint_from_ix(
                    ix.program_id_index,
                    &ix.accounts,
                    &ix.data,
                    static_keys,
                    &mut created_mints,
                    &mut mayhem_mints,
                );
            }
        }
        VersionedMessage::V0(msg) => {
            for ix in &msg.instructions {
                scan_create_mint_from_ix(
                    ix.program_id_index,
                    &ix.accounts,
                    &ix.data,
                    static_keys,
                    &mut created_mints,
                    &mut mayhem_mints,
                );
            }
        }
    }
    (created_mints, mayhem_mints)
}

/// Pump.fun 外层或 pump_fees `create_fee_sharing_config` 外层，保持与交易内 ix 顺序一致。
#[inline]
fn dispatch_shred_outer(
    program_id_index: u8,
    ix_accounts: &[u8],
    data: &[u8],
    static_keys: &[Pubkey],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
    created_mints: &HashSet<Pubkey>,
    mayhem_mints: &HashSet<Pubkey>,
    events: &mut Vec<DexEvent>,
) {
    let Some(program_id) = static_keys.get(program_id_index as usize) else {
        return;
    };
    if *program_id == PROGRAM_ID_PUBKEY {
        if let Some(ev) = parse_pumpfun_instruction(
            data,
            static_keys,
            ix_accounts,
            signature,
            slot,
            tx_index,
            recv_us,
            created_mints,
            mayhem_mints,
        ) {
            events.push(ev);
        }
        return;
    }
    super::pfees_ix::try_push_pump_fees_outer_if_applicable(
        program_id_index,
        data,
        ix_accounts,
        static_keys,
        signature,
        slot,
        tx_index,
        recv_us,
        events,
    );
}

/// 解析交易中的 Pump 外层指令并写入 `events`（调用前 `events` 应已 `clear` 或按需复用容量）。
#[inline]
pub(crate) fn parse_transaction_pump_events(
    transaction: &VersionedTransaction,
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
    events: &mut Vec<DexEvent>,
) {
    let static_keys = transaction.message.static_account_keys();
    let (created_mints, mayhem_mints) =
        detect_pumpfun_create_mints(&transaction.message, static_keys);
    match &transaction.message {
        VersionedMessage::Legacy(msg) => {
            for ix in &msg.instructions {
                dispatch_shred_outer(
                    ix.program_id_index,
                    &ix.accounts,
                    &ix.data,
                    static_keys,
                    signature,
                    slot,
                    tx_index,
                    recv_us,
                    &created_mints,
                    &mayhem_mints,
                    events,
                );
            }
        }
        VersionedMessage::V0(msg) => {
            for ix in &msg.instructions {
                dispatch_shred_outer(
                    ix.program_id_index,
                    &ix.accounts,
                    &ix.data,
                    static_keys,
                    signature,
                    slot,
                    tx_index,
                    recv_us,
                    &created_mints,
                    &mayhem_mints,
                    events,
                );
            }
        }
    }
}

// --- 单条 outer ix 解析（由原 `client.rs` 迁入） ---

#[inline]
fn parse_pumpfun_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    ix_accounts: &[u8],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
    created_mints: &HashSet<Pubkey>,
    mayhem_mints: &HashSet<Pubkey>,
) -> Option<DexEvent> {
    if data.len() < 8 {
        return None;
    }
    let disc: [u8; 8] = data[0..8].try_into().ok()?;
    let ix_data = &data[8..];

    match disc {
        d if d == discriminators::CREATE => {
            parse_create_instruction(data, accounts, ix_accounts, signature, slot, tx_index, recv_us)
        }
        d if d == discriminators::CREATE_V2 => {
            parse_create_v2_instruction(data, accounts, ix_accounts, signature, slot, tx_index, recv_us)
        }
        d if d == discriminators::BUY => parse_buy_instruction(
            ix_data,
            accounts,
            ix_accounts,
            signature,
            slot,
            tx_index,
            recv_us,
            created_mints,
            mayhem_mints,
        ),
        d if d == discriminators::SELL => {
            parse_sell_instruction(ix_data, accounts, ix_accounts, signature, slot, tx_index, recv_us)
        }
        d if d == discriminators::BUY_EXACT_SOL_IN => parse_buy_exact_sol_in_instruction(
            ix_data,
            accounts,
            ix_accounts,
            signature,
            slot,
            tx_index,
            recv_us,
            created_mints,
            mayhem_mints,
        ),
        d if d == discriminators::MIGRATE_BONDING_CURVE_CREATOR => {
            parse_migrate_bonding_curve_creator_shred(
                accounts,
                ix_accounts,
                signature,
                slot,
                tx_index,
                recv_us,
            )
        }
        _ => None,
    }
}

/// `migrate_bonding_curve_creator` 外层 ix（`idls/pumpfun.json`）；无链上事件体时 `timestamp=0`，
/// `old_creator` 未知则填默认，`new_creator` 取 `sharing_config` 账户（与常见费分成迁移一致）。
#[inline]
fn parse_migrate_bonding_curve_creator_shred(
    accounts: &[Pubkey],
    ix_accounts: &[u8],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
) -> Option<DexEvent> {
    const MIN_ACC: usize = 5;
    if ix_accounts.len() < MIN_ACC {
        return None;
    }
    let get_account = |idx: usize| -> Option<Pubkey> {
        ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
    };
    let mint = get_account(0)?;
    let bonding_curve = get_account(1).unwrap_or_default();
    let sharing_config = get_account(2).unwrap_or_default();
    let metadata = EventMetadata {
        signature,
        slot,
        tx_index,
        block_time_us: 0,
        grpc_recv_us: recv_us,
        recent_blockhash: None,
    };
    Some(DexEvent::PumpFunMigrateBondingCurveCreator(
        PumpFunMigrateBondingCurveCreatorEvent {
            metadata,
            timestamp: 0,
            mint,
            bonding_curve,
            sharing_config,
            old_creator: Pubkey::default(),
            new_creator: sharing_config,
        },
    ))
}

#[inline]
fn parse_create_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    ix_accounts: &[u8],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
) -> Option<DexEvent> {
    if ix_accounts.len() < 10 {
        return None;
    }

    let get_account =
        |idx: usize| -> Option<Pubkey> { ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied() };

    let mut offset = 8;

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

    let creator = if offset + 32 <= data.len() {
        read_pubkey(data, offset).unwrap_or_default()
    } else {
        Pubkey::default()
    };

    let mint = get_account(0)?;
    let bonding_curve = get_account(2).unwrap_or_default();
    let user = get_account(7).unwrap_or_default();

    let metadata = EventMetadata {
        signature,
        slot,
        tx_index,
        block_time_us: 0,
        grpc_recv_us: recv_us,
        recent_blockhash: None,
    };

    Some(DexEvent::PumpFunCreate(PumpFunCreateTokenEvent {
        metadata,
        name,
        symbol,
        uri,
        mint,
        bonding_curve,
        user,
        creator,
        token_program: get_account(9).unwrap_or_default(),
        ..Default::default()
    }))
}

#[inline]
fn parse_create_v2_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    ix_accounts: &[u8],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
) -> Option<DexEvent> {
    const CREATE_V2_MIN_ACCOUNTS: usize = 16;
    if ix_accounts.len() < CREATE_V2_MIN_ACCOUNTS {
        return None;
    }

    let get_account =
        |idx: usize| -> Option<Pubkey> { ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied() };

    let payload = &data[8..];
    let mut offset = 0usize;
    let name = if let Some((s, len)) = read_str_unchecked(payload, offset) {
        offset += len;
        s.to_string()
    } else {
        String::new()
    };
    let symbol = if let Some((s, len)) = read_str_unchecked(payload, offset) {
        offset += len;
        s.to_string()
    } else {
        String::new()
    };
    let uri = if let Some((s, len)) = read_str_unchecked(payload, offset) {
        offset += len;
        s.to_string()
    } else {
        String::new()
    };
    if payload.len() < offset + 32 + 1 {
        return None;
    }
    let creator = read_pubkey(payload, offset).unwrap_or_default();
    offset += 32;
    let is_mayhem_mode = read_bool(payload, offset).unwrap_or(false);
    offset += 1;
    let is_cashback_enabled = read_option_bool_idl(payload, offset).unwrap_or(false);

    let mint = get_account(0)?;
    let bonding_curve = get_account(2).unwrap_or_default();
    let user = get_account(5).unwrap_or_default();

    let metadata = EventMetadata {
        signature,
        slot,
        tx_index,
        block_time_us: 0,
        grpc_recv_us: recv_us,
        recent_blockhash: None,
    };

    let mayhem_program_id = get_account(9).unwrap_or_default();

    Some(DexEvent::PumpFunCreateV2(PumpFunCreateV2TokenEvent {
        metadata,
        name,
        symbol,
        uri,
        mint,
        bonding_curve,
        user,
        creator,
        mint_authority: get_account(1).unwrap_or_default(),
        associated_bonding_curve: get_account(3).unwrap_or_default(),
        global: get_account(4).unwrap_or_default(),
        system_program: get_account(6).unwrap_or_default(),
        token_program: get_account(7).unwrap_or_default(),
        associated_token_program: get_account(8).unwrap_or_default(),
        mayhem_program_id,
        global_params: get_account(10).unwrap_or_default(),
        sol_vault: get_account(11).unwrap_or_default(),
        mayhem_state: get_account(12).unwrap_or_default(),
        mayhem_token_vault: get_account(13).unwrap_or_default(),
        event_authority: get_account(14).unwrap_or_default(),
        program: get_account(15).unwrap_or_default(),
        is_mayhem_mode,
        is_cashback_enabled,
        ..Default::default()
    }))
}

#[inline]
fn parse_buy_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    ix_accounts: &[u8],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
    created_mints: &HashSet<Pubkey>,
    mayhem_mints: &HashSet<Pubkey>,
) -> Option<DexEvent> {
    if ix_accounts.len() < 7 {
        return None;
    }

    let get_account =
        |idx: usize| -> Option<Pubkey> { ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied() };

    let (token_amount, sol_amount) = if data.len() >= 16 {
        (read_u64_le(data, 0).unwrap_or(0), read_u64_le(data, 8).unwrap_or(0))
    } else {
        (0, 0)
    };

    let mint = get_account(2)?;
    let is_created_buy = created_mints.contains(&mint);
    let is_mayhem_mode = mayhem_mints.contains(&mint);

    let metadata = EventMetadata {
        signature,
        slot,
        tx_index,
        block_time_us: 0,
        grpc_recv_us: recv_us,
        recent_blockhash: None,
    };

    Some(DexEvent::PumpFunTrade(PumpFunTradeEvent {
        metadata,
        mint,
        bonding_curve: get_account(3).unwrap_or_default(),
        user: get_account(6).unwrap_or_default(),
        sol_amount,
        token_amount,
        fee_recipient: get_account(1).unwrap_or_default(),
        is_buy: true,
        is_created_buy,
        timestamp: 0,
        virtual_sol_reserves: 0,
        virtual_token_reserves: 0,
        real_sol_reserves: 0,
        real_token_reserves: 0,
        fee_basis_points: 0,
        fee: 0,
        creator: Pubkey::default(),
        creator_fee_basis_points: 0,
        creator_fee: 0,
        track_volume: false,
        total_unclaimed_tokens: 0,
        total_claimed_tokens: 0,
        current_sol_volume: 0,
        last_update_timestamp: 0,
        ix_name: "buy".to_string(),
        mayhem_mode: is_mayhem_mode,
        cashback_fee_basis_points: 0,
        cashback: 0,
        is_cashback_coin: false,
        associated_bonding_curve: get_account(4).unwrap_or_default(),
        token_program: token_program_or_default(get_account(8).unwrap_or_default()),
        creator_vault: get_account(9).unwrap_or_default(),
        account: None,
    }))
}

#[inline]
fn parse_sell_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    ix_accounts: &[u8],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
) -> Option<DexEvent> {
    if ix_accounts.len() < 7 {
        return None;
    }

    let get_account =
        |idx: usize| -> Option<Pubkey> { ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied() };

    let (token_amount, sol_amount) = if data.len() >= 16 {
        (read_u64_le(data, 0).unwrap_or(0), read_u64_le(data, 8).unwrap_or(0))
    } else {
        (0, 0)
    };

    let mint = get_account(2)?;
    let metadata = EventMetadata {
        signature,
        slot,
        tx_index,
        block_time_us: 0,
        grpc_recv_us: recv_us,
        recent_blockhash: None,
    };

    Some(DexEvent::PumpFunTrade(PumpFunTradeEvent {
        metadata,
        mint,
        bonding_curve: get_account(3).unwrap_or_default(),
        user: get_account(6).unwrap_or_default(),
        sol_amount,
        token_amount,
        fee_recipient: get_account(1).unwrap_or_default(),
        is_buy: false,
        is_created_buy: false,
        timestamp: 0,
        virtual_sol_reserves: 0,
        virtual_token_reserves: 0,
        real_sol_reserves: 0,
        real_token_reserves: 0,
        fee_basis_points: 0,
        fee: 0,
        creator: Pubkey::default(),
        creator_fee_basis_points: 0,
        creator_fee: 0,
        track_volume: false,
        total_unclaimed_tokens: 0,
        total_claimed_tokens: 0,
        current_sol_volume: 0,
        last_update_timestamp: 0,
        ix_name: "sell".to_string(),
        mayhem_mode: false,
        cashback_fee_basis_points: 0,
        cashback: 0,
        is_cashback_coin: false,
        associated_bonding_curve: get_account(4).unwrap_or_default(),
        token_program: token_program_or_default(get_account(9).unwrap_or_default()),
        creator_vault: get_account(8).unwrap_or_default(),
        account: None,
    }))
}

#[inline]
fn parse_buy_exact_sol_in_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    ix_accounts: &[u8],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
    created_mints: &HashSet<Pubkey>,
    mayhem_mints: &HashSet<Pubkey>,
) -> Option<DexEvent> {
    if ix_accounts.len() < 7 {
        return None;
    }

    let get_account =
        |idx: usize| -> Option<Pubkey> { ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied() };

    let (sol_amount, token_amount) = if data.len() >= 16 {
        (read_u64_le(data, 0).unwrap_or(0), read_u64_le(data, 8).unwrap_or(0))
    } else {
        (0, 0)
    };

    let mint = get_account(2)?;
    let is_created_buy = created_mints.contains(&mint);
    let is_mayhem_mode = mayhem_mints.contains(&mint);

    let metadata = EventMetadata {
        signature,
        slot,
        tx_index,
        block_time_us: 0,
        grpc_recv_us: recv_us,
        recent_blockhash: None,
    };

    Some(DexEvent::PumpFunTrade(PumpFunTradeEvent {
        metadata,
        mint,
        bonding_curve: get_account(3).unwrap_or_default(),
        user: get_account(6).unwrap_or_default(),
        sol_amount,
        token_amount,
        fee_recipient: get_account(1).unwrap_or_default(),
        is_buy: true,
        is_created_buy,
        timestamp: 0,
        virtual_sol_reserves: 0,
        virtual_token_reserves: 0,
        real_sol_reserves: 0,
        real_token_reserves: 0,
        fee_basis_points: 0,
        fee: 0,
        creator: Pubkey::default(),
        creator_fee_basis_points: 0,
        creator_fee: 0,
        track_volume: false,
        total_unclaimed_tokens: 0,
        total_claimed_tokens: 0,
        current_sol_volume: 0,
        last_update_timestamp: 0,
        ix_name: "buy_exact_sol_in".to_string(),
        mayhem_mode: is_mayhem_mode,
        cashback_fee_basis_points: 0,
        cashback: 0,
        is_cashback_coin: false,
        associated_bonding_curve: get_account(4).unwrap_or_default(),
        token_program: token_program_or_default(get_account(8).unwrap_or_default()),
        creator_vault: get_account(9).unwrap_or_default(),
        account: None,
    }))
}
