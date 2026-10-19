//! 配置 → 项目，项目 → 配置里的一项，以及项目之间的引用。

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value as Json};
use serde_yaml_ng::{Mapping, Value as Yaml};
use tw_config::{Config, Protocol, Provider};

use super::json::{key, to_json, to_yaml, typed_mapping};
use super::{GUARDS, Item, ItemId, ItemKind, ModelError, PluginCodec, global, unit};
use crate::{Pack, PluginFiles};

/// ChatGPT 账号登录所得的账户 ID 所在的请求头：跟着登录走，不随配置带走
pub(crate) const ACCOUNT_HEADER: &str = tw_config::chatgpt::ACCOUNT_HEADER;

/// 一个上游是 ChatGPT 账号。它的登录（`oauth`）和账户 ID 请求头属于这台设备
pub fn is_chatgpt(p: &Provider) -> bool {
    p.effective_protocol() == Some(Protocol::Chatgpt)
}

fn shape(item: &ItemId, detail: impl std::fmt::Display) -> ModelError {
    ModelError::Shape {
        item: item.to_string(),
        detail: detail.to_string(),
    }
}

// ─────────────────────────────────────────────────────────── 配置 → 项目

pub(crate) fn of_config(
    cfg: &Config,
    pack: &Pack,
    codec: &dyn PluginCodec,
) -> Result<BTreeMap<ItemId, Item>, ModelError> {
    let mut out = BTreeMap::new();
    let mut put = |id: ItemId, units: BTreeMap<String, Json>| {
        out.insert(
            id,
            Item {
                units,
                ..Default::default()
            },
        );
    };

    for p in &cfg.providers {
        let id = ItemId::new(ItemKind::Provider, &p.name);
        let units = provider_units(p).map_err(|e| shape(&id, e))?;
        put(id, units);
    }
    order(
        &mut put,
        ItemKind::ProviderOrder,
        cfg.providers.iter().map(|p| &p.name),
    );

    for px in &cfg.proxies {
        let id = ItemId::new(ItemKind::Proxy, &px.name);
        let units = whole(px, "name").map_err(|e| shape(&id, e))?;
        put(id, units);
    }
    for c in &cfg.clients {
        let id = ItemId::new(ItemKind::Key, &c.name);
        let units = whole(c, "name").map_err(|e| shape(&id, e))?;
        put(id, units);
    }
    for a in cfg.aliases.iter() {
        let id = ItemId::new(ItemKind::Alias, &a.name);
        put(
            id,
            one(Json::Array(
                a.models.iter().cloned().map(Json::String).collect(),
            )),
        );
    }
    for g in &cfg.groups {
        let id = ItemId::new(ItemKind::Group, &g.name);
        let units = whole(g, "name").map_err(|e| shape(&id, e))?;
        put(id, units);
    }
    for r in &cfg.routes {
        let id = ItemId::new(ItemKind::Route, &r.name);
        let units = whole(r, "name").map_err(|e| shape(&id, e))?;
        put(id, units);
    }
    order(
        &mut put,
        ItemKind::RouteOrder,
        cfg.routes.iter().map(|r| &r.name),
    );
    for s in &cfg.pricing.sheets {
        let id = ItemId::new(ItemKind::PriceSheet, &s.name);
        let units = whole(s, "name").map_err(|e| shape(&id, e))?;
        put(id, units);
    }
    let rules: [(&str, Vec<Json>); 3] = [
        ("redact", rule_values(&cfg.security.redact.custom)),
        (
            "inspect_tools",
            rule_values(&cfg.security.inspect_tools.custom),
        ),
        ("content", rule_values(&cfg.security.content.custom)),
    ];
    for (guard, list) in rules {
        for v in list {
            let Json::Object(mut o) = v else { continue };
            let name = o
                .remove("name")
                .and_then(|n| n.as_str().map(str::to_string))
                .unwrap_or_default();
            put(
                ItemId::new(ItemKind::SecurityRule, format!("{guard}/{name}")),
                one(Json::Object(o)),
            );
        }
    }

    for p in &cfg.plugins {
        let id = ItemId::new(ItemKind::Plugin, &p.id);
        let files = pack.plugins.get(&p.id);
        let (units, files) = plugin_units(p, files, codec);
        out.insert(
            id,
            Item {
                units,
                files,
                ..Default::default()
            },
        );
    }
    let mut put = |id: ItemId, units: BTreeMap<String, Json>| {
        out.insert(
            id,
            Item {
                units,
                ..Default::default()
            },
        );
    };
    order(
        &mut put,
        ItemKind::PluginOrder,
        cfg.plugins.iter().map(|p| &p.id),
    );

    for (name, units) in
        global_units(cfg).map_err(|e| shape(&ItemId::new(ItemKind::Global, ""), e))?
    {
        if !units.is_empty() {
            put(ItemId::new(ItemKind::Global, name), units);
        }
    }

    if let Some(prefs) = &pack.client_prefs {
        let mut units = BTreeMap::new();
        super::json::flatten("", prefs, &mut units);
        if !units.is_empty() && prefs != &Json::Object(Default::default()) {
            put(ItemId::new(ItemKind::ClientPrefs, ""), units);
        }
    }
    Ok(out)
}

