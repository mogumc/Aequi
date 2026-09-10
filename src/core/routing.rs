//! 选路（§6.4，D11）：SWRR + LRS。
//!
//! - SWRR：平滑加权轮询 —— 长期按权重比例分配，且平滑（不连续同一密钥）
//! - LRS：最近最少选择优先 —— "被冷落最久"的密钥优先补位；新密钥 `last_selected_seq = 0` 一入池即被选中
//! - 选路**不看数组位置** —— 插入顺序不再影响命中率（修掉旧版位置偏置）
//!
//! 并发模型（§6.4.3，**有意放弃无锁**）：每上游一把短锁保护 `current_weight`，
//! 持锁纳秒级、无 IO。选路每请求一次，20ns 锁成本可忽略，换来算法精确正确。

use std::collections::HashMap;

/// 候选槽位的最小视图 —— `core` 不关心槽位其它字段（health/enabled 等由调用方过滤）。
pub trait Candidate {
    /// 稳定数字 id（SWRR 平滑权重状态的键）。
    fn stable_id(&self) -> u64;
    fn weight(&self) -> u32;
    /// LRS 主键：最近一次被选中的全局序号（0 = 从未选中）。
    fn last_selected_seq(&self) -> u64;
    /// 公平性审计计数。
    fn served_total(&self) -> u64;
}

/// 单上游 / 单绑定的选择器状态。由调用方持每上游短锁（`parking_lot::Mutex`）保护。
#[derive(Debug, Default)]
pub struct SwrrSelector {
    /// SWRR 平滑权重（按候选 stable_id 索引）。
    current_weight: HashMap<u64, i64>,
    /// 全局选择序号发生器（LRS 主键来源），单调递增。
    seq: u64,
}

impl SwrrSelector {
    #[must_use]
    pub fn new() -> Self {
        Self { current_weight: HashMap::new(), seq: 0 }
    }

    /// 全局选择序号，单调递增。
    #[must_use]
    pub const fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// 核心：在**调用方已完成资格过滤**的候选集合上评分
    /// （enabled && health==Active && 不在冷却 && 在途未满 && 不在请求局部排除集）。
    ///
    /// 评分顺序（§6.4.2）：权重 >1 存在 → SWRR；全为 1 → 纯 LRS（等价严格轮转，对增删自愈）。
    #[must_use]
    pub fn pick<C: Candidate>(&mut self, candidates: &[C]) -> Option<usize> {
        if candidates.is_empty() {
            return None;
        }

        let has_weighted = candidates.iter().any(|c| c.weight() > 1);

        if has_weighted {
            // ① SWRR：全部候选 cw += weight，取最大者，其 cw -= total
            let total: i64 = candidates.iter().map(|c| i64::from(c.weight())).sum();
            let mut best_idx = 0usize;
            let mut best_score = i64::MIN;
            for (i, c) in candidates.iter().enumerate() {
                let cw = self.current_weight.entry(c.stable_id()).or_insert(0);
                *cw += i64::from(c.weight());
                if *cw > best_score {
                    best_score = *cw;
                    best_idx = i;
                }
            }
            if let Some(cw) = self.current_weight.get_mut(&candidates[best_idx].stable_id()) {
                *cw -= total;
            }
            Some(best_idx)
        } else {
            // 纯 LRS：取 (last_selected_seq, served_total) 最小者
            let mut best_idx = 0usize;
            let mut best_key = (u64::MAX, u64::MAX);
            for (i, c) in candidates.iter().enumerate() {
                let k = (c.last_selected_seq(), c.served_total());
                if k < best_key {
                    best_key = k;
                    best_idx = i;
                }
            }
            Some(best_idx)
        }
    }

    /// 候选移除后清理 SWRR 状态（防 HashMap 无界增长）。
    pub fn forget(&mut self, stable_id: u64) {
        self.current_weight.remove(&stable_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug)]
    struct Slot {
        id: u64,
        weight: u32,
        last_seq: u64,
        served: u64,
    }

    impl Candidate for Slot {
        fn stable_id(&self) -> u64 {
            self.id
        }
        fn weight(&self) -> u32 {
            self.weight
        }
        fn last_selected_seq(&self) -> u64 {
            self.last_seq
        }
        fn served_total(&self) -> u64 {
            self.served
        }
    }

    fn slot(id: u64, weight: u32) -> Slot {
        Slot { id, weight, last_seq: 0, served: 0 }
    }

    /// 选择并更新槽位状态（模拟调用方逻辑）。
    fn select(sel: &mut SwrrSelector, slots: &mut [Slot]) -> usize {
        let idx = sel.pick(slots).unwrap();
        slots[idx].last_seq = sel.next_seq();
        slots[idx].served += 1;
        idx
    }

