//! ShredStream 热路径：DEX **外层**指令解析（无 inner CPI）。
//!
//! - 与 `client.rs` 解耦，便于维护与 `#[inline]` 边界优化。
//! - 避免每笔交易克隆整张 `static_account_keys`、避免 `Vec<IxRef>` 指令副本。
//! - Pump.fun 使用专用外层热路径；其它已支持 DEX 协议走统一指令解析入口。

use smallvec::SmallVec;
use solana_sdk::message::VersionedMessage;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::VersionedTransaction;

use crate::accounts::program_ids::SPL_TOKEN_2022_PROGRAM_ID;
use crate::core::events::{
    DexEvent, EventMetadata, PumpFunCreateTokenEvent, PumpFunCreateV2TokenEvent,
    PumpFunMigrateBondingCurveCreatorEvent, PumpFunTradeEvent,
};
use crate::grpc::types::EventTypeFilter;
use crate::instr::program_ids::{
    BONK_PROGRAM_ID, METEORA_DAMM_V2_PROGRAM_ID, METEORA_DLMM_PROGRAM_ID, METEORA_POOLS_PROGRAM_ID,
    ORCA_WHIRLPOOL_PROGRAM_ID, PUMPSWAP_PROGRAM_ID, PUMP_FEES_PROGRAM_ID,
    RAYDIUM_AMM_V4_PROGRAM_ID, RAYDIUM_CLMM_PROGRAM_ID, RAYDIUM_CPMM_PROGRAM_ID,
};
use crate::instr::pump::discriminators;
use crate::instr::pump::PROGRAM_ID_PUBKEY;
use crate::instr::utils::{
    read_bool, read_option_bool_idl, read_pubkey, read_str_unchecked, read_u64_le,
};

type PumpMintSet = SmallVec<[Pubkey; 4]>;

#[inline(always)]
fn push_unique_mint(mints: &mut PumpMintSet, mint: Pubkey) {
    if !mints.iter().any(|existing| *existing == mint) {
        mints.push(mint);
    }
}

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
    created_mints: &mut PumpMintSet,
    mayhem_mints: &mut PumpMintSet,
) {
    if instruction_references_loaded_key(program_id_index, ix_accounts, static_keys.len()) {
        return;
    }
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
    push_unique_mint(created_mints, mint);
    if disc == discriminators::CREATE_V2 {
        let is_mayhem = crate::instr::utils::parse_create_v2_tail_fields(&data[8..])
            .map(|(_, m, _)| m)
            .unwrap_or(false);
        if is_mayhem {
            push_unique_mint(mayhem_mints, mint);
        }
    }
}

#[inline(always)]
fn instruction_references_loaded_key(
    program_id_index: u8,
    ix_accounts: &[u8],
    static_key_len: usize,
) -> bool {
    program_id_index as usize >= static_key_len
        || ix_accounts.iter().any(|&idx| idx as usize >= static_key_len)
}

/// 第一遍：收集本笔交易内 Pump Create/CreateV2 的 mint（**零指令副本**，直接引用 message 内 `CompiledInstruction`）。
#[inline]
fn detect_pumpfun_create_mints(
    message: &VersionedMessage,
    static_keys: &[Pubkey],
) -> (PumpMintSet, PumpMintSet) {
    let mut created_mints = PumpMintSet::new();
    let mut mayhem_mints = PumpMintSet::new();
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

/// DEX 外层指令解析，保持与交易内 ix 顺序一致。
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
    filter: Option<&EventTypeFilter>,
    created_mints: &PumpMintSet,
    mayhem_mints: &PumpMintSet,
    events: &mut Vec<DexEvent>,
) {
    if instruction_references_loaded_key(program_id_index, ix_accounts, static_keys.len()) {
        log::trace!(
            target: "sol_parser_sdk::shredstream",
            "skip shred outer ix because it references ALT-loaded accounts; wait for gRPC meta"
        );
        return;
    }
    let Some(program_id) = static_keys.get(program_id_index as usize) else {
        return;
    };
    if *program_id == PROGRAM_ID_PUBKEY {
        if filter.is_some_and(|f| !f.includes_pumpfun()) {
            return;
        }
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
            if filter.map(|f| f.should_include_dex_event(&ev)).unwrap_or(true) {
                events.push(ev);
            }
        }
        return;
    }
    if !is_supported_unified_outer_program(program_id, filter) {
        return;
    }
    if let Some(ev) = parse_non_pump_dex_outer(
        *program_id,
        data,
        ix_accounts,
        static_keys,
        signature,
        slot,
        tx_index,
        recv_us,
        filter,
    ) {
        events.push(ev);
    }
}