fn one(v: Json) -> BTreeMap<String, Json> {
    BTreeMap::from([(unit::ALL.to_string(), v)])
}

fn order<'a>(
    put: &mut impl FnMut(ItemId, BTreeMap<String, Json>),
    kind: ItemKind,
    names: impl Iterator<Item = &'a String>,
) {
    let list: Vec<Json> = names.cloned().map(Json::String).collect();
    if !list.is_empty() {
        put(ItemId::new(kind, ""), one(Json::Array(list)));
    }
}

/// 整个对象是一个单位，名字不在里面（名字是项目的标识）
fn whole<T: serde::Serialize>(v: &T, name_key: &str) -> Result<BTreeMap<String, Json>, String> {
    match to_json(v)? {
        Json::Object(mut o) => {
            o.remove(name_key);
            Ok(one(Json::Object(o)))
        }
        _ => Err("it does not serialize to an object".into()),
    }
}

fn rule_values<T: serde::Serialize>(rules: &[T]) -> Vec<Json> {
    rules.iter().filter_map(|r| to_json(r).ok()).collect()
}

// ─────────────────────────────────────────────────────────── 上游

/// 上游的一个字段归哪个单位。**不认识的归 `other`**：将来新加的字段照样参与比较
fn provider_unit(field: &str) -> &'static str {
    match field {
        "base_url" | "protocol" | "key" | "headers" | "oauth" | "aws" | "proxy"
        | "on_proxy_fail" | "signed_in" => unit::CONN,
        "models" | "models_only" | "model_specs" => unit::MODELS,
        "billing" | "pricing" => unit::BILLING,
        _ => unit::OTHER,
    }
}

pub(crate) const PROVIDER_UNITS: &[&str] = &[unit::CONN, unit::MODELS, unit::BILLING, unit::OTHER];

/// 去掉这台设备自己的部分：ChatGPT 的登录和账户 ID 请求头
pub(crate) fn shareable(p: &Provider) -> Provider {
    let mut p = p.clone();
    if is_chatgpt(&p) {
        p.oauth = None;
        p.headers = tw_config::Headers::new(
            p.headers
                .iter()
                .filter(|h| !h.name.eq_ignore_ascii_case(ACCOUNT_HEADER))
                .cloned()
                .collect(),
        );
    }
    p
}

fn provider_units(p: &Provider) -> Result<BTreeMap<String, Json>, String> {
    let p = shareable(p);
    let Json::Object(mut obj) = to_json(&p)? else {
        return Err("it does not serialize to an object".into());
    };
    obj.remove("name");
    // **请求头按写的顺序**：JSON 对象的键是排过序的，所以写成一串 [名字, 值]
    if obj.contains_key("headers") {
        obj.insert(
            "headers".into(),
            Json::Array(
                p.headers
                    .iter()
                    .map(|h| {
                        Json::Array(vec![
                            Json::String(h.name.clone()),
                            Json::String(h.value.raw().to_string()),
                        ])
                    })
                    .collect(),
            ),
        );
    }
    let mut units: BTreeMap<String, Map<String, Json>> = PROVIDER_UNITS
        .iter()
        .map(|u| (u.to_string(), Map::new()))
        .collect();
    for (k, v) in obj {
        units
            .get_mut(provider_unit(&k))
            .expect("every unit is there")
            .insert(k, v);
    }
    Ok(units
        .into_iter()
        .map(|(k, v)| (k, Json::Object(v)))
        .collect())
}