    #[test]
    fn uniform_weights_strictly_fair() {
        // §6.4.4 验收 1：均匀权重 10k 次，max/min served ∈ [1, 1.02]
        let mut slots: Vec<Slot> = (0..8).map(|i| slot(i, 1)).collect();
        let mut sel = SwrrSelector::new();
        for _ in 0..10_000 {
            select(&mut sel, &mut slots);
        }
        let min = slots.iter().map(|s| s.served).min().unwrap();
        let max = slots.iter().map(|s| s.served).max().unwrap();
        assert_eq!(min, 1250, "等权重（纯 LRS）下应严格均分");
        assert_eq!(max, 1250);
        assert!((max as f64 / min as f64) <= 1.02);
    }

    #[test]
    fn weighted_proportional() {
        // §6.4.4 验收 4：权重 1:2:3 → 比例误差 < 2%
        let mut slots = vec![slot(0, 1), slot(1, 2), slot(2, 3)];
        let mut sel = SwrrSelector::new();
        let n = 12_000;
        for _ in 0..n {
            select(&mut sel, &mut slots);
        }
        let expect = [2000.0, 4000.0, 6000.0];
        for (i, e) in expect.iter().enumerate() {
            let actual = slots[i].served as f64;
            let err = (actual - e).abs() / e;
            assert!(err < 0.02, "slot{i}: got {actual}, want {e}");
        }
    }

    #[test]
    fn swrr_is_smooth_no_burst() {
        // SWRR 平滑性：权重 1:4 的经典序列不出现长连击（理论上限 = 高权重/低权重 = 4，
        // 但标准 SWRR 对 1:4 的实际最大连击为 1——即 4,1,4,1,...；允许上界 2 容忍实现变体）
        let mut slots = vec![slot(0, 1), slot(1, 4)];
        let mut sel = SwrrSelector::new();
        let mut run = 0u32;
        let mut last: Option<usize> = None;
        let mut hits = [0u64; 2];
        for _ in 0..1_000 {
            let idx = select(&mut sel, &mut slots);
            hits[idx] += 1;
            match last {
                Some(p) if p == idx => run += 1,
                _ => run = 1,
            }
            last = Some(idx);
            assert!(run <= 2, "SWRR 不应连续命中同一槽超过 2 次");
        }
        // 比例守恒：1:4 → 200:800
        assert_eq!(hits[0], 200);
        assert_eq!(hits[1], 800);
    }

    #[test]
    fn new_key_selected_immediately() {
        // §6.4.4 验收 3：中途新增密钥（last_seq=0）→ 立即被选中
        let mut slots = vec![slot(0, 1), slot(1, 1)];
        let mut sel = SwrrSelector::new();
        for _ in 0..100 {
            select(&mut sel, &mut slots);
        }
        slots.push(slot(99, 1)); // 新密钥
        let idx = select(&mut sel, &mut slots);
        assert_eq!(slots[idx].id, 99, "LRS 应优先补位新密钥");
    }

    #[test]
    fn no_position_bias() {
        // 旧版缺陷回归：插入顺序不得影响命中分布
        let mut a: Vec<Slot> = (0..5).map(|i| slot(i, 1)).collect();
        let mut b: Vec<Slot> = (0..5).rev().map(|i| slot(i + 10, 1)).collect();
        let mut sel = SwrrSelector::new();
        for _ in 0..5_000 {
            select(&mut sel, &mut a);
            select(&mut sel, &mut b);
        }
        let spread_a = a.iter().map(|s| s.served).max().unwrap() - a.iter().map(|s| s.served).min().unwrap();
        let spread_b = b.iter().map(|s| s.served).max().unwrap() - b.iter().map(|s| s.served).min().unwrap();
        assert_eq!(spread_a, 0);
        assert_eq!(spread_b, 0);
    }

    #[test]
    fn retry_exclusion_is_caller_side() {
        // 重试排除是请求局部的过滤 —— 由调用方先过滤再传入；本函数不做全局副作用。
        let slots = vec![slot(0, 1), slot(1, 1)];
        let mut sel = SwrrSelector::new();
        let filtered: Vec<Slot> = slots.iter().filter(|s| s.id != 0).copied().collect();
        let idx = sel.pick(&filtered).unwrap();
        assert_eq!(filtered[idx].id, 1);
    }

    #[test]
    fn empty_candidates() {
        let mut sel = SwrrSelector::new();
        assert!(sel.pick::<Slot>(&[]).is_none());
    }

    #[test]
    fn forget_cleans_state() {
        let mut sel = SwrrSelector::new();
        let slots = vec![slot(7, 5)];
        let _ = sel.pick(&slots);
        sel.forget(7);
        // 清理后重新从零开始
        let idx = sel.pick(&slots).unwrap();
        assert_eq!(idx, 0);
    }
}