#[inline(always)]
fn is_supported_unified_outer_program(
    program_id: &Pubkey,
    filter: Option<&EventTypeFilter>,
) -> bool {
    let Some(f) = filter else {
        return matches!(
            *program_id,
            PUMPSWAP_PROGRAM_ID
                | PUMP_FEES_PROGRAM_ID
                | BONK_PROGRAM_ID
                | RAYDIUM_CPMM_PROGRAM_ID
                | RAYDIUM_CLMM_PROGRAM_ID
                | RAYDIUM_AMM_V4_PROGRAM_ID
                | ORCA_WHIRLPOOL_PROGRAM_ID
                | METEORA_POOLS_PROGRAM_ID
                | METEORA_DAMM_V2_PROGRAM_ID
                | METEORA_DLMM_PROGRAM_ID
        );
    };

    if *program_id == PUMPSWAP_PROGRAM_ID {
        f.includes_pumpswap()
    } else if *program_id == PUMP_FEES_PROGRAM_ID {
        f.includes_pump_fees()
    } else if *program_id == BONK_PROGRAM_ID {
        f.includes_raydium_launchpad()
    } else if *program_id == RAYDIUM_CPMM_PROGRAM_ID {
        f.includes_raydium_cpmm()
    } else if *program_id == RAYDIUM_CLMM_PROGRAM_ID {
        f.includes_raydium_clmm()
    } else if *program_id == RAYDIUM_AMM_V4_PROGRAM_ID {
        f.includes_raydium_amm_v4()
    } else if *program_id == ORCA_WHIRLPOOL_PROGRAM_ID {
        f.includes_orca_whirlpool()
    } else if *program_id == METEORA_POOLS_PROGRAM_ID {
        f.includes_meteora_pools()
    } else if *program_id == METEORA_DAMM_V2_PROGRAM_ID {
        f.includes_meteora_damm_v2()
    } else if *program_id == METEORA_DLMM_PROGRAM_ID {
        f.includes_meteora_dlmm()
    } else {
        false
    }
}

#[inline]
fn parse_non_pump_dex_outer(
    program_id: Pubkey,
    data: &[u8],
    ix_accounts: &[u8],
    static_keys: &[Pubkey],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
    filter: Option<&EventTypeFilter>,
) -> Option<DexEvent> {
    let mut accounts: SmallVec<[Pubkey; 64]> = SmallVec::new();
    for &idx in ix_accounts {
        accounts.push(*static_keys.get(idx as usize)?);
    }
    crate::instr::parse_instruction_unified(
        data,
        &accounts,
        signature,
        slot,
        tx_index,
        None,
        recv_us,
        filter,
        &program_id,
    )
}

#[inline]
pub fn parse_transaction_dex_events(
    transaction: &VersionedTransaction,
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
    events: &mut Vec<DexEvent>,
) {
    parse_transaction_dex_events_with_filter(
        transaction,
        signature,
        slot,
        tx_index,
        recv_us,
        None,
        events,
    );
}

#[inline]
pub fn parse_transaction_dex_events_with_filter(
    transaction: &VersionedTransaction,
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
    filter: Option<&EventTypeFilter>,
    events: &mut Vec<DexEvent>,
) {
    parse_transaction_pump_events_with_filter(
        transaction,
        signature,
        slot,
        tx_index,
        recv_us,
        filter,
        events,
    );
}

