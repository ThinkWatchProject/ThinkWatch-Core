//! 每个同步版本自己说的那几件事（配置包 manifest 里的 `sync`）：第几版、谁写的、依据的
//! 上一版本、还没处理的冲突。
//!
//! **冲突记录随版本走**：所有设备看到的是同一份冲突，任一设备处理之后，处理结果作为新版本
//! 上传，冲突从记录里去掉，别的设备随之清除，不重复处理。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tw_pack::model::ItemId;

/// `manifest.sync`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub seq: u64,
    /// 写它的设备，完整的标识
    pub device: String,
    pub device_name: String,
    /// 依据的上一版本（版本标识，见 [`crate::layout::VersionName::id`]）。第一个版本没有；
    /// 合并了并列的版本时有几个
    #[serde(default)]
    pub parents: Vec<String>,
    /// 上一版本各自的内容标识（`Pack::content_id`），和 `parents` 一一对应。上一版本在同步
    /// 位置上已经删了时，按它去本机备份里找基准
    #[serde(default)]
    pub parent_content: Vec<String>,
    /// 内容的 SHA-256（只在不加密时写）：发现传输中损坏。加密时 age 的认证加密已经管了
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// 还没处理的冲突
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<Conflict>,
}

/// 一处冲突：同一个对象（的几个单位）在两台设备上被改成了不同的内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conflict {
    /// 随机生成
    pub id: String,
    pub item: ItemId,
    /// 冲突的单位
    pub units: Vec<String>,
    /// 同步版本里现在是这一边的内容
    pub shared: Side,
    /// 另一边：它的设备在冲突处理之前一直用自己的
    pub other: Side,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
}

/// 冲突的一边
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Side {
    pub device: String,
    pub device_name: String,
    /// 这一边改的时间
    pub at_ms: u64,
    /// 冲突的那几个单位在这一边的值。**这一边删了这个对象**是 `None`
    pub units: Option<BTreeMap<String, Value>>,
}

/// 冲突不是因为两边改得不一样，而是：
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// 另一边删了一把网关密钥，而这台设备上已接管的客户端正在用它：没有删
    KeyInUse,
    /// 合并出来的结果过不了配置校验（比如路由引用的上游被另一边删了）
    Invalid,
}

impl Conflict {
    /// 有一边删了整个对象：两边各留各的是整个对象，不只是几个单位
    pub fn whole_item(&self) -> bool {
        self.shared.units.is_none() || self.other.units.is_none()
    }

    /// 「两者都保留」能不能选：两边都还有这个对象，而且它有名字（全局设置、顺序、通用偏好
    /// 没有第二份可言）
    pub fn both_allowed(&self) -> bool {
        !self.whole_item() && self.item.kind.is_named() && self.reason.is_none()
    }

    /// 这台设备看这处冲突时，哪一边是「本机」：自己写的那一边；不是参与的设备时，是和本机
    /// 现在内容一样的那一边，都不一样就是同步版本里的那一边
    pub fn mine<'a>(
        &'a self,
        device: &str,
        local_units: Option<&BTreeMap<String, Value>>,
    ) -> (&'a Side, &'a Side) {
        if self.other.device == device {
            return (&self.other, &self.shared);
        }
        if self.shared.device == device {
            return (&self.shared, &self.other);
        }
        let mine_now = local_units.map(|l| self.pick(l));
        if mine_now.as_ref() == self.other.units.as_ref() {
            (&self.other, &self.shared)
        } else {
            (&self.shared, &self.other)
        }
    }

    /// 一个对象里冲突的那几个单位。有一边删了整个对象时是整个对象（处理冲突选了「有」的
    /// 那一边，要拿它整个恢复回来）
    pub fn pick(&self, units: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
        if self.whole_item() {
            return units.clone();
        }
        self.units
            .iter()
            .filter_map(|u| units.get(u).map(|v| (u.clone(), v.clone())))
            .collect()
    }
}

/// 一组冲突各留各的那部分：整个对象，和单个单位
pub fn holds(conflicts: &[Conflict]) -> (BTreeSet<ItemId>, BTreeSet<(ItemId, String)>) {
    let mut items = BTreeSet::new();
    let mut units = BTreeSet::new();
    for c in conflicts {
        if c.whole_item() {
            items.insert(c.item.clone());
        } else {
            for u in &c.units {
                units.insert((c.item.clone(), u.clone()));
            }
        }
    }
    (items, units)
}

/// 新的冲突标识
pub fn new_id() -> String {
    let r: u64 = rand::random();
    format!("{r:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tw_pack::model::ItemKind;

    fn side(device: &str, v: Option<Value>) -> Side {
        Side {
            device: device.into(),
            device_name: device.to_uppercase(),
            at_ms: 1,
            units: v.map(|v| BTreeMap::from([("conn".to_string(), v)])),
        }
    }

    fn c(shared: Option<Value>, other: Option<Value>, kind: ItemKind) -> Conflict {
        Conflict {
            id: new_id(),
            item: ItemId::new(kind, "relay"),
            units: vec!["conn".into()],
            shared: side("aaaa", shared),
            other: side("bbbb", other),
            reason: None,
        }
    }

    #[test]
    fn each_device_sees_its_own_side_as_local() {
        let x = c(Some(json!("A")), Some(json!("B")), ItemKind::Provider);
        assert_eq!(x.mine("aaaa", None).0.device, "aaaa");
        assert_eq!(x.mine("bbbb", None).0.device, "bbbb");
        // 第三台：和本机内容一样的那一边
        let local = BTreeMap::from([
            ("conn".to_string(), json!("B")),
            ("models".into(), json!(1)),
        ]);
        assert_eq!(x.mine("cccc", Some(&local)).0.device, "bbbb");
        assert_eq!(x.mine("cccc", None).0.device, "aaaa");
    }

    #[test]
    fn keeping_both_needs_two_living_named_objects() {
        assert!(c(Some(json!(1)), Some(json!(2)), ItemKind::Provider).both_allowed());
        assert!(!c(None, Some(json!(2)), ItemKind::Provider).both_allowed());
        assert!(!c(Some(json!(1)), None, ItemKind::Route).both_allowed());
        assert!(!c(Some(json!(1)), Some(json!(2)), ItemKind::Global).both_allowed());
        assert!(!c(Some(json!(1)), Some(json!(2)), ItemKind::RouteOrder).both_allowed());
        let mut k = c(Some(json!(1)), None, ItemKind::Key);
        k.reason = Some(Reason::KeyInUse);
        assert!(!k.both_allowed());
    }

    #[test]
    fn holds_split_into_whole_objects_and_units() {
        let (items, units) = holds(&[c(Some(json!(1)), Some(json!(2)), ItemKind::Provider), {
            let mut d = c(None, Some(json!(2)), ItemKind::Route);
            d.item = ItemId::new(ItemKind::Route, "long");
            d
        }]);
        assert_eq!(
            items,
            BTreeSet::from([ItemId::new(ItemKind::Route, "long")])
        );
        assert_eq!(
            units,
            BTreeSet::from([(ItemId::new(ItemKind::Provider, "relay"), "conn".to_string())])
        );
    }

    #[test]
    fn a_record_survives_json() {
        let m = Meta {
            seq: 7,
            device: "aaaa".into(),
            device_name: "Mac mini".into(),
            parents: vec!["0000000006-aaaaaaaa".into()],
            parent_content: vec!["abc".into()],
            sha256: None,
            conflicts: vec![c(Some(json!({"base_url": "x"})), None, ItemKind::Key)],
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["conflicts"][0]["item"]["kind"], "key");
        let back: Meta = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
    }
}