/// 上游的单位拼回配置里的那一项。`local` 是本机同名的那一家：它是 ChatGPT 账号、拼出来
/// 的也是的话，登录和账户 ID 请求头取它的
pub(crate) fn provider_mapping(
    id: &ItemId,
    units: &BTreeMap<String, Json>,
    local: Option<&Provider>,
) -> Result<(Provider, Mapping), ModelError> {
    let mut m = Mapping::new();
    m.insert(key("name"), Yaml::String(id.name.clone()));
    for (u, v) in units {
        let Json::Object(o) = v else {
            return Err(shape(id, format!("unit `{u}` is not an object")));
        };
        for (k, v) in o {
            let value = if k == "headers" {
                headers_yaml(v).map_err(|e| shape(id, e))?
            } else {
                to_yaml(v)
            };
            m.insert(key(k), value);
        }
    }
    let (mut p, _) = typed_mapping::<Provider>(Yaml::Mapping(m)).map_err(|e| shape(id, e))?;
    if let Some(local) = local
        && is_chatgpt(&p)
        && is_chatgpt(local)
    {
        p.oauth = local.oauth.clone();
        if let Some(h) = local.headers.get(ACCOUNT_HEADER) {
            let mut all: Vec<tw_config::Header> = p
                .headers
                .iter()
                .filter(|x| !x.name.eq_ignore_ascii_case(ACCOUNT_HEADER))
                .cloned()
                .collect();
            all.push(h.clone());
            p.headers = tw_config::Headers::new(all);
        }
    }
    let m = match serde_yaml_ng::to_value(&p) {
        Ok(Yaml::Mapping(m)) => m,
        _ => return Err(shape(id, "it does not serialize to a mapping")),
    };
    Ok((p, m))
}

/// `[[名字, 值], …]` → 按这个顺序的 YAML 映射
fn headers_yaml(v: &Json) -> Result<Yaml, String> {
    let Json::Array(pairs) = v else {
        return Err("headers is not a list of pairs".into());
    };
    let mut m = Mapping::new();
    for p in pairs {
        match p.as_array().map(Vec::as_slice) {
            Some([Json::String(n), Json::String(v)]) => {
                m.insert(key(n), Yaml::String(v.clone()));
            }
            _ => return Err("headers is not a list of pairs".into()),
        }
    }
    Ok(Yaml::Mapping(m))
}

// ─────────────────────────────────────────────────────────── 别的有名字的

/// 一个只有 `all` 的项目拼回配置里的那一项（名字键叫 `name_key`），按类型 `T` 核过、
/// 写成它的字段顺序
pub(crate) fn whole_mapping<T>(
    id: &ItemId,
    name: &str,
    units: &BTreeMap<String, Json>,
) -> Result<(T, Mapping), ModelError>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let mut m = Mapping::new();
    m.insert(key("name"), Yaml::String(name.to_string()));
    match units.get(unit::ALL) {
        Some(Json::Object(o)) => {
            for (k, v) in o {
                m.insert(key(k), to_yaml(v));
            }
        }
        Some(_) => return Err(shape(id, "unit `all` is not an object")),
        None => return Err(shape(id, "unit `all` is missing")),
    }
    typed_mapping::<T>(Yaml::Mapping(m)).map_err(|e| shape(id, e))
}

/// 别名的模型列表
pub(crate) fn alias_models(
    id: &ItemId,
    units: &BTreeMap<String, Json>,
) -> Result<Vec<String>, ModelError> {
    match units.get(unit::ALL) {
        Some(Json::Array(a)) => a
            .iter()
            .map(|m| {
                m.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| shape(id, "a model is not a string"))
            })
            .collect(),
        _ => Err(shape(id, "unit `all` is not a list of models")),
    }
}

