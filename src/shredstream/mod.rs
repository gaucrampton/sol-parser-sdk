//! ShredStream 模块 - Jito ShredStream 超低延迟交易订阅
//!
//! 提供从 Jito ShredStream 直接订阅 Solana Entry 数据的能力，
//! 相比 gRPC 订阅具有更低的延迟（约 50-100ms 优势）。
//!
//! ## 使用示例
//! ```rust,no_run
//! use sol_parser_sdk::shredstream::{ShredStreamClient, ShredStreamConfig};
//! use sol_parser_sdk::DexEvent;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = ShredStreamClient::new("http://localhost:10800").await?;
//!
//!     // 订阅并获取事件队列
//!     let queue = client.subscribe().await?;
//!
//!     // 消费事件
//!     loop {
//!         if let Some(event) = queue.pop() {
//!             println!("Received: {:?}", event);
//!         } else {
//!             std::hint::spin_loop();
//!         }
//!     }
//! }
//! ```
//!
//! ## 限制说明
//! ShredStream 相比 gRPC 订阅有以下限制：
//! - 仅 `static_account_keys()`：V0 交易若带 **地址查找表（ALT）**，超出静态表的账户索引无法解析，对应腿可能解析失败；无 ALT 时静态表即全表。
//! - 不解析 **inner instructions**：多笔买必须体现在 **多条外层** Pump `buy` / `buy_exact_sol_in` 上（铸币捆绑通常如此）；若全塞进单条 outer 的 CPI 链则 ShredStream 无法逐腿还原。
//! - 无 block_time，恒为 0
//! - tx_index 是 entry 内索引而非 slot 内索引

pub mod client;
pub mod config;
pub mod proto;

pub use client::ShredStreamClient;
pub use config::ShredStreamConfig;
