//! ShredStream：`pfeeUx...`（pump_fees）**外层**指令。
//!
//! 与 [`crate::instr::pump_fees`] 使用相同 IDL 账户索引与 discriminator，**不**编造链上未发出的事件。
//!
//! Shred 热路径**仅**解析并入队 `update_fee_shares`（取 `pump_creator_vault` 等）；其它 `pfee…` 外层指令直接忽略，减轻下游负担。全量 entry 仍由 ShredStream 代理侧决定。

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

use crate::core::events::DexEvent;
use crate::instr::program_ids::PUMP_FEES_PROGRAM_ID;
use crate::instr::pump_fees::UPDATE_FEE_SHARES_IX;

const MAX_STACK_ACCOUNTS: usize = 32;

#[inline(always)]
fn ix_pubkeys_with_cap(
    ix_accounts: &[u8],
    static_keys: &[Pubkey],
    out: &mut [Pubkey; MAX_STACK_ACCOUNTS],
) -> Option<usize> {
    let mut n = 0usize;
    for &idx in ix_accounts {
        let pk = static_keys.get(idx as usize).copied()?;
        if n >= out.len() {
            return None;
        }
        out[n] = pk;
        n += 1;
    }
    Some(n)
}

#[inline]
pub(crate) fn try_push_pump_fees_outer_if_applicable(
    program_id_index: u8,
    data: &[u8],
    ix_accounts: &[u8],
    static_keys: &[Pubkey],
    signature: Signature,
    slot: u64,
    tx_index: u64,
    recv_us: i64,
    events: &mut Vec<DexEvent>,
) {
    let Some(pid) = static_keys.get(program_id_index as usize) else {
        return;
    };
    if *pid != PUMP_FEES_PROGRAM_ID {
        return;
    }
    if data.len() < 8 || data[..8] != UPDATE_FEE_SHARES_IX {
        return;
    }
    let mut stack = [Pubkey::default(); MAX_STACK_ACCOUNTS];
    let Some(n) = ix_pubkeys_with_cap(ix_accounts, static_keys, &mut stack) else {
        return;
    };
    if let Some(ev) = crate::instr::pump_fees::parse_instruction(
        data,
        &stack[..n],
        signature,
        slot,
        tx_index,
        None,
        recv_us,
    ) {
        events.push(ev);
    }
}
