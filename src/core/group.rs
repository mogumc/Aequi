//! 分组与授权（§6.3，D8）。
//!
//! 分组**只决定可访问的模型绑定集合**，不携带倍率（倍率在 RateTable 按模型统一配置）。
//! 授权判定全项目**唯一函数**为 [`allowed`]（§八 第 6 条）。

use crate::core::binding::ModelBinding;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

/// 组 id：语义字符串（"default" / "pro" / "internal" / "__admin__"）。
/// 不是 Uuid —— 组是运维定义的少量枚举式实体，可读性优先；但仍不可作为 URI 段。
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupId(pub Box<str>);

impl GroupId {
    pub const ADMIN: &'static str = "__admin__";
    pub const DEFAULT: &'static str = "default";

    #[must_use]
    pub fn new(s: impl Into<Box<str>>) -> Self {
        Self(s.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GroupId({})", self.0)
    }
}

/// 组策略：绑定集合的三种表达（§6.3.1）。`Except` 覆盖面随目录增长自动收紧。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "ids", rename_all = "snake_case")]
pub enum Policy {
    #[default]
    All,
    Only(BTreeSet<String>),
    Except(BTreeSet<String>),
}

impl Policy {
    /// 纯判定：该绑定 id 是否被本策略允许。
    #[must_use]
    pub fn permits(&self, binding_id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(set) => set.contains(binding_id),
            Self::Except(set) => !set.contains(binding_id),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Group {
    pub id: GroupId,
    pub name: String,
    pub description: Option<String>,
    /// ★ 只决定可访问哪些「模型绑定」（以 BindingId 表达）。
    pub bindings: Policy,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl Group {
    #[must_use]
    pub fn new(id: GroupId, name: impl Into<String>) -> Self {
        let now = crate::core::config::now_ms();
        Self {
            id,
            name: name.into(),
            description: None,
            bindings: Policy::All,
            created_at_ms: now,
            updated_at_ms: now,
        }
    }
}

/// 访问密钥的分组归属（§6.3.1）。`deny` 优先于 `allow`。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessKeyGroups {
    pub allow: BTreeSet<GroupId>,
    pub deny: BTreeSet<GroupId>,
}

/// ★★ 授权判定全项目唯一函数（§6.3.2 / §八 第 6 条）。
///
/// 被代理绑定过滤、`/v1/models` 列举、admin 预览共用；任何其它位置
/// 出现"该密钥能否用该绑定"的判断即为违反硬约束。
///
/// 判定顺序：`__admin__` 全通 → `deny` 优先 → `allow` 存在任一许可组。
#[must_use]
pub fn allowed(binding: &ModelBinding, groups: &AccessKeyGroups, group_lookup: &dyn Fn(&GroupId) -> Option<Group>) -> bool {
    // 管理组全通
    if groups.allow.iter().any(|g| g.as_str() == GroupId::ADMIN) {
        return true;
    }
    // deny 优先
    for g in &groups.deny {
        if let Some(grp) = group_lookup(g) {
            if grp.bindings.permits(binding.id.as_uuid().to_string().as_str()) {
                return false;
            }
        }
    }
    // allow：存在任一允许组即放行
    for g in &groups.allow {
        if let Some(grp) = group_lookup(g) {
            if grp.bindings.permits(binding.id.as_uuid().to_string().as_str()) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::binding::ModelBinding;
    use crate::core::id::{BindingId, UpstreamId};
    use std::collections::HashMap;

    fn binding(id: &str) -> ModelBinding {
        let mut b = ModelBinding::new("m", UpstreamId::generate(), "m");
        b.id = BindingId::parse(id).unwrap_or_else(|| BindingId::generate());
        // 用固定 uuid 便于策略匹配
        b.id = BindingId::from_uuid(uuid::Uuid::parse_str(id).unwrap());
        b
    }

    fn lookup(groups: &[Group]) -> impl Fn(&GroupId) -> Option<Group> + '_ {
        let map: HashMap<GroupId, Group> = groups.iter().map(|g| (g.id.clone(), g.clone())).collect();
        move |id: &GroupId| map.get(id).cloned()
    }

    fn group(id: &str, policy: Policy) -> Group {
        let mut g = Group::new(GroupId::new(id), id);
        g.bindings = policy;
        g
    }

    const B1: &str = "01900000-0000-7000-8000-000000000001";
    const B2: &str = "01900000-0000-7000-8000-000000000002";

    #[test]
    fn admin_group_allows_everything() {
        let b = binding(B1);
        let mut g = AccessKeyGroups::default();
        g.allow.insert(GroupId::new(GroupId::ADMIN));
        assert!(allowed(&b, &g, &|_| None));
    }

    #[test]
    fn only_policy() {
        let groups = [group("pro", Policy::Only(BTreeSet::from([B1.to_string()])))];
        let lk = lookup(&groups);
        let mut akg = AccessKeyGroups::default();
        akg.allow.insert(GroupId::new("pro"));

        assert!(allowed(&binding(B1), &akg, &lk));
        assert!(!allowed(&binding(B2), &akg, &lk));
    }

    #[test]
    fn deny_overrides_allow() {
        let groups = [
            group("base", Policy::All),
            group("blocked", Policy::Only(BTreeSet::from([B1.to_string()]))),
        ];
        let lk = lookup(&groups);
        let mut akg = AccessKeyGroups::default();
        akg.allow.insert(GroupId::new("base"));
        akg.deny.insert(GroupId::new("blocked"));

        assert!(!allowed(&binding(B1), &akg, &lk), "deny 必须优先于 allow");
        assert!(allowed(&binding(B2), &akg, &lk));
    }

    #[test]
    fn multiple_allow_groups_union() {
        // 集合语义：一个密钥可同时属于互不包含的多个组（旧版标量 level 无法表达）
        let groups = [
            group("a", Policy::Only(BTreeSet::from([B1.to_string()]))),
            group("b", Policy::Only(BTreeSet::from([B2.to_string()]))),
        ];
        let lk = lookup(&groups);
        let mut akg = AccessKeyGroups::default();
        akg.allow.insert(GroupId::new("a"));
        akg.allow.insert(GroupId::new("b"));

        assert!(allowed(&binding(B1), &akg, &lk));
        assert!(allowed(&binding(B2), &akg, &lk));
    }

    #[test]
    fn level_group_equivalence() {
        // §6.3.4：L >= M ⇔ {0..L} ∩ {M..max} ≠ ∅ —— 用组表达验证等价性
        let max = 3i64;
        let key_groups: BTreeSet<String> = (0..=2).map(|i| format!("lvl_{i}")).collect();

        // 上游 min_key_level=1 → 其绑定归组 Only({lvl_1..lvl_3})
        let binding_policy_ids: BTreeSet<String> = (1..=max).map(|i| format!("lvl_{i}")).collect();
        let g = Policy::Only(binding_policy_ids);

        // 密钥 level=2 → allow = {lvl_0..lvl_2}
        let mut akg = AccessKeyGroups::default();
        akg.allow = key_groups.iter().cloned().map(GroupId::new).collect();

        // 等价性判定：key 的 allow 组集合与绑定组的交集非空
        let intersects = match &g {
            Policy::Only(set) => set.iter().any(|x| key_groups.contains(x)),
            _ => false,
        };
        assert!(intersects, "L=2 >= M=1 → 允许");

        // 反例：key level=0（allow={lvl_0}）与绑定组 {lvl_1..} 无交集 → 拒绝
        let low_key: BTreeSet<String> = (0..=0).map(|i| format!("lvl_{i}")).collect();
        let no_intersect = match &g {
            Policy::Only(set) => set.iter().any(|x| low_key.contains(x)),
            _ => false,
        };
        assert!(!no_intersect, "L=0 < M=1 → 拒绝");

        let mut b = ModelBinding::new("m", UpstreamId::generate(), "m");
        b.id = BindingId::from_uuid(uuid::Uuid::now_v7());
        let _ = (b, akg);
    }

    #[test]
    fn except_policy() {
        let groups = [group("pub", Policy::Except(BTreeSet::from([B2.to_string()])))];
        let lk = lookup(&groups);
        let mut akg = AccessKeyGroups::default();
        akg.allow.insert(GroupId::new("pub"));

        assert!(allowed(&binding(B1), &akg, &lk));
        assert!(!allowed(&binding(B2), &akg, &lk));
    }
}
