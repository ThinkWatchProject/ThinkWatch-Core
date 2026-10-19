//! 恢复：把配置换回某一个备份版本，全部或者其中几项。
//!
//! 按项目列出那一版和现在的差异（新增、删除、修改），缺省全选，也就是整体换回那一版；
//! 没选的保持现在的样子。
//!
//! - **本机设置留着现在的**（监听、保留期、同步设置、控制面的钥匙），ChatGPT 上游的登录也是
//!   现在的 —— 那一版里的旧 token 可能早就因为刷新作废了。那一版有、现在没有的 ChatGPT 上游
//!   恢复成停用：它的登录随删除吊销了，要重新登录
//! - **本机接管的客户端在用的网关密钥不删**（固定密钥）：那一行标成留着
//! - 恢复本身也留一个版本，所以可以撤销

use std::collections::BTreeSet;

use serde_json::Value as Json;

use crate::model::{self, ItemId, ItemKind, ModelError, PluginCodec, Snapshot, unit};
use crate::{Pack, import};

/// 恢复对一项做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Add,
    Remove,
    Modify,
}

/// 为什么留着它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// 接管的客户端在用这把网关密钥
    KeyInUse,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub item: ItemId,
    pub change: Change,
    pub kept: Option<Kept>,
}

/// 那一版和现在的差异。`pinned` 是固定密钥的名字。`with_prefs` 为 false 时通用偏好不算
/// （经远程连接恢复：偏好在 Lite 那边）
pub fn preview(
    local: &Pack,
    backup: &Pack,
    pinned: &BTreeSet<String>,
    with_prefs: bool,
) -> Result<Vec<Row>, ModelError> {
    preview_with(local, backup, pinned, with_prefs, model::codec())
}

pub fn preview_with(
    local: &Pack,
    backup: &Pack,
    pinned: &BTreeSet<String>,
    with_prefs: bool,
    codec: &dyn PluginCodec,
) -> Result<Vec<Row>, ModelError> {
    let l = model::snapshot_with(local, codec)?;
    let b = model::snapshot_with(backup, codec)?;
    Ok(rows(&l, &b, pinned, with_prefs))
}

fn rows(l: &Snapshot, b: &Snapshot, pinned: &BTreeSet<String>, with_prefs: bool) -> Vec<Row> {
    let (l, b) = (model::normalized(l), model::normalized(b));
    let ids: BTreeSet<&ItemId> = l.items.keys().chain(b.items.keys()).collect();
    ids.into_iter()
        .filter(|id| with_prefs || id.kind != ItemKind::ClientPrefs)
        .filter_map(|id| {
            let change = match (l.items.get(id), b.items.get(id)) {
                (None, Some(_)) => Change::Add,
                (Some(_), None) => Change::Remove,
                (Some(x), Some(y)) if x.units != y.units => Change::Modify,
                _ => return None,
            };
            let kept =
                (change == Change::Remove && id.kind == ItemKind::Key && pinned.contains(&id.name))
                    .then_some(Kept::KeyInUse);
            Some(Row {
                item: id.clone(),
                change,
                kept,
            })
        })
        .collect()
}

/// 恢复的结果。
#[derive(Debug, Clone)]
pub struct Applied {
    pub pack: Pack,
    /// 值变了或者没了的网关密钥
    pub changed_keys: Vec<String>,
    /// 因为固定密钥留着没删的
    pub kept: Vec<ItemId>,
}

/// 恢复。`items` 是只恢复这几项，None 是全部
pub fn apply(
    local: &Pack,
    backup: &Pack,
    items: Option<&[ItemId]>,
    pinned: &BTreeSet<String>,
    with_prefs: bool,
) -> Result<Applied, ModelError> {
    apply_with(local, backup, items, pinned, with_prefs, model::codec())
}

pub fn apply_with(
    local: &Pack,
    backup: &Pack,
    items: Option<&[ItemId]>,
    pinned: &BTreeSet<String>,
    with_prefs: bool,
    codec: &dyn PluginCodec,
) -> Result<Applied, ModelError> {
    let l = model::snapshot_with(local, codec)?;
    let b = model::snapshot_with(backup, codec)?;
    let picked: Option<BTreeSet<&ItemId>> = items.map(|xs| xs.iter().collect());
    let mut target = l.clone();
    let mut kept = Vec::new();
    for row in rows(&l, &b, pinned, with_prefs) {
        if picked.as_ref().is_some_and(|p| !p.contains(&row.item)) {
            continue;
        }
        if row.kept.is_some() {
            kept.push(row.item.clone());
            continue;
        }
        match b.items.get(&row.item) {
            Some(it) => {
                let mut it = it.clone();
                // 那一版有、现在没有的 ChatGPT 账号上游：没有登录，停用着回来
                if row.item.kind == ItemKind::Provider
                    && !l.items.contains_key(&row.item)
                    && model::provider_of(&row.item, &it).is_ok_and(|p| model::is_chatgpt(&p))
                    && let Some(Json::Object(other)) = it.units.get_mut(unit::OTHER)
                {
                    other.insert("disabled".into(), Json::Bool(true));
                }
                target.items.insert(row.item.clone(), it);
            }
            None => {
                target.items.remove(&row.item);
            }
        }
    }
    target.relink();
    let pack = model::materialize_with(local, &target, codec)?;
    tw_config::try_parse(&pack.config_yaml).map_err(|r| ModelError::Config(Box::new(r)))?;
    Ok(Applied {
        changed_keys: import::changed_keys(local, &pack),
        pack,
        kept,
    })
}

/// 配置读不进来（安全模式）时的恢复：整份换成那一版，**只有控制面的钥匙换成现在这一把**
/// （`current_text` 是磁盘上现在那份读不进来的原文）。插件和通用偏好照那一版
pub fn whole(backup: &Pack, current_text: &str) -> Pack {
    let real = tw_config::control_key::raw_in(current_text)
        .ok()
        .flatten()
        .or_else(|| raw_key_line(current_text));
    let text = tw_config::control_key::unmask(&backup.config_yaml, real.as_deref())
        .unwrap_or_else(|_| backup.config_yaml.clone());
    Pack {
        config_yaml: text,
        ..backup.clone()
    }
}

/// YAML 坏到按路径找不到时，退一步在原文里找 `key: <64 个十六进制字符>` 那一行
fn raw_key_line(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let v = l
            .trim()
            .strip_prefix("key:")?
            .trim()
            .trim_matches(['"', '\'']);
        (v.len() == tw_api::control::KEY_HEX_LEN && v.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| v.to_string())
    })
}

#[cfg(test)]
mod tests;
