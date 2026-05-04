//! ShredStream 客户端

use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use futures::StreamExt;
use solana_entry::entry::Entry as SolanaEntry;
use solana_sdk::message::VersionedMessage;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::core::now_micros;
use crate::shredstream::config::ShredStreamConfig;
use crate::shredstream::proto::{Entry, ShredstreamProxyClient, SubscribeEntriesRequest};
use crate::DexEvent;

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
        // 热路径：`static_account_keys` 零拷贝、`pump_ix` 内不克隆 CompiledInstruction。
        let mut events = Vec::new();
        super::pump_ix::parse_transaction_pump_events(
            transaction,
            signature,
            slot,
            tx_index,
            recv_us,
            &mut events,
        );
        crate::core::pumpfun_fee_enrich::enrich_pumpfun_same_tx_post_merge(&mut events);

        for mut event in events {
            if let Some(meta) = event.metadata_mut() {
                meta.grpc_recv_us = recv_us;
            }
            let _ = queue.push(event);
        }
    }
}