/// 顺序项目里的名字
pub(crate) fn order_names(units: &BTreeMap<String, Json>) -> Vec<String> {
    match units.get(unit::ALL) {
        Some(Json::Array(a)) => a
            .iter()
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

// ─────────────────────────────────────────────────────────── 插件

/// 一个插件的单位，和带着走的文件。
///
/// **比较的是批准过的那一份**：文件被人改了、还没批准时，脚本那个单位按底稿算 —— 没批准的
/// 改动不该借导出或同步跑到别的设备上，在那里被当成批准过的。底稿和配置里的哈希都对不上时
/// 按文件算（导入时这样的插件是错误）
fn plugin_units(
    entry: &tw_config::Plugin,
    files: Option<&PluginFiles>,
    codec: &dyn PluginCodec,
) -> (BTreeMap<String, Json>, Option<PluginFiles>) {
    let mut units = BTreeMap::new();
    units.insert(unit::ENABLED.to_string(), Json::Bool(entry.enabled));
    let Some(files) = files else {
        return (units, None);
    };
    let source = approved_source(entry, files);
    let text = String::from_utf8_lossy(source).into_owned();
    let (code, data) = match codec.split(&text) {
        Some((code, data)) => (code, Some(data)),
        None => (text, None),
    };
    let shipped = codec
        .shipped(&entry.id)
        .map(|s| codec.split(&s).map_or(s, |(c, _)| c));
    // 随 Core 发的默认插件、代码没动过：脚本由各设备自己的 Core 给，不比较
    if shipped.as_deref() != Some(code.as_str()) {
        units.insert(unit::SCRIPT.to_string(), Json::String(code));
    }
    if let Some(d) = data {
        units.insert(unit::SETTINGS.to_string(), d);
    }
    // 带着走的是批准过的那一份：和单位说的是同一份代码
    let carried = PluginFiles {
        script: source.to_vec(),
        approved: Some(source.to_vec()),
    };
    (units, Some(carried))
}

/// 批准过的那一份：哈希对得上配置的脚本或底稿。都对不上是脚本
pub(crate) fn approved_source<'a>(entry: &tw_config::Plugin, files: &'a PluginFiles) -> &'a [u8] {
    if sha256_hex(&files.script) == entry.sha256 {
        return &files.script;
    }
    match &files.approved {
        Some(a) if sha256_hex(a) == entry.sha256 => a,
        _ => &files.script,
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

// ─────────────────────────────────────────────────────────── 全局设置

/// 每一节的单位：等于默认值的字段不在里面
/// 一节全局设置的单位
type Units = BTreeMap<String, Json>;

fn global_units(cfg: &Config) -> Result<Vec<(&'static str, Units)>, String> {
    let mut out = Vec::new();
    out.push((
        global::FAILOVER,
        fields(global::FAILOVER, &to_json(&cfg.failover)?),
    ));
    let mut sec = BTreeMap::new();
    if let Json::Object(o) = to_json(&cfg.security)? {
        for (guard, v) in o {
            let Json::Object(fields) = v else { continue };
            for (f, v) in fields {
                if f == "custom" {
                    continue;
                }
                match (f.as_str(), v) {
                    ("actions", Json::Object(actions)) => {
                        for (rule, a) in actions {
                            sec.insert(format!("{}.{guard}.actions.{rule}", global::SECURITY), a);
                        }
                    }
                    (_, v) => {
                        sec.insert(format!("{}.{guard}.{f}", global::SECURITY), v);
                    }
                }
            }
        }
    }
    out.push((global::SECURITY, sec));
    let scalar = |name: &str, v: Option<&String>| -> BTreeMap<String, Json> {
        v.map(|v| BTreeMap::from([(name.to_string(), Json::String(v.clone()))]))
            .unwrap_or_default()
    };
    out.push((
        global::DEFAULT_ROUTE,
        scalar(global::DEFAULT_ROUTE, cfg.default_route.as_ref()),
    ));
    out.push((
        global::DEFAULT_KEY,
        scalar(global::DEFAULT_KEY, cfg.default_key.as_ref()),
    ));
    out.push((
        global::CLIENT_PROBES,
        fields(global::CLIENT_PROBES, &to_json(&cfg.client_probes)?),
    ));
    let mut pricing = to_json(&cfg.pricing)?;
    if let Json::Object(o) = &mut pricing {
        o.remove("sheets");
    }
    out.push((global::PRICING, fields(global::PRICING, &pricing)));
    for (_, units) in out.iter_mut() {
        units.retain(|path, v| !is_global_default(path, v));
    }
    Ok(out)
}

fn fields(section: &str, v: &Json) -> BTreeMap<String, Json> {
    match v {
        Json::Object(o) => o
            .iter()
            .map(|(k, v)| (format!("{section}.{k}"), v.clone()))
            .collect(),
        _ => BTreeMap::new(),
    }
}

/// 全局设置的一个单位按路径拆开：节名，和节里的那几步
pub(crate) fn global_path(path: &str) -> Vec<String> {
    let limit = if path.starts_with("security.") { 4 } else { 2 };
    path.splitn(limit, '.').map(str::to_string).collect()
}

/// 一节的默认值写成 JSON，和读进它的类型。节名不认识是 None
fn section_default(section: &str) -> Option<Json> {
    match section {
        global::FAILOVER => to_json(&tw_config::Failover::default()).ok(),
        global::SECURITY => to_json(&tw_config::Security::default()).ok(),
        global::CLIENT_PROBES => to_json(&tw_config::ClientProbes::default()).ok(),
        global::PRICING => to_json(&tw_pricing_default()).ok(),
        _ => None,
    }
}

fn tw_pricing_default() -> Json {
    let mut d = to_json(&tw_pricing::PricingConfig::default()).unwrap_or(Json::Null);
    if let Json::Object(o) = &mut d {
        o.remove("sheets");
    }
    d
}

/// 一节的 JSON 读进它的类型，再写出来：默认值去掉。读不进去是 Err
pub(crate) fn section_normalized(section: &str, v: &Json) -> Result<Json, String> {
    fn round<T: serde::de::DeserializeOwned + serde::Serialize>(v: &Json) -> Result<Json, String> {
        let t: T = serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
        to_json(&t)
    }
    match section {
        global::FAILOVER => round::<tw_config::Failover>(v),
        global::SECURITY => round::<tw_config::Security>(v),
        global::CLIENT_PROBES => round::<tw_config::ClientProbes>(v),
        global::PRICING => {
            let mut out = round::<tw_pricing::PricingConfig>(v)?;
            if let Json::Object(o) = &mut out {
                o.remove("sheets");
            }
            Ok(out)
        }
        other => Err(format!("there is no section `{other}`")),
    }
}

/// 全局设置的一个单位是不是等于默认值。**按类型判断**：把它写进默认的那一节、读进类型
/// 再写出来，和默认的那一节一样就是默认值 —— 序列化时省掉的默认值也认得出
pub(crate) fn is_global_default(path: &str, v: &Json) -> bool {
    let parts = global_path(path);
    match parts.as_slice() {
        [s] if s == global::DEFAULT_ROUTE => v.as_str() == Some(tw_engine::DEFAULT_ROUTE),
        [s] if s == global::DEFAULT_KEY => v.as_str() == Some(tw_config::DEFAULT_KEY),
        [section, rest @ ..] if !rest.is_empty() => {
            let Some(default) = section_default(section) else {
                return false;
            };
            let mut with = default.clone();
            set_path(&mut with, rest, v.clone());
            match section_normalized(section, &with) {
                Ok(n) => n == default,
                Err(_) => false,
            }
        }
        _ => false,
    }
}

/// 在一棵 JSON 里按路径写一个值，缺的对象补出来
pub(crate) fn set_path(root: &mut Json, path: &[String], v: Json) {
    let mut cur = root;
    for (i, p) in path.iter().enumerate() {
        if !cur.is_object() {
            *cur = Json::Object(Default::default());
        }
        let o = cur.as_object_mut().expect("just made it an object");
        if i + 1 == path.len() {
            o.insert(p.clone(), v);
            return;
        }
        cur = o
            .entry(p.clone())
            .or_insert_with(|| Json::Object(Default::default()));
    }
}

// ─────────────────────────────────────────────────────────── 引用

pub(crate) fn relink(items: &mut BTreeMap<ItemId, Item>) {
    let names = |kind: ItemKind| -> BTreeSet<String> {
        items
            .keys()
            .filter(|id| id.kind == kind)
            .map(|id| id.name.clone())
            .collect()
    };
    let groups = names(ItemKind::Group);
    let aliases = names(ItemKind::Alias);
    for (id, item) in items.iter_mut() {
        item.refs = refs_of(id, &item.units, &groups, &aliases);
    }
}

fn refs_of(
    id: &ItemId,
    units: &BTreeMap<String, Json>,
    groups: &BTreeSet<String>,
    aliases: &BTreeSet<String>,
) -> BTreeSet<ItemId> {
    let mut out = BTreeSet::new();
    let s = |v: Option<&Json>| v.and_then(Json::as_str).map(str::to_string);
    let all = units.get(unit::ALL);
    let get = |v: Option<&Json>, k: &str| v.and_then(|v| v.get(k)).cloned();
    match id.kind {
        ItemKind::Provider => {
            if let Some(px) = s(units.get(unit::CONN).and_then(|c| c.get("proxy")))
                && px != tw_config::DIRECT
                && px != tw_config::SYSTEM
            {
                out.insert(ItemId::new(ItemKind::Proxy, px));
            }
            if let Some(sheet) = s(units.get(unit::BILLING).and_then(|c| c.get("pricing"))) {
                out.insert(ItemId::new(ItemKind::PriceSheet, sheet));
            }
        }
        ItemKind::Key => {
            if let Some(r) = s(get(all, "route").as_ref()) {
                out.insert(ItemId::new(ItemKind::Route, r));
            }
            for m in get(all, "allow")
                .and_then(|a| a.as_array().cloned())
                .unwrap_or_default()
            {
                if let Some(m) = m.as_str()
                    && aliases.contains(m)
                {
                    out.insert(ItemId::new(ItemKind::Alias, m));
                }
            }
        }
        ItemKind::Group => {
            for m in get(all, "providers")
                .and_then(|a| a.as_array().cloned())
                .unwrap_or_default()
            {
                let name = m.as_str().map(str::to_string).or_else(|| s(m.get("name")));
                if let Some(n) = name {
                    out.insert(ItemId::new(ItemKind::Provider, n));
                }
            }
            if let Some(sel) = s(get(all, "selected").as_ref()) {
                out.insert(ItemId::new(ItemKind::Provider, sel));
            }
        }
        ItemKind::Route => {
            for rule in get(all, "rules")
                .and_then(|a| a.as_array().cloned())
                .unwrap_or_default()
            {
                match rule.get("to") {
                    Some(Json::String(n)) if n == tw_engine::ALL_UPSTREAMS => {}
                    Some(Json::String(n)) if groups.contains(n) => {
                        out.insert(ItemId::new(ItemKind::Group, n));
                    }
                    Some(Json::String(n)) => {
                        out.insert(ItemId::new(ItemKind::Provider, n));
                    }
                    Some(Json::Array(pinned)) => {
                        for p in pinned {
                            if let Some(n) = s(p.get("provider")) {
                                out.insert(ItemId::new(ItemKind::Provider, n));
                            }
                        }
                    }
                    _ => {}
                }
                let when = rule.get("when");
                let phase_two = when.and_then(|w| w.get("provider_would_be"));
                match phase_two {
                    Some(Json::String(n)) => {
                        out.insert(ItemId::new(ItemKind::Provider, n));
                    }
                    Some(Json::Array(list)) => {
                        for n in list.iter().filter_map(Json::as_str) {
                            out.insert(ItemId::new(ItemKind::Provider, n));
                        }
                    }
                    _ => {}
                }
                if let Some(k) = s(when.and_then(|w| w.get("client"))) {
                    out.insert(ItemId::new(ItemKind::Key, k));
                }
                if let Some(m) = s(when.and_then(|w| w.get("model")))
                    && aliases.contains(&m)
                {
                    out.insert(ItemId::new(ItemKind::Alias, m));
                }
                if phase_two.is_none()
                    && let Some(m) = s(rule.get("set").and_then(|w| w.get("model")))
                    && aliases.contains(&m)
                {
                    out.insert(ItemId::new(ItemKind::Alias, m));
                }
            }
        }
        ItemKind::Global => {
            if let Some(r) = s(units.get(global::DEFAULT_ROUTE)) {
                out.insert(ItemId::new(ItemKind::Route, r));
            }
            if let Some(k) = s(units.get(global::DEFAULT_KEY)) {
                out.insert(ItemId::new(ItemKind::Key, k));
            }
        }
        _ => {}
    }
    out
}

/// 自定义安全规则的项目名拆成分项和规则名
pub(crate) fn rule_name(name: &str) -> Option<(&str, &str)> {
    let (guard, rule) = name.split_once('/')?;
    GUARDS.contains(&guard).then_some((guard, rule))
}
