//! 身份类型（§2.2① / §八 第 2 条）。
//!
//! - `UpstreamId / BindingId / GroupId / AccessKeyId / RequestId` = Uuid7（时间有序）
//! - `UpstreamKeyId` = `blake3(secret)[..16]` 指纹
//! - `name` 仅展示，不得作主键 / 树名 / URI

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// 生成新的 Uuid7 身份。
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            #[must_use]
            pub const fn from_uuid(u: Uuid) -> Self {
                Self(u)
            }

            #[must_use]
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }

            /// 解析字符串形式；失败返回 `None`（无 IO，纯函数）。
            #[must_use]
            pub fn parse(s: &str) -> Option<Self> {
                Uuid::parse_str(s).ok().map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // 连字符完整形式，管理端与日志统一
                write!(f, "{}", self.0.hyphenated())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0.hyphenated())
            }
        }
    };
}

uuid_id!(
    /// 上游身份（Uuid7）。重命名不影响关联数据。
    UpstreamId
);
uuid_id!(
    /// 模型绑定身份（Uuid7）。
    BindingId
);
uuid_id!(
    /// 访问密钥身份（Uuid7，系统生成）。
    AccessKeyId
);
uuid_id!(
    /// 请求关联 id（Uuid7，时间有序，可作时序键）。
    RequestId
);

/// 上游密钥身份 = `blake3(secret)[..16]`（32 hex chars）。
/// 同一密钥在任何上游得到同一 id —— 允许跨上游指纹去重，但授权范围仍按 `upstream_id` 圈定。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UpstreamKeyId([u8; 16]);

impl UpstreamKeyId {
    /// 由密钥明文派生指纹。
    #[must_use]
    pub fn from_secret(secret: &[u8]) -> Self {
        let h = blake3::hash(secret);
        let mut id = [0u8; 16];
        id.copy_from_slice(&h.as_bytes()[..16]);
        Self(id)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// 32 位 hex，用于展示、事件与指标标签（非明文）。
    #[must_use]
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[must_use]
    pub fn parse_hex(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() != 32 {
            return None;
        }
        let mut id = [0u8; 16];
        for (i, chunk) in b.chunks(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            id[i] = ((hi << 4) | lo) as u8;
        }
        Some(Self(id))
    }
}

impl fmt::Display for UpstreamKeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.hex())
    }
}

impl fmt::Debug for UpstreamKeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UpstreamKeyId({})", self.hex())
    }
}

impl Serialize for UpstreamKeyId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for UpstreamKeyId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse_hex(&s).ok_or_else(|| serde::de::Error::custom("invalid ukey id hex"))
    }
}

/// 上游密钥健康态（全项目唯一定义，§6.1.1）。
///
/// 恢复方式各不相同，**禁止**把人工暂停（`enabled`）混进本枚举：
/// - `Active`：参与选路
/// - `Cooling`：§6.2 退避中，到期自动恢复
/// - `SuspendedAuth`：凭证失效，仅探针 200 恢复
/// - `QuotaExhausted`：额度耗尽，仅管理员 reset/加额恢复（探针无效）
/// - `Unknown`：连续探测无结论的标记态
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[repr(u8)]
pub enum UpstreamHealth {
    Active = 0,
    Cooling = 1,
    SuspendedAuth = 2,
    QuotaExhausted = 3,
    Unknown = 4,
}

impl UpstreamHealth {
    /// 是否参与选路（`Unknown` 由调用方按 `probe.unverified_usable` 配置决定，默认可参与）。
    #[must_use]
    pub const fn selectable(&self) -> bool {
        matches!(self, Self::Active | Self::Unknown)
    }

    #[must_use]
    pub const fn as_u8(&self) -> u8 {
        *self as u8
    }

    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Active),
            1 => Some(Self::Cooling),
            2 => Some(Self::SuspendedAuth),
            3 => Some(Self::QuotaExhausted),
            4 => Some(Self::Unknown),
            _ => None,
        }
    }
}

impl fmt::Display for UpstreamHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Active => "Active",
            Self::Cooling => "Cooling",
            Self::SuspendedAuth => "SuspendedAuth",
            Self::QuotaExhausted => "QuotaExhausted",
            Self::Unknown => "Unknown",
        };
        f.write_str(s)
    }
}

/// 探针五态判定（§6.1.1）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[repr(u8)]
pub enum ProbeOutcome {
    Unverified = 0,
    Valid = 1,
    Invalid = 2,
    RateLimited = 3,
    Inconclusive = 4,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid7_ids_are_time_ordered() {
        let a = UpstreamId::generate();
        let b = UpstreamId::generate();
        assert!(a < b, "Uuid7 必须时间有序");
    }

    #[test]
    fn key_id_is_stable_fingerprint() {
        let a = UpstreamKeyId::from_secret(b"sk-abc");
        let b = UpstreamKeyId::from_secret(b"sk-abc");
        let c = UpstreamKeyId::from_secret(b"sk-xyz");
        assert_eq!(a, b);
        assert_ne!(a, c);
        // hex round-trip
        assert_eq!(UpstreamKeyId::parse_hex(&a.hex()), Some(a));
        assert_eq!(a.hex().len(), 32);
    }

    #[test]
    fn health_selectability() {
        assert!(UpstreamHealth::Active.selectable());
        assert!(UpstreamHealth::Unknown.selectable());
        assert!(!UpstreamHealth::Cooling.selectable());
        assert!(!UpstreamHealth::SuspendedAuth.selectable());
        assert!(!UpstreamHealth::QuotaExhausted.selectable());
        // u8 round-trip
        for v in 0..5u8 {
            let h = UpstreamHealth::from_u8(v).unwrap();
            assert_eq!(h.as_u8(), v);
        }
        assert!(UpstreamHealth::from_u8(9).is_none());
    }
}
