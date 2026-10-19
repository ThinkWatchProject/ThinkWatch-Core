//! 设备记录：`devices/<设备标识前 8 位>`，每台设备只写自己那一份。
//!
//! 加密同步时它也加密（给版本密钥的公钥）：设备名称常常就是人名（「张三的 MacBook Air」）。

use serde::{Deserialize, Serialize};

use crate::keyring::Keyring;

/// 设备记录多久没更新就从同步位置上移除
pub const STALE_AFTER_MS: u64 = 180 * 24 * 3600 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// 完整的设备标识
    pub id: String,
    pub name: String,
    pub core_version: String,
    /// 最后一次同步成功的时间
    pub last_seen_ms: u64,
    /// 同步位置的最新版本这台设备读不了（由较新的 Core 写的），暂停着等更新
    #[serde(default)]
    pub paused_update: bool,
    /// 它的 core 开着远程控制端口（远程 core）
    #[serde(default)]
    pub remote_core: bool,
}

impl DeviceRecord {
    /// 写进同步位置的样子：加密同步时加密给版本密钥
    pub fn seal(&self, keyring: Option<&Keyring>) -> Vec<u8> {
        let json = serde_json::to_vec(self).expect("plain data serializes");
        match keyring {
            Some(k) => k.encrypt(&json),
            None => json,
        }
    }

    /// 读回来。解不开、读不懂的是 `None`（别的设备刚改了加密设置、还没传完、被人改坏了）
    pub fn open(bytes: &[u8], keyring: Option<&Keyring>) -> Option<Self> {
        let json = if crate::keyring::is_age(bytes) {
            keyring?.decrypt(bytes)?
        } else {
            bytes.to_vec()
        };
        serde_json::from_slice(&json).ok()
    }

    pub fn is_stale(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_seen_ms) > STALE_AFTER_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> DeviceRecord {
        DeviceRecord {
            id: "3f9a2c1e-0b1d-4e2f-9a3b-5c6d7e8f9a0b".into(),
            name: "张三的 MacBook Air".into(),
            core_version: "0.77.0".into(),
            last_seen_ms: 1_000,
            paused_update: false,
            remote_core: false,
        }
    }

    #[test]
    fn a_record_reads_back_plain_or_sealed() {
        let r = rec();
        assert_eq!(DeviceRecord::open(&r.seal(None), None), Some(r.clone()));
        let k = Keyring::generate();
        let sealed = r.seal(Some(&k));
        assert!(
            !String::from_utf8_lossy(&sealed).contains("张三"),
            "加密时名字不能是明文"
        );
        assert_eq!(DeviceRecord::open(&sealed, Some(&k)), Some(r.clone()));
        assert_eq!(DeviceRecord::open(&sealed, None), None);
        assert_eq!(
            DeviceRecord::open(&sealed, Some(&Keyring::generate())),
            None
        );
        assert_eq!(DeviceRecord::open(b"{not json", None), None);
    }

    #[test]
    fn a_record_goes_stale_after_half_a_year() {
        let r = rec();
        assert!(!r.is_stale(r.last_seen_ms + STALE_AFTER_MS));
        assert!(r.is_stale(r.last_seen_ms + STALE_AFTER_MS + 1));
    }
}