#[inline]
fn parse_transaction_pump_events_with_filter(
    transaction: &VersionedTransaction,
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
    filter: Option<&EventTypeFilter>,
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
                    filter,
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
                    filter,
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
    created_mints: &PumpMintSet,
    mayhem_mints: &PumpMintSet,
) -> Option<DexEvent> {
    if data.len() < 8 {
        return None;
    }
    let disc: [u8; 8] = data[0..8].try_into().ok()?;
    let ix_data = &data[8..];

    match disc {
        d if d == discriminators::CREATE => parse_create_instruction(
            data,
            accounts,
            ix_accounts,
            signature,
            slot,
            tx_index,
            recv_us,
        ),
        d if d == discriminators::CREATE_V2 => parse_create_v2_instruction(
            data,
            accounts,
            ix_accounts,
            signature,
            slot,
            tx_index,
            recv_us,
        ),
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
        d if d == discriminators::SELL => parse_sell_instruction(
            ix_data,
            accounts,
            ix_accounts,
            signature,
            slot,
            tx_index,
            recv_us,
        ),
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
        d if d == discriminators::BUY_V2 => parse_buy_v2_instruction(
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
        d if d == discriminators::BUY_EXACT_QUOTE_IN_V2 => parse_buy_exact_quote_in_v2_instruction(
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
        d if d == discriminators::SELL_V2 => parse_sell_v2_instruction(
            ix_data,
            accounts,
            ix_accounts,
            signature,
            slot,
            tx_index,
            recv_us,
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
    Some(DexEvent::PumpFunMigrateBondingCurveCreator(PumpFunMigrateBondingCurveCreatorEvent {
        metadata,
        timestamp: 0,
        mint,
        bonding_curve,
        sharing_config,
        old_creator: Pubkey::default(),
        new_creator: sharing_config,
    }))
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

    let get_account = |idx: usize| -> Option<Pubkey> {
        ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
    };

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

    let get_account = |idx: usize| -> Option<Pubkey> {
        ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
    };

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
    created_mints: &PumpMintSet,
    mayhem_mints: &PumpMintSet,
) -> Option<DexEvent> {
    const LEGACY_BUY_ACCOUNTS: usize = 16;
    if ix_accounts.len() < LEGACY_BUY_ACCOUNTS {
        return None;
    }

    let get_account = |idx: usize| -> Option<Pubkey> {
        ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
    };

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

    Some(DexEvent::PumpFunBuy(PumpFunTradeEvent {
        metadata,
        mint,
        global: get_account(0).unwrap_or_default(),
        bonding_curve: get_account(3).unwrap_or_default(),
        bonding_curve_v2: get_account(16).unwrap_or_default(),
        associated_bonding_curve: get_account(4).unwrap_or_default(),
        associated_user: get_account(5).unwrap_or_default(),
        user: get_account(6).unwrap_or_default(),
        system_program: get_account(7).unwrap_or_default(),
        sol_amount,
        token_amount,
        amount: token_amount,
        max_sol_cost: sol_amount,
        min_sol_output: 0,
        spendable_sol_in: 0,
        spendable_quote_in: 0,
        min_tokens_out: 0,
        fee_recipient: get_account(1).unwrap_or_default(),
        token_program: token_program_or_default(get_account(8).unwrap_or_default()),
        creator_vault: get_account(9).unwrap_or_default(),
        event_authority: get_account(10).unwrap_or_default(),
        program: get_account(11).unwrap_or_default(),
        global_volume_accumulator: get_account(12).unwrap_or_default(),
        user_volume_accumulator: get_account(13).unwrap_or_default(),
        fee_config: get_account(14).unwrap_or_default(),
        fee_program: get_account(15).unwrap_or_default(),
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
        buyback_fee_recipient: get_account(17).unwrap_or_default(),
        account: get_account(17).filter(|pk| *pk != Pubkey::default()),
        ..Default::default()
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
    const LEGACY_SELL_ACCOUNTS: usize = 14;
    if ix_accounts.len() < LEGACY_SELL_ACCOUNTS {
        return None;
    }

    let get_account = |idx: usize| -> Option<Pubkey> {
        ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
    };

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

    Some(DexEvent::PumpFunSell(PumpFunTradeEvent {
        metadata,
        mint,
        global: get_account(0).unwrap_or_default(),
        bonding_curve: get_account(3).unwrap_or_default(),
        bonding_curve_v2: if ix_accounts.len() >= 17 {
            get_account(15).unwrap_or_default()
        } else {
            get_account(14).unwrap_or_default()
        },
        associated_bonding_curve: get_account(4).unwrap_or_default(),
        associated_user: get_account(5).unwrap_or_default(),
        user: get_account(6).unwrap_or_default(),
        system_program: get_account(7).unwrap_or_default(),
        sol_amount,
        token_amount,
        amount: token_amount,
        max_sol_cost: 0,
        min_sol_output: sol_amount,
        spendable_sol_in: 0,
        spendable_quote_in: 0,
        min_tokens_out: 0,
        fee_recipient: get_account(1).unwrap_or_default(),
        token_program: token_program_or_default(get_account(9).unwrap_or_default()),
        creator_vault: get_account(8).unwrap_or_default(),
        event_authority: get_account(10).unwrap_or_default(),
        program: get_account(11).unwrap_or_default(),
        global_volume_accumulator: Pubkey::default(),
        user_volume_accumulator: if ix_accounts.len() >= 17 {
            get_account(14).unwrap_or_default()
        } else {
            Pubkey::default()
        },
        fee_config: get_account(12).unwrap_or_default(),
        fee_program: get_account(13).unwrap_or_default(),
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
        buyback_fee_recipient: if ix_accounts.len() >= 17 {
            get_account(16).unwrap_or_default()
        } else {
            get_account(15).unwrap_or_default()
        },
        account: if ix_accounts.len() >= 17 {
            get_account(16).filter(|pk| *pk != Pubkey::default())
        } else {
            get_account(15).filter(|pk| *pk != Pubkey::default())
        },
        ..Default::default()
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
    created_mints: &PumpMintSet,
    mayhem_mints: &PumpMintSet,
) -> Option<DexEvent> {
    const LEGACY_BUY_ACCOUNTS: usize = 16;
    if ix_accounts.len() < LEGACY_BUY_ACCOUNTS {
        return None;
    }

    let get_account = |idx: usize| -> Option<Pubkey> {
        ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
    };

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

    Some(DexEvent::PumpFunBuyExactSolIn(PumpFunTradeEvent {
        metadata,
        mint,
        global: get_account(0).unwrap_or_default(),
        bonding_curve: get_account(3).unwrap_or_default(),
        bonding_curve_v2: get_account(16).unwrap_or_default(),
        associated_bonding_curve: get_account(4).unwrap_or_default(),
        associated_user: get_account(5).unwrap_or_default(),
        user: get_account(6).unwrap_or_default(),
        system_program: get_account(7).unwrap_or_default(),
        sol_amount,
        token_amount,
        amount: token_amount,
        max_sol_cost: sol_amount,
        min_sol_output: 0,
        spendable_sol_in: sol_amount,
        spendable_quote_in: 0,
        min_tokens_out: token_amount,
        fee_recipient: get_account(1).unwrap_or_default(),
        token_program: token_program_or_default(get_account(8).unwrap_or_default()),
        creator_vault: get_account(9).unwrap_or_default(),
        event_authority: get_account(10).unwrap_or_default(),
        program: get_account(11).unwrap_or_default(),
        global_volume_accumulator: get_account(12).unwrap_or_default(),
        user_volume_accumulator: get_account(13).unwrap_or_default(),
        fee_config: get_account(14).unwrap_or_default(),
        fee_program: get_account(15).unwrap_or_default(),
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
        buyback_fee_recipient: get_account(17).unwrap_or_default(),
        account: get_account(17).filter(|pk| *pk != Pubkey::default()),
        ..Default::default()
    }))
}

/// `buy_v2`：27 个固定账户（IDL `buy_v2`）；mint=#1 bonding_curve=#10 user=#13 fee=#6 base_token_program=#3。
#[inline]
fn parse_buy_v2_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    ix_accounts: &[u8],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
    created_mints: &PumpMintSet,
    mayhem_mints: &PumpMintSet,
) -> Option<DexEvent> {
    const MIN_ACC: usize = 27;
    if ix_accounts.len() < MIN_ACC {
        return None;
    }

    let get_account = |idx: usize| -> Option<Pubkey> {
        ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
    };

    let (token_amount, sol_amount) = if data.len() >= 16 {
        (read_u64_le(data, 0).unwrap_or(0), read_u64_le(data, 8).unwrap_or(0))
    } else {
        (0, 0)
    };

    let mint = get_account(1)?;
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

    Some(DexEvent::PumpFunBuy(PumpFunTradeEvent {
        metadata,
        mint,
        quote_mint: get_account(2).unwrap_or_default(),
        global: get_account(0).unwrap_or_default(),
        bonding_curve: get_account(10).unwrap_or_default(),
        associated_bonding_curve: get_account(11).unwrap_or_default(),
        associated_quote_bonding_curve: get_account(12).unwrap_or_default(),
        associated_user: get_account(14).unwrap_or_default(),
        associated_quote_user: get_account(15).unwrap_or_default(),
        user: get_account(13).unwrap_or_default(),
        system_program: get_account(24).unwrap_or_default(),
        sol_amount,
        token_amount,
        amount: token_amount,
        max_sol_cost: sol_amount,
        min_sol_output: 0,
        spendable_sol_in: 0,
        spendable_quote_in: 0,
        min_tokens_out: 0,
        fee_recipient: get_account(6).unwrap_or_default(),
        token_program: token_program_or_default(get_account(3).unwrap_or_default()),
        quote_token_program: token_program_or_default(get_account(4).unwrap_or_default()),
        associated_token_program: get_account(5).unwrap_or_default(),
        creator_vault: get_account(16).unwrap_or_default(),
        associated_quote_fee_recipient: get_account(7).unwrap_or_default(),
        buyback_fee_recipient: get_account(8).unwrap_or_default(),
        associated_quote_buyback_fee_recipient: get_account(9).unwrap_or_default(),
        associated_creator_vault: get_account(17).unwrap_or_default(),
        sharing_config: get_account(18).unwrap_or_default(),
        event_authority: get_account(25).unwrap_or_default(),
        program: get_account(26).unwrap_or_default(),
        global_volume_accumulator: get_account(19).unwrap_or_default(),
        user_volume_accumulator: get_account(20).unwrap_or_default(),
        associated_user_volume_accumulator: get_account(21).unwrap_or_default(),
        fee_config: get_account(22).unwrap_or_default(),
        fee_program: get_account(23).unwrap_or_default(),
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
        account: None,
        ..Default::default()
    }))
}

#[inline]
fn parse_buy_exact_quote_in_v2_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    ix_accounts: &[u8],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
    created_mints: &PumpMintSet,
    mayhem_mints: &PumpMintSet,
) -> Option<DexEvent> {
    const MIN_ACC: usize = 27;
    if ix_accounts.len() < MIN_ACC {
        return None;
    }

    let get_account = |idx: usize| -> Option<Pubkey> {
        ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
    };

    let (sol_amount, token_amount) = if data.len() >= 16 {
        (read_u64_le(data, 0).unwrap_or(0), read_u64_le(data, 8).unwrap_or(0))
    } else {
        (0, 0)
    };

    let mint = get_account(1)?;
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

    Some(DexEvent::PumpFunBuy(PumpFunTradeEvent {
        metadata,
        mint,
        quote_mint: get_account(2).unwrap_or_default(),
        global: get_account(0).unwrap_or_default(),
        bonding_curve: get_account(10).unwrap_or_default(),
        associated_bonding_curve: get_account(11).unwrap_or_default(),
        associated_quote_bonding_curve: get_account(12).unwrap_or_default(),
        associated_user: get_account(14).unwrap_or_default(),
        associated_quote_user: get_account(15).unwrap_or_default(),
        user: get_account(13).unwrap_or_default(),
        system_program: get_account(24).unwrap_or_default(),
        sol_amount,
        token_amount,
        amount: token_amount,
        max_sol_cost: 0,
        quote_amount: sol_amount,
        min_sol_output: 0,
        spendable_sol_in: 0,
        spendable_quote_in: sol_amount,
        min_tokens_out: token_amount,
        fee_recipient: get_account(6).unwrap_or_default(),
        token_program: token_program_or_default(get_account(3).unwrap_or_default()),
        quote_token_program: token_program_or_default(get_account(4).unwrap_or_default()),
        associated_token_program: get_account(5).unwrap_or_default(),
        creator_vault: get_account(16).unwrap_or_default(),
        associated_quote_fee_recipient: get_account(7).unwrap_or_default(),
        buyback_fee_recipient: get_account(8).unwrap_or_default(),
        associated_quote_buyback_fee_recipient: get_account(9).unwrap_or_default(),
        associated_creator_vault: get_account(17).unwrap_or_default(),
        sharing_config: get_account(18).unwrap_or_default(),
        event_authority: get_account(25).unwrap_or_default(),
        program: get_account(26).unwrap_or_default(),
        global_volume_accumulator: get_account(19).unwrap_or_default(),
        user_volume_accumulator: get_account(20).unwrap_or_default(),
        associated_user_volume_accumulator: get_account(21).unwrap_or_default(),
        fee_config: get_account(22).unwrap_or_default(),
        fee_program: get_account(23).unwrap_or_default(),
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
        ix_name: "buy_exact_quote_in".to_string(),
        mayhem_mode: is_mayhem_mode,
        cashback_fee_basis_points: 0,
        cashback: 0,
        is_cashback_coin: false,
        account: None,
        ..Default::default()
    }))
}

/// `sell_v2`：26 个固定账户（IDL `sell_v2`）。
#[inline]
fn parse_sell_v2_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    ix_accounts: &[u8],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
) -> Option<DexEvent> {
    const MIN_ACC: usize = 26;
    if ix_accounts.len() < MIN_ACC {
        return None;
    }

    let get_account = |idx: usize| -> Option<Pubkey> {
        ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
    };

    let (token_amount, sol_amount) = if data.len() >= 16 {
        (read_u64_le(data, 0).unwrap_or(0), read_u64_le(data, 8).unwrap_or(0))
    } else {
        (0, 0)
    };

    let mint = get_account(1)?;

    let metadata = EventMetadata {
        signature,
        slot,
        tx_index,
        block_time_us: 0,
        grpc_recv_us: recv_us,
        recent_blockhash: None,
    };

    Some(DexEvent::PumpFunSell(PumpFunTradeEvent {
        metadata,
        mint,
        quote_mint: get_account(2).unwrap_or_default(),
        global: get_account(0).unwrap_or_default(),
        bonding_curve: get_account(10).unwrap_or_default(),
        associated_bonding_curve: get_account(11).unwrap_or_default(),
        associated_quote_bonding_curve: get_account(12).unwrap_or_default(),
        associated_user: get_account(14).unwrap_or_default(),
        associated_quote_user: get_account(15).unwrap_or_default(),
        user: get_account(13).unwrap_or_default(),
        system_program: get_account(23).unwrap_or_default(),
        sol_amount,
        token_amount,
        amount: token_amount,
        max_sol_cost: 0,
        min_sol_output: sol_amount,
        spendable_sol_in: 0,
        spendable_quote_in: 0,
        min_tokens_out: 0,
        fee_recipient: get_account(6).unwrap_or_default(),
        token_program: token_program_or_default(get_account(3).unwrap_or_default()),
        quote_token_program: token_program_or_default(get_account(4).unwrap_or_default()),
        associated_token_program: get_account(5).unwrap_or_default(),
        creator_vault: get_account(16).unwrap_or_default(),
        associated_quote_fee_recipient: get_account(7).unwrap_or_default(),
        buyback_fee_recipient: get_account(8).unwrap_or_default(),
        associated_quote_buyback_fee_recipient: get_account(9).unwrap_or_default(),
        associated_creator_vault: get_account(17).unwrap_or_default(),
        sharing_config: get_account(18).unwrap_or_default(),
        event_authority: get_account(24).unwrap_or_default(),
        program: get_account(25).unwrap_or_default(),
        global_volume_accumulator: Pubkey::default(),
        user_volume_accumulator: get_account(19).unwrap_or_default(),
        associated_user_volume_accumulator: get_account(20).unwrap_or_default(),
        fee_config: get_account(21).unwrap_or_default(),
        fee_program: get_account(22).unwrap_or_default(),
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
        account: None,
        ..Default::default()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::Signature;

    fn unique_accounts(n: usize) -> Vec<Pubkey> {
        (0..n).map(|_| Pubkey::new_unique()).collect()
    }

    fn ix_accounts(n: usize) -> Vec<u8> {
        (0..n).map(|i| i as u8).collect()
    }

    fn amount_data(first: u64, second: u64) -> Vec<u8> {
        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&first.to_le_bytes());
        data.extend_from_slice(&second.to_le_bytes());
        data
    }

    #[test]
    fn unified_shred_outer_programs_cover_supported_protocols() {
        for program_id in [
            PUMPSWAP_PROGRAM_ID,
            PUMP_FEES_PROGRAM_ID,
            BONK_PROGRAM_ID,
            RAYDIUM_CPMM_PROGRAM_ID,
            RAYDIUM_CLMM_PROGRAM_ID,
            RAYDIUM_AMM_V4_PROGRAM_ID,
            ORCA_WHIRLPOOL_PROGRAM_ID,
            METEORA_POOLS_PROGRAM_ID,
            METEORA_DAMM_V2_PROGRAM_ID,
            METEORA_DLMM_PROGRAM_ID,
        ] {
            assert!(
                is_supported_unified_outer_program(&program_id, None),
                "ShredStream outer parser missing {program_id}"
            );
        }
    }

    #[test]
    fn unified_shred_outer_program_filter_skips_unrequested_protocols() {
        let raydium_only =
            EventTypeFilter::include_only(vec![crate::grpc::types::EventType::RaydiumCpmmSwap]);

        assert!(is_supported_unified_outer_program(&RAYDIUM_CPMM_PROGRAM_ID, Some(&raydium_only)));
        assert!(!is_supported_unified_outer_program(
            &ORCA_WHIRLPOOL_PROGRAM_ID,
            Some(&raydium_only)
        ));
    }

    #[test]
    fn shred_pumpfun_trade_variants_are_specific_and_keep_exact_fields() {
        let accounts = unique_accounts(27);
        let legacy_buy_ix = ix_accounts(18);
        let legacy_sell_ix = ix_accounts(17);
        let v2_ix = ix_accounts(27);
        let no_created = PumpMintSet::new();
        let no_mayhem = PumpMintSet::new();

        let buy = parse_buy_instruction(
            &amount_data(100, 200),
            &accounts,
            &legacy_buy_ix,
            Signature::default(),
            1,
            0,
            9,
            &no_created,
            &no_mayhem,
        )
        .expect("buy");
        match buy {
            DexEvent::PumpFunBuy(t) => {
                assert_eq!(t.bonding_curve_v2, accounts[16]);
                assert_eq!(t.buyback_fee_recipient, accounts[17]);
            }
            other => panic!("expected buy variant, got {other:?}"),
        }

        let sell = parse_sell_instruction(
            &amount_data(300, 400),
            &accounts,
            &legacy_sell_ix,
            Signature::default(),
            1,
            0,
            9,
        )
        .expect("sell");
        match sell {
            DexEvent::PumpFunSell(t) => {
                assert_eq!(t.user_volume_accumulator, accounts[14]);
                assert_eq!(t.bonding_curve_v2, accounts[15]);
                assert_eq!(t.buyback_fee_recipient, accounts[16]);
            }
            other => panic!("expected sell variant, got {other:?}"),
        }

        let exact_quote = parse_buy_exact_quote_in_v2_instruction(
            &amount_data(500, 600),
            &accounts,
            &v2_ix,
            Signature::default(),
            1,
            0,
            9,
            &no_created,
            &no_mayhem,
        )
        .expect("exact quote buy");

        match exact_quote {
            DexEvent::PumpFunBuy(t) => {
                assert_eq!(t.ix_name, "buy_exact_quote_in");
                assert_eq!(t.spendable_quote_in, 500);
                assert_eq!(t.min_tokens_out, 600);
                assert_eq!(t.max_sol_cost, 0);
                assert_eq!(t.quote_amount, 500);
                assert_eq!(t.quote_mint, accounts[2]);
                assert_eq!(t.associated_quote_user, accounts[15]);
                assert_eq!(t.associated_creator_vault, accounts[17]);
                assert_eq!(t.sharing_config, accounts[18]);
                assert_eq!(t.global_volume_accumulator, accounts[19]);
                assert_eq!(t.fee_program, accounts[23]);
            }
            other => panic!("expected exact buy variant, got {other:?}"),
        }
    }

    #[test]
    fn shred_pumpfun_legacy_trade_rejects_short_account_lists() {
        let accounts = unique_accounts(16);
        let no_created = PumpMintSet::new();
        let no_mayhem = PumpMintSet::new();

        assert!(parse_buy_instruction(
            &amount_data(100, 200),
            &accounts,
            &ix_accounts(15),
            Signature::default(),
            1,
            0,
            9,
            &no_created,
            &no_mayhem,
        )
        .is_none());

        assert!(parse_sell_instruction(
            &amount_data(300, 400),
            &accounts,
            &ix_accounts(13),
            Signature::default(),
            1,
            0,
            9,
        )
        .is_none());
    }
}
