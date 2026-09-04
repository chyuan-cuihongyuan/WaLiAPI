//! 多 Key 加权随机选择（FIX-10 的单一实现）。
//!
//! 此前 `core/proxy.rs`（legacy 轨）与 `endpoint_executor/driver.rs`（主轨）
//! 各持有一份复制的实现，且边界比较符写错为 `pick <= 0`：随机数取自半开
//! 区间 `[0, total)`，减去最后一个权重后恰为 0 时 `<= 0` 仍然命中——等权
//! 两个 Key 时随机数 0 与 1 都落在第一个 Key 上，第二个 Key 永远轮不到
//! （上游 issue #34 「总是用左边那个的额度」的根因）。权重选择逻辑从两份
//! 漂移的复制收敛为此处一份，与渠道级加权排序（route_plan）的 `point < 0`
//! 正确写法一致。

use rand::Rng;

/// 从加权池中按权重随机选一个 Key。`pool` 元素为 `(key, weight)`，weight
/// 由调用方 clamp 到 ≥1；空池返回 `None`。
pub fn pick_weighted_key(pool: &[(String, i64)]) -> Option<String> {
    let total: i64 = pool.iter().map(|(_, w)| *w).sum();
    if pool.is_empty() || total <= 0 {
        return None;
    }
    // 半开区间 [0, total)：累计权重减到 pick < 0 的首个 Key 胜出——
    // 每个权重 w 恰好覆盖 w 个随机值，等权两 Key 各 50%。
    let mut pick = rand::rng().random_range(0..total);
    for (key, w) in pool {
        pick -= w;
        if pick < 0 {
            return Some(key.clone());
        }
    }
    // 浮点以上为整数运算，恒有 pick < 0 在池内命中；兜底首项防未来改动。
    pool.first().map(|(k, _)| k.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn pool(entries: &[(&str, i64)]) -> Vec<(String, i64)> {
        entries
            .iter()
            .map(|(k, w)| (k.to_string(), *w))
            .collect()
    }

    /// 等权两 Key 的分布回归（#34 根因）：修复前第二个 Key 命中率为 0。
    #[test]
    fn equal_weights_are_evenly_distributed() {
        let p = pool(&[("a", 1), ("b", 1)]);
        let mut counts: HashMap<String, usize> = HashMap::new();
        for _ in 0..4000 {
            *counts.entry(pick_weighted_key(&p).unwrap()).or_default() += 1;
        }
        let a = counts["a"];
        let b = counts["b"];
        assert_eq!(a + b, 4000);
        // 各约 50%（3σ 容差）；修复前 b 恒为 0。
        assert!((1900..=2100).contains(&a), "a={a}");
        assert!((1900..=2100).contains(&b), "b={b}");
    }

    /// 权重 3:1 的分布按比例倾斜。
    #[test]
    fn weighted_distribution_follows_weights() {
        let p = pool(&[("heavy", 3), ("light", 1)]);
        let mut heavy = 0usize;
        for _ in 0..4000 {
            if pick_weighted_key(&p).unwrap() == "heavy" {
                heavy += 1;
            }
        }
        // 期望 75%（3σ 容差 ±2.1%）。
        assert!((2870..=3130).contains(&heavy), "heavy={heavy}/4000");
    }

    /// 边界：空池 / 全零权重不 panic、不选中。
    #[test]
    fn degenerate_pools_return_none() {
        assert!(pick_weighted_key(&[]).is_none());
        assert!(pick_weighted_key(&pool(&[("a", 0)])).is_none());
    }

    /// 单 Key 池无论权重多少恒选中该 Key。
    #[test]
    fn single_key_always_chosen() {
        let p = pool(&[("only", 7)]);
        for _ in 0..100 {
            assert_eq!(pick_weighted_key(&p).unwrap(), "only");
        }
    }
}
