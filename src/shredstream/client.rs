//! ShredStream 客户端

use std::collections::HashSet;
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use futures::StreamExt;
use solana_entry::entry::Entry as SolanaEntry;
use solana_sdk::message::VersionedMessage;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::accounts::program_ids::SPL_TOKEN_2022_PROGRAM_ID;
use crate::core::now_micros;
use crate::shredstream::config::ShredStreamConfig;
use crate::shredstream::proto::{Entry, ShredstreamProxyClient, SubscribeEntriesRequest};
use crate::DexEvent;

/// 获取 token_program，如果为 default 则返回 Token-2022 Program
/// 默认使用 Token-2022 更安全，因为 Token-2022 兼容 Token 账户，反之则不行
#[inline]
fn get_token_program_or_default(token_program: Pubkey) -> Pubkey {
    if token_program == Pubkey::default() {
        SPL_TOKEN_2022_PROGRAM_ID
    } else {
        token_program
    }
}

// IxRef 类型定义 - 用于包装指令数据
// 避免直接导入 CompiledInstruction 以解决版本冲突
#[derive(Debug, Clone)]
struct IxRef {
    program_id_index: u8,
    accounts: Vec<u8>,
    data: Vec<u8>,
}

impl IxRef {
    fn new(program_id_index: u8, accounts: Vec<u8>, data: Vec<u8>) -> Self {
        Self {
            program_id_index,
            accounts,
            data,
        }
    }
}

