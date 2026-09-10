//! 模型绑定（§4.2.1，D8 核心）：`display_model × upstream_id × upstream_model` 单一真源。
//!
//! 取代旧版 `Upstream.models` / `model_map` / `model_rmap` / `ModelRoutesFile` 三真源双写。

use crate::core::id::{BindingId, UpstreamId};
use crate::core::rating::Rate;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelBinding {
    pub id: BindingId,
    /// 对外暴露的单一模型 id（可重命名 / 加别名）。
    pub display_model: String,
    /// ★ 必带上游身份 —— 同一 display_model 在不同上游是不同绑定，授权按绑定判定。
    pub upstream: UpstreamId,
    /// 上游实际模型名（请求改写依据）。
    pub upstream_model: String,
    pub enabled: bool,
    /// `None` → 用模型级统一倍率（RateTable）。
    pub rate_override: Option<Rate>,
}

impl ModelBinding {
    #[must_use]
    pub fn new(display_model: impl Into<String>, upstream: UpstreamId, upstream_model: impl Into<String>) -> Self {
        Self {
            id: BindingId::generate(),
            display_model: display_model.into(),
            upstream,
            upstream_model: upstream_model.into(),
            enabled: true,
            rate_override: None,
        }
    }
}

/// 内存派生索引（§4.2.1）：由绑定集合单向派生，不落库、不双写。
#[derive(Debug, Default, Clone)]
pub struct BindingIndex {
    by_display: std::collections::HashMap<String, smallvec::SmallVec<[BindingId; 2]>>,
    by_upstream: std::collections::HashMap<UpstreamId, Vec<BindingId>>,
}

impl BindingIndex {
    /// 从绑定列表重建派生索引（只在管理端变更后调用，非热路径）。
    #[must_use]
    pub fn derive<'a>(bindings: impl IntoIterator<Item = &'a ModelBinding>) -> Self {
        let mut idx = Self::default();
        for b in bindings {
            if !b.enabled {
                continue;
            }
            idx.by_display
                .entry(b.display_model.clone())
                .or_default()
                .push(b.id);
            idx.by_upstream.entry(b.upstream).or_default().push(b.id);
        }
        idx
    }

    /// 按 display_model 取候选绑定（保持插入序，供分组过滤 + SWRR）。
    #[must_use]
    pub fn candidates(&self, display_model: &str) -> &[BindingId] {
        match self.by_display.get(display_model) {
            Some(v) => v,
            None => &[],
        }
    }

    #[must_use]
    pub fn bindings_of_upstream(&self, upstream: &UpstreamId) -> &[BindingId] {
        match self.by_upstream.get(upstream) {
            Some(v) => v,
            None => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(display: &str, upstream: &str, upstream_model: &str) -> ModelBinding {
        // 测试内固定 id：通过 parse 生成
        let u = UpstreamId::parse(upstream).unwrap_or_else(|| {
            let _ = upstream;
            UpstreamId::generate()
        });
        let mut b = ModelBinding::new(display, u, upstream_model);
        b.id = BindingId::parse(upstream_model).unwrap_or_else(|| {
            let _ = upstream_model;
            BindingId::generate()
        });
        b
    }

    #[test]
    fn index_derives_one_way() {
        let u1 = UpstreamId::generate();
        let u2 = UpstreamId::generate();
        let mut b1 = ModelBinding::new("gpt-4o", u1, "gpt-4o-2024");
        b1.enabled = true;
        let mut b2 = ModelBinding::new("gpt-4o", u2, "openai/gpt-4o");
        b2.enabled = true;
        let mut disabled = ModelBinding::new("o1", u1, "o1-preview");
        disabled.enabled = false;

        let idx = BindingIndex::derive([&b1, &b2, &disabled]);
        // 同一 display_model 两个上游 → 2 候选；顺序 = 输入顺序
        assert_eq!(idx.candidates("gpt-4o").len(), 2);
        // 禁用绑定不进索引
        assert_eq!(idx.candidates("o1").len(), 0);
        // 反查：u1 只有 b1（disabled 不计）
        assert_eq!(idx.bindings_of_upstream(&u1).len(), 1);
        assert_eq!(idx.bindings_of_upstream(&u2).len(), 1);
    }

    #[test]
    fn bindings_are_distinct_per_upstream() {
        // D8 的意义：同 display_model 在不同上游是不同绑定（不同 id）
        let a = make("gpt-4o", "01900000-0000-7000-8000-000000000001", "m-a");
        let b = make("gpt-4o", "01900000-0000-7000-8000-000000000002", "m-b");
        assert_ne!(a.upstream, b.upstream);
        assert_ne!(a.id, b.id);
    }
}
