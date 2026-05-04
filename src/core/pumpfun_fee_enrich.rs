//! 同笔交易内 Pump 事件后处理（**零 RPC**，供 gRPC/Shred 热路径）：
//! - CreateV2 + 后续 Buy 分离时，将 Buy 的 fee recipient 回填到 `observed_fee_recipient`
//! - 将 **Create / CreateV2 指令**里的 `is_cashback_enabled`、`is_mayhem_mode` 合并进同一 mint 的 Trade 事件，
//!   避免仅有外层指令、TradeEvent 日志缺字段时 `sol-trade-sdk` 误判返现与 Mayhem fee 池

use std::collections::HashMap;

use solana_sdk::pubkey::Pubkey;

use crate::core::events::{DexEvent, PumpFunTradeEvent};

fn pumpfun_buy_like_mint_fee(e: &DexEvent) -> Option<(Pubkey, Pubkey)> {
    match e {
        DexEvent::PumpFunTrade(t) if t.is_buy && t.mint != Pubkey::default() => {
            Some((t.mint, t.fee_recipient))
        }
        DexEvent::PumpFunBuy(t) if t.mint != Pubkey::default() => Some((t.mint, t.fee_recipient)),
        DexEvent::PumpFunBuyExactSolIn(t) if t.mint != Pubkey::default() => {
            Some((t.mint, t.fee_recipient))
        }
        _ => None,
    }
}

/// 扫描同签名下的买入类事件，按 mint 记录 `fee_recipient`（ShredStream 外层的 buy 已从 accounts[1] 解析）。
pub fn enrich_create_v2_observed_fee_recipient(events: &mut [DexEvent]) {
    let mut mint_to_fee: HashMap<Pubkey, Pubkey> = HashMap::new();
    for e in events.iter() {
        if let Some((mint, fee)) = pumpfun_buy_like_mint_fee(e) {
            if fee != Pubkey::default() {
                mint_to_fee.entry(mint).or_insert(fee);
            }
        }
    }
    if mint_to_fee.is_empty() {
        return;
    }
    for e in events.iter_mut() {
        if let DexEvent::PumpFunCreateV2(c) = e {
            if c.observed_fee_recipient == Pubkey::default() {
                if let Some(&f) = mint_to_fee.get(&c.mint) {
                    c.observed_fee_recipient = f;
                }
            }
        }
    }
}

#[inline]
fn collect_create_cashback_and_mayhem(events: &[DexEvent]) -> HashMap<Pubkey, (bool, bool)> {
    let mut m: HashMap<Pubkey, (bool, bool)> = HashMap::new();
    for e in events {
        match e {
            DexEvent::PumpFunCreateV2(c) if c.mint != Pubkey::default() => {
                m.entry(c.mint).or_insert((c.is_cashback_enabled, c.is_mayhem_mode));
            }
            DexEvent::PumpFunCreate(c) if c.mint != Pubkey::default() => {
                m.entry(c.mint).or_insert((c.is_cashback_enabled, c.is_mayhem_mode));
            }
            _ => {}
        }
    }
    m
}

#[inline]
fn trade_event_mut(e: &mut DexEvent) -> Option<&mut PumpFunTradeEvent> {
    match e {
        DexEvent::PumpFunTrade(t)
        | DexEvent::PumpFunBuy(t)
        | DexEvent::PumpFunSell(t)
        | DexEvent::PumpFunBuyExactSolIn(t) => Some(t),
        _ => None,
    }
}

/// 将同笔交易中 Create / CreateV2 的 `is_cashback_enabled`、`is_mayhem_mode` 并入 Trade 事件（与官方 `create_v2` / bonding 语义一致）。
pub fn enrich_pumpfun_trades_from_create_instructions(events: &mut [DexEvent]) {
    let flags = collect_create_cashback_and_mayhem(events);
    if flags.is_empty() {
        return;
    }
    for e in events.iter_mut() {
        if let Some(t) = trade_event_mut(e) {
            if t.mint == Pubkey::default() {
                continue;
            }
            let Some(&(cb_en, mayhem_create)) = flags.get(&t.mint) else {
                continue;
            };
            t.is_cashback_coin |= cb_en;
            t.mayhem_mode |= mayhem_create;
            if cb_en {
                t.track_volume = true;
            }
        }
    }
}

/// 合并调用：fee 回填 + Create→Trade 标志（gRPC / Shred 在 `merge` 之后调用一次即可）。
pub fn enrich_pumpfun_same_tx_post_merge(events: &mut [DexEvent]) {
    enrich_create_v2_observed_fee_recipient(events);
    enrich_pumpfun_trades_from_create_instructions(events);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{EventMetadata, PumpFunCreateV2TokenEvent};
    use solana_sdk::signature::Signature;

    #[test]
    fn enrich_fills_create_v2_from_same_tx_buy() {
        let sig = Signature::default();
        let mint = Pubkey::new_unique();
        let fee = Pubkey::new_unique();
        let meta = EventMetadata {
            signature: sig,
            slot: 1,
            tx_index: 0,
            block_time_us: 0,
            grpc_recv_us: 0,
            recent_blockhash: None,
        };
        let mut events: Vec<DexEvent> = vec![
            DexEvent::PumpFunCreateV2(PumpFunCreateV2TokenEvent {
                metadata: meta.clone(),
                mint,
                ..Default::default()
            }),
            DexEvent::PumpFunTrade(PumpFunTradeEvent {
                metadata: meta,
                mint,
                fee_recipient: fee,
                is_buy: true,
                ..Default::default()
            }),
        ];
        enrich_create_v2_observed_fee_recipient(&mut events);
        if let DexEvent::PumpFunCreateV2(c) = &events[0] {
            assert_eq!(c.observed_fee_recipient, fee);
        } else {
            panic!("expected CreateV2");
        }
    }

    #[test]
    fn enrich_merges_cashback_and_mayhem_from_create_v2_to_trade() {
        let sig = Signature::default();
        let mint = Pubkey::new_unique();
        let meta = EventMetadata {
            signature: sig,
            slot: 1,
            tx_index: 0,
            block_time_us: 0,
            grpc_recv_us: 0,
            recent_blockhash: None,
        };
        let mut events: Vec<DexEvent> = vec![
            DexEvent::PumpFunCreateV2(PumpFunCreateV2TokenEvent {
                metadata: meta.clone(),
                mint,
                is_mayhem_mode: true,
                is_cashback_enabled: true,
                ..Default::default()
            }),
            DexEvent::PumpFunTrade(PumpFunTradeEvent {
                metadata: meta,
                mint,
                mayhem_mode: false,
                is_cashback_coin: false,
                track_volume: false,
                ..Default::default()
            }),
        ];
        enrich_pumpfun_same_tx_post_merge(&mut events);
        if let DexEvent::PumpFunTrade(t) = &events[1] {
            assert!(t.mayhem_mode);
            assert!(t.is_cashback_coin);
            assert!(t.track_volume);
        } else {
            panic!("expected trade");
        }
    }
}