/// ShredStream 客户端
#[derive(Clone)]
pub struct ShredStreamClient {
    endpoint: String,
    config: ShredStreamConfig,
    subscription_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl ShredStreamClient {
    /// 创建新客户端
    pub async fn new(endpoint: impl Into<String>) -> crate::common::AnyResult<Self> {
        Self::new_with_config(endpoint, ShredStreamConfig::default()).await
    }

    /// 使用自定义配置创建客户端
    pub async fn new_with_config(
        endpoint: impl Into<String>,
        config: ShredStreamConfig,
    ) -> crate::common::AnyResult<Self> {
        let endpoint = endpoint.into();
        // 测试连接
        let _ = ShredstreamProxyClient::connect(endpoint.clone()).await?;

        Ok(Self { endpoint, config, subscription_handle: Arc::new(Mutex::new(None)) })
    }

    /// 订阅 DEX 事件（自动重连）
    ///
    /// 返回一个队列，事件会被推送到该队列中
    pub async fn subscribe(&self) -> crate::common::AnyResult<Arc<ArrayQueue<DexEvent>>> {
        // 停止现有订阅
        self.stop().await;

        let queue = Arc::new(ArrayQueue::new(100_000));
        let queue_clone = Arc::clone(&queue);

        let endpoint = self.endpoint.clone();
        let config = self.config.clone();

        let handle = tokio::spawn(async move {
            let mut delay = config.reconnect_delay_ms;
            let mut attempts = 0u32;

            loop {
                if config.max_reconnect_attempts > 0 && attempts >= config.max_reconnect_attempts {
                    log::error!("Max reconnection attempts reached, giving up");
                    break;
                }
                attempts += 1;

                match Self::stream_events(&endpoint, &queue_clone).await {
                    Ok(_) => {
                        delay = config.reconnect_delay_ms;
                        attempts = 0;
                    }
                    Err(e) => {
                        log::error!("ShredStream error: {} - retry in {}ms", e, delay);
                        tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
                        delay = (delay * 2).min(60_000);
                    }
                }
            }
        });

        *self.subscription_handle.lock().await = Some(handle);
        Ok(queue)
    }

    /// 停止订阅
    pub async fn stop(&self) {
        if let Some(handle) = self.subscription_handle.lock().await.take() {
            handle.abort();
        }
    }

    /// 核心事件流处理
    async fn stream_events(
        endpoint: &str,
        queue: &Arc<ArrayQueue<DexEvent>>,
    ) -> Result<(), String> {
        let mut client = ShredstreamProxyClient::connect(endpoint.to_string())
            .await
            .map_err(|e| e.to_string())?;
        let request = tonic::Request::new(SubscribeEntriesRequest {});
        let mut stream =
            client.subscribe_entries(request).await.map_err(|e| e.to_string())?.into_inner();

        log::info!("ShredStream connected, receiving entries...");

        while let Some(message) = stream.next().await {
            match message {
                Ok(entry) => {
                    Self::process_entry(entry, queue);
                }
                Err(e) => {
                    log::error!("Stream error: {:?}", e);
                    return Err(e.to_string());
                }
            }
        }

        Ok(())
    }

    /// 处理单个 Entry 消息
    #[inline]
    fn process_entry(entry: Entry, queue: &Arc<ArrayQueue<DexEvent>>) {
        let slot = entry.slot;
        let recv_us = now_micros();

        // 反序列化 Entry 数据
        let entries = match bincode::deserialize::<Vec<SolanaEntry>>(&entry.entries) {
            Ok(e) => e,
            Err(e) => {
                log::debug!("Failed to deserialize entries: {}", e);
                return;
            }
        };

        // 处理每个 Entry 中的交易
        for entry in entries {
            for (tx_index, transaction) in entry.transactions.iter().enumerate() {
                Self::process_transaction(transaction, slot, recv_us, tx_index as u64, queue);
            }
        }
    }

    /// 处理单个交易
    #[inline]
    fn process_transaction(
        transaction: &solana_sdk::transaction::VersionedTransaction,
        slot: u64,
        recv_us: i64,
        tx_index: u64,
        queue: &Arc<ArrayQueue<DexEvent>>,
    ) {
        if transaction.signatures.is_empty() {
            return;
        }

        let signature = transaction.signatures[0];
        if let VersionedMessage::V0(m) = &transaction.message {
            if !m.address_table_lookups.is_empty() {
                log::debug!(
                    target: "sol_parser_sdk::shredstream",
                    "V0 tx uses address lookup tables; only static keys are available — \
                     some instruction account indices may resolve to wrong pubkeys (often only 1 BUY gets is_created_buy)"
                );
            }
        }
        let accounts: Vec<Pubkey> = transaction.message.static_account_keys().to_vec();

        // 解析交易中的指令
        let mut events = Vec::new();
        Self::parse_transaction_instructions(
            transaction,
            &accounts,
            signature,
            slot,
            tx_index,
            recv_us,
            &mut events,
        );
        crate::core::pumpfun_fee_enrich::enrich_create_v2_observed_fee_recipient(&mut events);

        // 推送到队列
        for mut event in events {
            // 填充接收时间戳
            if let Some(meta) = event.metadata_mut() {
                meta.grpc_recv_us = recv_us;
            }
            let _ = queue.push(event);
        }
    }

    /// 解析交易指令，提取 PumpFun 事件
    #[inline]
    fn parse_transaction_instructions(
        transaction: &solana_sdk::transaction::VersionedTransaction,
        accounts: &[solana_sdk::pubkey::Pubkey],
        signature: solana_sdk::signature::Signature,
        slot: u64,
        tx_index: u64,
        recv_us: i64,
        events: &mut Vec<DexEvent>,
    ) {
        use solana_sdk::message::VersionedMessage;

        let message = &transaction.message;

        // 获取所有指令
        let instructions: Vec<IxRef> = match message {
            VersionedMessage::Legacy(msg) => {
                msg.instructions.iter().map(|ix| IxRef::new(ix.program_id_index, ix.accounts.clone(), ix.data.clone())).collect()
            }
            VersionedMessage::V0(msg) => {
                msg.instructions.iter().map(|ix| IxRef::new(ix.program_id_index, ix.accounts.clone(), ix.data.clone())).collect()
            }
        };

        // 检测 CREATE/CREATE_V2 指令创建的 mint 地址（用于精确判断 is_created_buy 和 mayhem_mode）
        let (created_mints, mayhem_mints) = Self::detect_pumpfun_create_mints(&instructions, accounts);

        // 解析每个指令
        for ix in &instructions {
            let program_id = accounts.get(ix.program_id_index as usize);

            // 只处理 PumpFun 指令
            if let Some(program_id) = program_id {
                if *program_id == crate::instr::pump::PROGRAM_ID_PUBKEY {
                    if let Some(event) = Self::parse_pumpfun_instruction(
                        &ix.data,
                        accounts,
                        &ix.accounts,
                        signature,
                        slot,
                        tx_index,
                        recv_us,
                        &created_mints,
                        &mayhem_mints,
                    ) {
                        events.push(event);
                    }
                }
            }
        }
    }

    /// 检测交易中 PumpFun CREATE/CREATE_V2 指令创建的 mint 地址
    /// 返回 (created_mints, mayhem_mints) 元组：
    /// - created_mints: 所有创建的 mint 地址集合（用于精确判断 is_created_buy）
    /// - mayhem_mints: Mayhem Mode 代币的 mint 地址集合
    ///
    /// Mayhem Mode 判断方式（与 IDL `create_v2` 指令数据中的 `is_mayhem_mode` 一致）：
    /// - CREATE_V2：从 ix data（disc 之后）解析 `is_mayhem_mode`，**不能**用账户 #10 Mayhem Program 推断（非 Mayhem 时该账户仍存在）
    /// - CREATE 指令创建的代币不是 Mayhem Mode
    #[inline]
    fn detect_pumpfun_create_mints(
        instructions: &[IxRef],
        accounts: &[Pubkey],
    ) -> (HashSet<Pubkey>, HashSet<Pubkey>) {
        use crate::instr::pump::discriminators;

        let mut created_mints = HashSet::new();
        let mut mayhem_mints = HashSet::new();

        for ix in instructions {
            if let Some(program_id) = accounts.get(ix.program_id_index as usize) {
                if *program_id == crate::instr::pump::PROGRAM_ID_PUBKEY {
                    if ix.data.len() >= 8 {
                        let disc: [u8; 8] = ix.data[0..8].try_into().unwrap_or_default();
                        if disc == discriminators::CREATE || disc == discriminators::CREATE_V2 {
                            // CREATE/CREATE_V2 指令中 mint 在账户索引 0
                            if let Some(&mint_idx) = ix.accounts.get(0) {
                                if let Some(&mint) = accounts.get(mint_idx as usize) {
                                    created_mints.insert(mint);

                                    if disc == discriminators::CREATE_V2 {
                                        let is_mayhem = crate::instr::utils::parse_create_v2_tail_fields(
                                            &ix.data[8..],
                                        )
                                        .map(|(_, m, _)| m)
                                        .unwrap_or(false);
                                        if is_mayhem {
                                            mayhem_mints.insert(mint);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        (created_mints, mayhem_mints)
    }

    /// 解析单个 PumpFun 指令
    #[inline]
    fn parse_pumpfun_instruction(
        data: &[u8],
        accounts: &[Pubkey],
        ix_accounts: &[u8],
        signature: solana_sdk::signature::Signature,
        slot: u64,
        tx_index: u64,
        recv_us: i64,
        created_mints: &HashSet<Pubkey>,
        mayhem_mints: &HashSet<Pubkey>,
    ) -> Option<DexEvent> {
        use crate::instr::pump::discriminators;
        use crate::instr::utils::*;

        if data.len() < 8 {
            return None;
        }

        let disc: [u8; 8] = data[0..8].try_into().ok()?;
        let ix_data = &data[8..];

        // 获取指令中的账户
        let get_account = |idx: usize| -> Option<Pubkey> {
            ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
        };

        match disc {
            // CREATE 指令
            d if d == discriminators::CREATE => {
                Self::parse_create_instruction(data, accounts, ix_accounts, signature, slot, tx_index, recv_us)
            }
            // CREATE_V2 指令
            d if d == discriminators::CREATE_V2 => {
                Self::parse_create_v2_instruction(data, accounts, ix_accounts, signature, slot, tx_index, recv_us)
            }
            // BUY 指令
            d if d == discriminators::BUY => {
                Self::parse_buy_instruction(
                    ix_data,
                    accounts,
                    ix_accounts,
                    signature,
                    slot,
                    tx_index,
                    recv_us,
                    created_mints,
                    mayhem_mints,
                )
            }
            // SELL 指令
            d if d == discriminators::SELL => {
                Self::parse_sell_instruction(ix_data, accounts, ix_accounts, signature, slot, tx_index, recv_us)
            }
            // BUY_EXACT_SOL_IN 指令
            d if d == discriminators::BUY_EXACT_SOL_IN => {
                Self::parse_buy_exact_sol_in_instruction(
                    ix_data,
                    accounts,
                    ix_accounts,
                    signature,
                    slot,
                    tx_index,
                    recv_us,
                    created_mints,
                    mayhem_mints,
                )
            }
            _ => None,
        }
    }

    /// 解析 CREATE 指令
    ///
    /// CREATE 指令账户映射 (from IDL):
    /// 0: mint, 1: mint_authority, 2: bonding_curve, 3: associated_bonding_curve,
    /// 4: global, 5: mpl_token_metadata, 6: metadata, 7: user, ...
    #[inline]
    fn parse_create_instruction(
        data: &[u8],
        accounts: &[solana_sdk::pubkey::Pubkey],
        ix_accounts: &[u8],
        signature: solana_sdk::signature::Signature,
        slot: u64,
        tx_index: u64,
        recv_us: i64,
    ) -> Option<DexEvent> {
        use crate::instr::utils::*;
        use crate::core::events::*;

        // CREATE 指令至少需要 10 个账户（0..9 含 token_program）
        if ix_accounts.len() < 10 {
            return None;
        }

        let get_account = |idx: usize| -> Option<solana_sdk::pubkey::Pubkey> {
            ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
        };

        let mut offset = 8; // 跳过 discriminator

        // 解析 name (string)
        let name = if let Some((s, len)) = read_str_unchecked(data, offset) {
            offset += len;
            s.to_string()
        } else {
            String::new()
        };

        // 解析 symbol (string)
        let symbol = if let Some((s, len)) = read_str_unchecked(data, offset) {
            offset += len;
            s.to_string()
        } else {
            String::new()
        };

        // 解析 uri (string)
        let uri = if let Some((s, len)) = read_str_unchecked(data, offset) {
            offset += len;
            s.to_string()
        } else {
            String::new()
        };

        // 从指令数据中读取 creator（在 name, symbol, uri 之后）
        let creator = if offset + 32 <= data.len() {
            read_pubkey(data, offset).unwrap_or_default()
        } else {
            solana_sdk::pubkey::Pubkey::default()
        };

        // 从账户中读取 mint, bonding_curve, user
        let mint = get_account(0)?;
        let bonding_curve = get_account(2).unwrap_or_default();
        let user = get_account(7).unwrap_or_default();

        let metadata = EventMetadata {
            signature,
            slot,
            tx_index,
            block_time_us: 0, // ShredStream 不提供 block_time
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

    /// 解析 CREATE_V2 指令
    ///
    /// CREATE_V2 指令账户映射 (from IDL):
    /// 0: mint, 1: mint_authority, 2: bonding_curve, 3: associated_bonding_curve,
    /// 4: global, 5: user, 6: system_program, 7: token_program, ...
    #[inline]
    fn parse_create_v2_instruction(
        data: &[u8],
        accounts: &[solana_sdk::pubkey::Pubkey],
        ix_accounts: &[u8],
        signature: solana_sdk::signature::Signature,
        slot: u64,
        tx_index: u64,
        recv_us: i64,
    ) -> Option<DexEvent> {
        use crate::instr::utils::*;
        use crate::core::events::*;

        const CREATE_V2_MIN_ACCOUNTS: usize = 16;
        if ix_accounts.len() < CREATE_V2_MIN_ACCOUNTS {
            return None;
        }

        let get_account = |idx: usize| -> Option<solana_sdk::pubkey::Pubkey> {
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
        let is_cashback_enabled = read_bool(payload, offset).unwrap_or(false);

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

    /// 解析 BUY 指令
    #[inline]
    fn parse_buy_instruction(
        data: &[u8],
        accounts: &[Pubkey],
        ix_accounts: &[u8],
        signature: solana_sdk::signature::Signature,
        slot: u64,
        tx_index: u64,
        recv_us: i64,
        created_mints: &HashSet<Pubkey>,
        mayhem_mints: &HashSet<Pubkey>,
    ) -> Option<DexEvent> {
        use crate::instr::utils::*;
        use crate::core::events::*;

        if ix_accounts.len() < 7 {
            return None;
        }

        let get_account = |idx: usize| -> Option<Pubkey> {
            ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
        };

        // 解析参数: amount (u64), max_sol_cost (u64)
        let (token_amount, sol_amount) = if data.len() >= 16 {
            (read_u64_le(data, 0).unwrap_or(0), read_u64_le(data, 8).unwrap_or(0))
        } else {
            (0, 0)
        };

        let mint = get_account(2)?;
        
        // 🔧 关键修复：只有当 mint 在 created_mints 中时，才标记为 is_created_buy
        let is_created_buy = created_mints.contains(&mint);

        // 🔧 Mayhem Mode 检测：CREATE_V2 指令创建的代币是 Mayhem Mode
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
            token_program: get_token_program_or_default(get_account(8).unwrap_or_default()),
            creator_vault: get_account(9).unwrap_or_default(),
            account: None,
        }))
    }

    /// 解析 SELL 指令
    #[inline]
    fn parse_sell_instruction(
        data: &[u8],
        accounts: &[solana_sdk::pubkey::Pubkey],
        ix_accounts: &[u8],
        signature: solana_sdk::signature::Signature,
        slot: u64,
        tx_index: u64,
        recv_us: i64,
    ) -> Option<DexEvent> {
        use crate::instr::utils::*;
        use crate::core::events::*;

        if ix_accounts.len() < 7 {
            return None;
        }

        let get_account = |idx: usize| -> Option<solana_sdk::pubkey::Pubkey> {
            ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
        };

        // 解析参数: amount (u64), min_sol_output (u64)
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
            token_program: get_token_program_or_default(get_account(9).unwrap_or_default()),
            creator_vault: get_account(8).unwrap_or_default(),
            account: None,
        }))
    }

    /// 解析 BUY_EXACT_SOL_IN 指令
    #[inline]
    fn parse_buy_exact_sol_in_instruction(
        data: &[u8],
        accounts: &[Pubkey],
        ix_accounts: &[u8],
        signature: solana_sdk::signature::Signature,
        slot: u64,
        tx_index: u64,
        recv_us: i64,
        created_mints: &HashSet<Pubkey>,
        mayhem_mints: &HashSet<Pubkey>,
    ) -> Option<DexEvent> {
        use crate::instr::utils::*;
        use crate::core::events::*;

        if ix_accounts.len() < 7 {
            return None;
        }

        let get_account = |idx: usize| -> Option<Pubkey> {
            ix_accounts.get(idx).and_then(|&i| accounts.get(i as usize)).copied()
        };

        // 解析参数: spendable_sol_in (u64), min_tokens_out (u64)
        let (sol_amount, token_amount) = if data.len() >= 16 {
            (read_u64_le(data, 0).unwrap_or(0), read_u64_le(data, 8).unwrap_or(0))
        } else {
            (0, 0)
        };

        let mint = get_account(2)?;
        
        // 🔧 关键修复：只有当 mint 在 created_mints 中时，才标记为 is_created_buy
        let is_created_buy = created_mints.contains(&mint);

        // 🔧 Mayhem Mode 检测：CREATE_V2 指令创建的代币是 Mayhem Mode
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
            token_program: get_token_program_or_default(get_account(8).unwrap_or_default()),
            creator_vault: get_account(9).unwrap_or_default(),
            account: None,
        }))
    }
}
