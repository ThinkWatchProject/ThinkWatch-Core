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
    /// 留下来的项目还引用着它（留着的密钥绑的路由、没选的路由用的上游……）：删了配置就不成立
    Referenced,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub item: ItemId,
    pub change: Change,
    pub kept: Option<Kept>,
}

/// 恢复的条件。
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// 固定密钥的名字：接管的客户端在用，不删
    pub pinned: BTreeSet<String>,
    /// 通用偏好算不算。经远程连接恢复时不算：偏好在 Lite 那边
    pub with_prefs: bool,
    /// 只恢复这几项（连同它们要的，见 [`plan`]），None 是全部
    pub items: Option<BTreeSet<ItemId>>,
    /// 不参与比较、保持现在样子的项目：恢复同步位置上的版本时，本机不同步的上游和随之不同步
    /// 的项目（规格 7.2）—— 那一版里本来就没有它们，不是删了
    pub skip: BTreeSet<ItemId>,
}

/// 那一版和现在的差异。`pinned` 是固定密钥的名字。`with_prefs` 为 false 时通用偏好不算
/// （经远程连接恢复：偏好在 Lite 那边）
pub fn preview(
    local: &Pack,
    backup: &Pack,
    pinned: &BTreeSet<String>,
    with_prefs: bool,
) -> Result<Vec<Row>, ModelError> {
    preview_opts(
        local,
        backup,
        &Options {
            pinned: pinned.clone(),
            with_prefs,
            ..Default::default()
        },
    )
}

/// 照这些条件恢复会动哪几项。**和 [`apply_opts`] 是同一个计划**：只选了几项时，预览列出的
/// 就是写下去的（连同一起恢复的、留着的）
pub fn preview_opts(local: &Pack, backup: &Pack, opts: &Options) -> Result<Vec<Row>, ModelError> {
    preview_with(local, backup, opts, model::codec())
}

pub fn preview_with(
    local: &Pack,
    backup: &Pack,
    opts: &Options,
    codec: &dyn PluginCodec,
) -> Result<Vec<Row>, ModelError> {
    let l = model::snapshot_with(local, codec)?;
    let b = model::snapshot_with(backup, codec)?;
    Ok(plan(&l, &b, &local_refs(local), opts).0)
}

/// 本机那几节（[`model::LOCAL_SECTIONS`]）引用的项目：同步设置走的出站代理。恢复留着本机那
/// 几节，它们引用的就得留着 —— 那一版里没有这个代理的话，删了它同步设置就不成立
fn local_refs(local: &Pack) -> BTreeSet<ItemId> {
    local_refs_in(&local.config_yaml)
}

