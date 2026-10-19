//! 在快照里给一个项目改名，引用它的地方跟着改。
//!
//! 和按名字编辑配置时的改名（`tw_config::refs`）是同一份规矩，只是改的是单位而不是原文：
//! 导入时选「改名导入」、同步时选「两者都保留」，改名的是配置包里的那一份，包里引用它的
//! 路由、别名、分组随之改用新名称。

use serde_json::Value as Json;

use super::items::rule_name;
use super::{ItemId, ItemKind, Snapshot, global, unit};

/// 把 `id` 改名成 `new_name`，连同快照里引用它的地方。没有这一项、名字没变、这种项目
/// 没有名字（顺序、全局设置、通用偏好）时什么都不做。新名字已经被占了也不做：调用方
/// 先挑一个没人用的（[`free_name`]）。
///
/// 自定义安全规则的 `new_name` 可以只写规则名（分项不变），也可以写成 `<分项>/<规则名>`
pub fn rename(snap: &mut Snapshot, id: &ItemId, new_name: &str) {
    if !id.kind.is_named() || id.name == new_name {
        return;
    }
    let new_name = match id.kind {
        ItemKind::SecurityRule if !new_name.contains('/') => match rule_name(&id.name) {
            Some((guard, _)) => format!("{guard}/{new_name}"),
            None => return,
        },
        _ => new_name.to_string(),
    };
    let new_id = ItemId::new(id.kind, new_name.clone());
    if snap.items.contains_key(&new_id) {
        return;
    }
    let Some(item) = snap.items.remove(id) else {
        return;
    };
    snap.items.insert(new_id, item);
    let old = id.name.as_str();
    let new = new_name.as_str();

    for (other, item) in snap.items.iter_mut() {
        let all = item.units.get_mut(unit::ALL);
        match (id.kind, other.kind) {
            (ItemKind::Provider, ItemKind::Route) => {
                if let Some(all) = all {
                    for rule in rules(all) {
                        match rule.get_mut("to") {
                            Some(Json::Array(pinned)) => {
                                for p in pinned {
                                    swap(p.get_mut("provider"), old, new);
                                }
                            }
                            to => swap(to, old, new),
                        }
                        if let Some(when) = rule.get_mut("when") {
                            swap_one_or_many(when.get_mut("provider_would_be"), old, new);
                        }
                    }
                }
            }
            (ItemKind::Provider, ItemKind::Group) => {
                if let Some(all) = all {
                    if let Some(Json::Array(members)) = all.get_mut("providers") {
                        for m in members {
                            match m {
                                Json::String(_) => swap(Some(m), old, new),
                                Json::Object(o) => swap(o.get_mut("name"), old, new),
                                _ => {}
                            }
                        }
                    }
                    swap(all.get_mut("selected"), old, new);
                }
            }
            (ItemKind::Provider, ItemKind::ProviderOrder)
            | (ItemKind::Route, ItemKind::RouteOrder)
            | (ItemKind::Plugin, ItemKind::PluginOrder) => {
                if let Some(Json::Array(list)) = all {
                    for n in list {
                        swap(Some(n), old, new);
                    }
                }
            }
            (ItemKind::Provider, ItemKind::Plugin) => {
                if let Some(Json::Array(list)) = item
                    .units
                    .get_mut(unit::SETTINGS)
                    .and_then(|d| d.get_mut("match"))
                    .and_then(|m| m.get_mut("upstreams"))
                {
                    for n in list {
                        swap(Some(n), old, new);
                    }
                }
            }
            (ItemKind::Proxy, ItemKind::Provider) => {
                swap(
                    item.units
                        .get_mut(unit::CONN)
                        .and_then(|c| c.get_mut("proxy")),
                    old,
                    new,
                );
            }
            (ItemKind::PriceSheet, ItemKind::Provider) => {
                swap(
                    item.units
                        .get_mut(unit::BILLING)
                        .and_then(|c| c.get_mut("pricing")),
                    old,
                    new,
                );
            }
            (ItemKind::Key, ItemKind::Route) => {
                if let Some(all) = all {
                    for rule in rules(all) {
                        swap(
                            rule.get_mut("when").and_then(|w| w.get_mut("client")),
                            old,
                            new,
                        );
                    }
                }
            }
            (ItemKind::Key, ItemKind::Global) if other.name == global::DEFAULT_KEY => {
                swap(item.units.get_mut(global::DEFAULT_KEY), old, new);
            }
            (ItemKind::Alias, ItemKind::Key) => {
                if let Some(Json::Array(allow)) = all.and_then(|a| a.get_mut("allow")) {
                    for m in allow {
                        swap(Some(m), old, new);
                    }
                }
            }
            (ItemKind::Alias, ItemKind::Route) => {
                if let Some(all) = all {
                    for rule in rules(all) {
                        let phase_two = rule
                            .get("when")
                            .is_some_and(|w| w.get("provider_would_be").is_some());
                        swap(
                            rule.get_mut("when").and_then(|w| w.get_mut("model")),
                            old,
                            new,
                        );
                        if !phase_two {
                            swap(
                                rule.get_mut("set").and_then(|s| s.get_mut("model")),
                                old,
                                new,
                            );
                        }
                    }
                }
            }
            (ItemKind::Group, ItemKind::Route) => {
                if let Some(all) = all {
                    for rule in rules(all) {
                        swap(rule.get_mut("to"), old, new);
                    }
                }
            }
            (ItemKind::Route, ItemKind::Key) => {
                swap(all.and_then(|a| a.get_mut("route")), old, new);
            }
            _ => {}
        }
    }

    // 默认路由：没写明时说的是叫「默认」的那条。改的正是它，要把新名字写明；改成的正是
    // 「默认」，写明的那一项就不用了
    if id.kind == ItemKind::Route {
        let at = ItemId::new(ItemKind::Global, global::DEFAULT_ROUTE);
        let current = snap
            .items
            .get(&at)
            .and_then(|i| i.units.get(global::DEFAULT_ROUTE))
            .and_then(Json::as_str)
            .unwrap_or(tw_engine::DEFAULT_ROUTE)
            .to_string();
        if current == old {
            if new == tw_engine::DEFAULT_ROUTE {
                snap.items.remove(&at);
            } else {
                snap.items.entry(at).or_default().units.insert(
                    global::DEFAULT_ROUTE.to_string(),
                    Json::String(new.to_string()),
                );
            }
        }
    }
    // 默认密钥同理
    if id.kind == ItemKind::Key {
        let at = ItemId::new(ItemKind::Global, global::DEFAULT_KEY);
        let written = snap
            .items
            .get(&at)
            .and_then(|i| i.units.get(global::DEFAULT_KEY))
            .is_some();
        if !written && old == tw_config::DEFAULT_KEY {
            snap.items.entry(at).or_default().units.insert(
                global::DEFAULT_KEY.to_string(),
                Json::String(new.to_string()),
            );
        } else if written && new == tw_config::DEFAULT_KEY {
            let now = snap
                .items
                .get(&at)
                .and_then(|i| i.units.get(global::DEFAULT_KEY));
            if now.and_then(Json::as_str) == Some(new) {
                snap.items.remove(&at);
            }
        }
    }
    snap.relink();
}

/// `base` 没被占就是它，被占了是「base (2)」「base (3)」……
pub fn free_name(snap: &Snapshot, kind: ItemKind, base: &str) -> String {
    let taken = |n: &str| snap.items.contains_key(&ItemId::new(kind, n));
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|i| format!("{base} ({i})"))
        .find(|n| !taken(n))
        .expect("an unbounded range always finds one")
}

fn rules(all: &mut Json) -> impl Iterator<Item = &mut Json> {
    all.get_mut("rules")
        .and_then(Json::as_array_mut)
        .into_iter()
        .flatten()
}

/// 这个位置上的字符串正是 `old` 就换成 `new`
fn swap(at: Option<&mut Json>, old: &str, new: &str) {
    if let Some(v) = at
        && v.as_str() == Some(old)
    {
        *v = Json::String(new.to_string());
    }
}

fn swap_one_or_many(at: Option<&mut Json>, old: &str, new: &str) {
    match at {
        Some(Json::Array(list)) => {
            for v in list {
                swap(Some(v), old, new);
            }
        }
        other => swap(other, old, new),
    }
}