fn local_refs_in(text: &str) -> BTreeSet<ItemId> {
    fn walk(v: &serde_yaml_ng::Value, out: &mut BTreeSet<ItemId>) {
        match v {
            serde_yaml_ng::Value::Mapping(m) => {
                for (k, v) in m {
                    if k.as_str() == Some("proxy")
                        && let Some(name) = v.as_str()
                        && name != tw_config::DIRECT
                        && name != tw_config::SYSTEM
                    {
                        out.insert(ItemId::new(ItemKind::Proxy, name));
                    }
                    walk(v, out);
                }
            }
            serde_yaml_ng::Value::Sequence(xs) => xs.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    if let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text) {
        for section in model::LOCAL_SECTIONS {
            if let Some(v) = doc.get(*section) {
                walk(v, &mut out);
            }
        }
    }
    out
}

/// 那一版，`skip` 里的项目换成现在的样子：现在有的照现在的，现在没有的不在；排列顺序里它们
/// 照现在的位置
fn overlay(l: &Snapshot, b: &Snapshot, skip: &BTreeSet<ItemId>) -> Snapshot {
    if skip.is_empty() {
        return b.clone();
    }
    let mut out = b.clone();
    for id in skip {
        match l.items.get(id) {
            Some(it) => {
                out.items.insert(id.clone(), it.clone());
            }
            None => {
                out.items.remove(id);
            }
        }
    }
    for kind in [ItemKind::Provider, ItemKind::Route, ItemKind::Plugin] {
        let order = ItemId::new(kind.order().expect("has an order"), "");
        let skipped = |n: &String| skip.contains(&ItemId::new(kind, n.clone()));
        let here = names_of(l.items.get(&order));
        let mut list: Vec<String> = names_of(out.items.get(&order))
            .into_iter()
            .filter(|n| !skipped(n))
            .collect();
        // 跳过的那几个插回现在的位置：紧跟在现在排在它前面、那一版里也有的那一个后面
        for (i, n) in here.iter().enumerate().filter(|(_, n)| skipped(n)) {
            let at = here[..i]
                .iter()
                .rev()
                .find_map(|p| list.iter().position(|x| x == p))
                .map_or(0, |p| p + 1);
            list.insert(at, n.clone());
        }
        if list.is_empty() && !out.items.contains_key(&order) {
            continue;
        }
        out.items.insert(
            order,
            model::Item {
                units: std::collections::BTreeMap::from([(
                    unit::ALL.to_string(),
                    Json::Array(list.into_iter().map(Json::String).collect()),
                )]),
                ..Default::default()
            },
        );
    }
    out.relink();
    out
}

fn names_of(it: Option<&model::Item>) -> Vec<String> {
    match it.and_then(|it| it.units.get(unit::ALL)) {
        Some(Json::Array(a)) => a
            .iter()
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// 两份之间的差异，按项目
fn diff(l: &Snapshot, b: &Snapshot, with_prefs: bool) -> Vec<(ItemId, Change)> {
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
            Some((id.clone(), change))
        })
        .collect()
}

/// 这个引用缺了，配置就不成立吗。别名（模型名）、插件的范围缺了只是匹配不到什么
fn hard(from: &ItemId, to: &ItemId) -> bool {
    from.kind != ItemKind::Plugin
        && to.kind != ItemKind::Alias
        && !(to.kind == ItemKind::Route && to.name == tw_engine::DEFAULT_ROUTE)
}

/// 照差异落成的目标，和预览里的那几行。
///
/// **预览能过的，写下去一定成**：恢复之后还有项目引用着缺了的东西时，缺的那一样
///
/// - 是这次要删的：留着（[`Kept::Referenced`]），比如留着的固定密钥绑的那条路由；
/// - 是那一版里有、没选上的：一起恢复，比如只选了一个已删的上游、它用的出站代理也删了。
///
/// 一直补到没有缺的为止。路由的去处写的是一个名字：上游里找不到时按同名的策略组找（组删了
/// 之后它读成一个不存在的上游）。`local` 是本机那几节引用的项目（[`local_refs`]），和别的
/// 引用一样总得留着
fn plan(
    l: &Snapshot,
    b: &Snapshot,
    local: &BTreeSet<ItemId>,
    opts: &Options,
) -> (Vec<Row>, Snapshot) {
    let b = &overlay(l, b, &opts.skip);
    let changes = diff(l, b, opts.with_prefs);
    let mut chosen: BTreeSet<ItemId> = changes
        .iter()
        .map(|(id, _)| id.clone())
        .filter(|id| opts.items.as_ref().is_none_or(|s| s.contains(id)))
        .collect();
    let mut kept: std::collections::BTreeMap<ItemId, Kept> = changes
        .iter()
        .filter(|(id, c)| {
            *c == Change::Remove && id.kind == ItemKind::Key && opts.pinned.contains(&id.name)
        })
        .map(|(id, _)| (id.clone(), Kept::KeyInUse))
        .collect();
    loop {
        let mut target = l.clone();
        for (id, _) in changes.iter().filter(|(id, _)| chosen.contains(id)) {
            if kept.contains_key(id) {
                continue;
            }
            match b.items.get(id) {
                Some(it) => {
                    target.items.insert(id.clone(), it.clone());
                }
                None => {
                    target.items.remove(id);
                }
            }
        }
        target.relink();
        let mut more = false;
        let missing: Vec<ItemId> = target
            .items
            .iter()
            .flat_map(|(id, it)| {
                it.refs
                    .iter()
                    .filter(|r| hard(id, r) && !target.items.contains_key(*r))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .chain(
                local
                    .iter()
                    .filter(|r| !target.items.contains_key(*r))
                    .cloned(),
            )
            .collect();
        for r in missing {
            // 指向一个名字的：上游里没有就是同名的策略组
            let group = ItemId::new(ItemKind::Group, r.name.clone());
            let r = if r.kind == ItemKind::Provider
                && !l.items.contains_key(&r)
                && !b.items.contains_key(&r)
                && (l.items.contains_key(&group) || b.items.contains_key(&group))
            {
                group
            } else {
                r
            };
            if target.items.contains_key(&r) {
                continue;
            }
            if l.items.contains_key(&r) {
                // 这次要删的：留着
                if let std::collections::btree_map::Entry::Vacant(e) = kept.entry(r) {
                    e.insert(Kept::Referenced);
                    more = true;
                }
            } else if b.items.contains_key(&r) && chosen.insert(r) {
                // 那一版里有、没选上的：一起恢复
                more = true;
            }
        }
        if !more {
            let rows = changes
                .into_iter()
                .filter(|(id, _)| chosen.contains(id) || kept.contains_key(id))
                .map(|(id, change)| Row {
                    kept: kept.get(&id).copied(),
                    item: id,
                    change,
                })
                .collect();
            return (rows, target);
        }
    }
}

/// 恢复的结果。
#[derive(Debug, Clone)]
pub struct Applied {
    pub pack: Pack,
    /// 值变了或者没了的网关密钥
    pub changed_keys: Vec<String>,
    /// 留着没删的：固定密钥，和留下来的项目还引用着的
    pub kept: Vec<ItemId>,
    /// 实际动了的那几项（连同一起恢复的、留着的），和同样条件下的预览一样
    pub rows: Vec<Row>,
}

/// 恢复。`items` 是只恢复这几项，None 是全部。所选项目引用的、本机已经没有的项目一起恢复
pub fn apply(
    local: &Pack,
    backup: &Pack,
    items: Option<&[ItemId]>,
    pinned: &BTreeSet<String>,
    with_prefs: bool,
) -> Result<Applied, ModelError> {
    apply_opts(
        local,
        backup,
        &Options {
            pinned: pinned.clone(),
            with_prefs,
            items: items.map(|xs| xs.iter().cloned().collect()),
            ..Default::default()
        },
    )
}

pub fn apply_opts(local: &Pack, backup: &Pack, opts: &Options) -> Result<Applied, ModelError> {
    apply_with(local, backup, opts, model::codec())
}

pub fn apply_with(
    local: &Pack,
    backup: &Pack,
    opts: &Options,
    codec: &dyn PluginCodec,
) -> Result<Applied, ModelError> {
    let l = model::snapshot_with(local, codec)?;
    let b = model::snapshot_with(backup, codec)?;
    let (rows, mut target) = plan(&l, &b, &local_refs(local), opts);
    let kept = rows
        .iter()
        .filter(|r| r.kept.is_some())
        .map(|r| r.item.clone())
        .collect();
    // 那一版有、现在没有的 ChatGPT 账号上游：它的登录随删除吊销了，停用着回来
    for row in rows
        .iter()
        .filter(|r| r.change == Change::Add && r.kept.is_none())
    {
        if row.item.kind != ItemKind::Provider {
            continue;
        }
        let Some(it) = target.items.get_mut(&row.item) else {
            continue;
        };
        if model::provider_of(&row.item, it).is_ok_and(|p| model::is_chatgpt(&p))
            && let Some(Json::Object(other)) = it.units.get_mut(unit::OTHER)
        {
            other.insert("disabled".into(), Json::Bool(true));
        }
    }
    target.relink();
    let mut pack = model::materialize_with(local, &target, codec)?;
    pack.config_yaml = settle_logins(&pack.config_yaml, backup)?;
    tw_config::try_parse(&pack.config_yaml).map_err(|r| ModelError::Config(Box::new(r)))?;
    Ok(Applied {
        changed_keys: import::changed_keys(local, &pack),
        pack,
        kept,
        rows,
    })
}

/// 落下去之后没有令牌的 OAuth 上游（那一版有、现在没有的；后来换了令牌端点、本机的令牌
/// 用不上的）：那一版里用的是同一个令牌端点、有令牌的，令牌从那一版里取回来；没有的停用着
/// 回来，等填上刷新令牌 —— 不是恢复失败
fn settle_logins(text: &str, backup: &Pack) -> Result<String, ModelError> {
    let now = model::parse(text)?;
    let then = model::parse(&backup.config_yaml)?;
    let mut out = text.to_string();
    for p in &now.providers {
        let Some(o) = &p.oauth else { continue };
        if model::is_chatgpt(p) || p.disabled || !o.refresh.trim().is_empty() {
            continue;
        }
        let from_then = then
            .providers
            .iter()
            .find(|x| x.name == p.name)
            .and_then(|x| x.oauth.as_ref())
            .filter(|t| t.endpoint == o.endpoint && !t.refresh.trim().is_empty());
        match from_then {
            Some(t) => out = put_tokens(&out, &p.name, t)?,
            None => {
                out = tw_config::edit::set(
                    &out,
                    &provider_field(&out, &p.name, "disabled")?,
                    Some(&serde_yaml_ng::Value::Bool(true)),
                )?;
            }
        }
    }
    Ok(out)
}

/// 把一份 OAuth 的令牌写进 `text` 里上游 `name` 那一家
fn put_tokens(text: &str, name: &str, o: &tw_config::OAuth) -> Result<String, ModelError> {
    let tokens = tw_config::RenewedTokens {
        access: o.access.as_deref().unwrap_or_default(),
        expires_at: o.expires_at.as_deref(),
        refresh: Some(&o.refresh),
    };
    let mut out =
        tw_config::patch_oauth_tokens(text, name, tokens).map_err(|e| ModelError::Shape {
            item: format!("provider `{name}`"),
            detail: e.to_string(),
        })?;
    // 没有 access token 的：去掉写进去的空值，下一次用到时拿 refresh 换
    if o.access.is_none() {
        let mut p = provider_field(&out, name, "oauth")?;
        p.push(tw_yaml::Step::key("access"));
        out = tw_config::edit::set(&out, &p, None)?;
    }
    Ok(out)
}

/// 配置里上游 `name` 的某个字段的路径
fn provider_field(text: &str, name: &str, field: &str) -> Result<Vec<tw_yaml::Step>, ModelError> {
    let doc = tw_config::edit::parse(text)?;
    let i = tw_config::edit::PROVIDERS
        .index_of(&doc, name)
        .ok_or_else(|| ModelError::Shape {
            item: format!("provider `{name}`"),
            detail: "it is not in the configuration".into(),
        })?;
    Ok(vec![
        tw_yaml::Step::key("providers"),
        tw_yaml::Step::Index(i),
        tw_yaml::Step::key(field),
    ])
}

/// 配置读不进来（安全模式）时的恢复：整份换成那一版，**本机那几节（监听、控制面的钥匙、
/// 保留期、同步设置）一律是现在的**：取现在这份原文里的（读得成 YAML 就取得到，哪怕它读不成
/// 一份配置），取不到就取 `last_local`（最近一个本机备份）里的，再取不到才留着那一版自己的。
/// 钥匙是现在这一把。插件和通用偏好照那一版。
///
/// 那一版可以是同步位置上的、导出的，里面本来就没有这几节 —— 不补上的话写回去连钥匙都没有
pub fn whole(backup: &Pack, current_text: &str, last_local: Option<&Pack>) -> Pack {
    use serde_yaml_ng::Value as Yaml;
    let real = tw_config::control_key::raw_in(current_text)
        .ok()
        .flatten()
        .or_else(|| raw_key_line(current_text))
        .filter(|k| k.as_str() != tw_api::control::KEY_MASK);
    let doc = |t: &str| {
        serde_yaml_ng::from_str::<Yaml>(t)
            .ok()
            .filter(Yaml::is_mapping)
    };
    let source = doc(current_text).or_else(|| last_local.and_then(|p| doc(&p.config_yaml)));
    let mut text = backup.config_yaml.clone();
    // 那一版是同步位置上的（没有本机那几节）：本机不同步的上游本来就不在里面
    let from_sync = backup.manifest.origin == crate::PackOrigin::Sync
        || doc(&backup.config_yaml)
            .is_some_and(|d| model::LOCAL_SECTIONS.iter().all(|k| d.get(*k).is_none()));
    if from_sync {
        let current = Pack {
            config_yaml: current_text.to_string(),
            ..backup.clone()
        };
        let from: Vec<&Pack> = std::iter::once(&current).chain(last_local).collect();
        text = bring_not_synced(&text, &from);
    }
    // 两处都取不到（原文坏到读不成 YAML、也没有本机备份）：留着那一版自己的
    for section in source.iter().flat_map(|_| model::LOCAL_SECTIONS) {
        let v = source.as_ref().and_then(|d| d.get(*section)).cloned();
        if let Ok(t) = tw_config::edit::set(&text, &[tw_yaml::Step::key(*section)], v.as_ref()) {
            text = t;
        }
    }
    // 本机那几节引用的出站代理（同步设置走的）那一版里没有：从取本机那几节的地方一起取来
    if let Some(src) = &source {
        let named = |d: &Yaml, name: &str| -> Option<Yaml> {
            d.get("proxies")?
                .as_sequence()?
                .iter()
                .find(|p| p.get("name").and_then(Yaml::as_str) == Some(name))
                .cloned()
        };
        for r in local_refs_in(&text) {
            let Some(now) = doc(&text) else { break };
            if named(&now, &r.name).is_some() {
                continue;
            }
            let Some(px) = named(src, &r.name) else {
                continue;
            };
            let mut list = now
                .get("proxies")
                .and_then(Yaml::as_sequence)
                .cloned()
                .unwrap_or_default();
            list.push(px);
            if let Ok(t) = tw_config::edit::set(
                &text,
                &[tw_yaml::Step::key("proxies")],
                Some(&Yaml::Sequence(list)),
            ) {
                text = t;
            }
        }
    }
    if let Some(k) = real {
        let at: Vec<tw_yaml::Step> = tw_api::control::KEY_PATH
            .iter()
            .map(|k| tw_yaml::Step::key(*k))
            .collect();
        if let Ok(t) = tw_yaml::insert(&text, &at, &tw_yaml::Scalar::s(k)) {
            text = t;
        }
    }
    Pack {
        config_yaml: text,
        ..backup.clone()
    }
}

/// 本机不同步的上游（`sync.exclude`），连同直接或间接引用它们的项目（路由、策略组、别名、
/// 网关密钥）。和同步那一路的算法一样
fn not_synced(names: &BTreeSet<String>, snap: &Snapshot) -> BTreeSet<ItemId> {
    let mut out: BTreeSet<ItemId> = names
        .iter()
        .map(|n| ItemId::new(ItemKind::Provider, n.clone()))
        .collect();
    loop {
        let before = out.len();
        for (id, it) in &snap.items {
            if !id.kind.is_order() && it.refs.iter().any(|r| out.contains(r)) {
                out.insert(id.clone());
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

/// 整份恢复同步位置上的一个版本：本机不同步的上游和随之不同步的项目，那一版里没有的从 `from`
/// 里第一份读得成一份配置的（现在的原文，或者最近一个读得成的本机版本）原样取来，连同它们的
/// 登录，接在各段的末尾。插件不取：它的文件和批准不在配置里
fn bring_not_synced(text: &str, from: &[&Pack]) -> String {
    use tw_config::edit::{self, Section};
    const CLIENTS: Section = Section {
        path: &["clients"],
        what: "gateway key",
        key: "name",
    };
    let Some((src, snap, raw)) = from.iter().find_map(|p| {
        Some((
            tw_config::try_parse(&p.config_yaml).ok()?,
            model::snapshot(p).ok()?,
            edit::parse(&p.config_yaml).ok()?,
        ))
    }) else {
        return text.to_string();
    };
    let names: BTreeSet<String> = src
        .sync
        .as_ref()
        .map(|s| s.exclude.iter().cloned().collect())
        .unwrap_or_default();
    if names.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    // 先上游，再引用它们的
    let mut ids: Vec<ItemId> = not_synced(&names, &snap)
        .into_iter()
        .filter(|id| snap.items.contains_key(id))
        .collect();
    ids.sort_by_key(|id| match id.kind {
        ItemKind::Provider => 0,
        ItemKind::Group => 1,
        ItemKind::Route => 2,
        ItemKind::Alias => 3,
        _ => 4,
    });
    for id in ids {
        let Ok(now) = edit::parse(&out) else { break };
        let section = match id.kind {
            ItemKind::Provider => edit::PROVIDERS,
            ItemKind::Group => edit::GROUPS,
            ItemKind::Route => edit::ROUTES,
            ItemKind::Key => CLIENTS,
            ItemKind::Alias => {
                let there = now
                    .get("aliases")
                    .and_then(|a| a.get(id.name.as_str()))
                    .is_some();
                if !there
                    && let Some(a) = src.aliases.find(&id.name)
                    && let Ok(t) = edit::upsert_alias(&out, None, a)
                {
                    out = t;
                }
                continue;
            }
            _ => continue,
        };
        if section.index_of(&now, &id.name).is_some() {
            continue;
        }
        let entry = section.index_of(&raw, &id.name).and_then(|i| {
            let mut cur = &raw;
            for k in section.path {
                cur = cur.get(*k)?;
            }
            cur.get(i)?.as_mapping().cloned()
        });
        if let Some(m) = entry
            && let Ok(t) = edit::upsert(&out, section, None, &m)
        {
            out = t;
        }
    }
    out
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
