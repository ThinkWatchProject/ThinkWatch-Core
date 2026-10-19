//! 把目标状态落回配置文本：[`materialize`]。
//!
//! # 两条路
//!
//! 先**按项目改原文**（`tw_config::edit`）：删掉目标里没有的，改掉变了的，加上新的，再排
//! 顺序、改全局设置的字段。没变的项目一个字节都不动，旁边的注释也留着。改完读回来拆一遍，
//! 和目标对得上才算数。
//!
//! 对不上（手写成了行内写法、锚点这类按项目改不动的）就**整份重写**：本机那几节原样留着，
//! 其余照目标生成。注释没了，但内容一定对。两条路都对不上就报错，什么都不写。

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;
use serde_yaml_ng::{Mapping, Value as Yaml};
use tw_config::edit::{self, Section};
use tw_config::{Config, Plugin};
use tw_yaml::Step;

use super::items::{
    alias_models, approved_source, global_path, order_names, provider_mapping, rule_name,
    sha256_hex, whole_mapping,
};
use super::json::{key, to_yaml, unflatten};
use super::{
    Item, ItemId, ItemKind, LOCAL_SECTIONS, ModelError, PluginCodec, Snapshot, codec, global,
    normalized, parse, snapshot_with, unit,
};
use crate::{Pack, PluginFiles};

const CLIENTS: Section = Section {
    path: &["clients"],
    what: "gateway key",
    key: "name",
};
const REDACT_RULES: Section = Section {
    path: &["security", "redact", "custom"],
    what: "custom rule",
    key: "name",
};
const TOOL_RULES: Section = Section {
    path: &["security", "inspect_tools", "custom"],
    what: "custom rule",
    key: "name",
};
const CONTENT_RULES: Section = Section {
    path: &["security", "content", "custom"],
    what: "custom rule",
    key: "name",
};

/// 没有批准过的插件在配置里写的哈希：空文件的 SHA-256。哪个插件文件都对不上它，插件就
/// 停在「文件变了、待批准」
const NOT_APPROVED: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// 以 `local` 为底，落成 `target` 那个样子：本机设置原样保留，其余换成 `target`；没变的
/// 项目保留原文里的注释与顺序。用进程里装的插件拆法
pub fn materialize(local: &Pack, target: &Snapshot) -> Result<Pack, ModelError> {
    materialize_with(local, target, codec())
}

/// 同 [`materialize`]，插件拆法由调用方给。
///
/// 插件：
///
/// - 代码没变的（包括默认插件）在本机的文件上改数据，字节尽量不动；代码变了的、新来的用
///   目标里带着的原文件（[`Item::files`]），没有就按单位拼出源码；默认插件在本机没有时
///   用这一份 Core 自带的那一份，也没有就不加
/// - **改得了工具调用的插件，代码在本机没批准过（新来的、代码变了的），或者在本机原来停用着、
///   落下去是开着的，就不批准**：文件写下去，配置里的哈希是一个谁都对不上的，插件停在
///   「待批准」，要在本机点过头（确认过的那条路）才跑。和插件页上一样：装上、打开、改代码都要
///   点头
/// - 别的照目标批准：哈希就是写下去的文件的
pub fn materialize_with(
    local: &Pack,
    target: &Snapshot,
    codec: &dyn PluginCodec,
) -> Result<Pack, ModelError> {
    land(local, target, codec, true)
}

/// 同 [`materialize_with`]，**插件照目标批准，不管它改不改得了工具调用**。只给生成配置包用
/// （导出）：落的是包里的内容，不是这台设备要跑的东西
pub(crate) fn materialize_for_export(
    local: &Pack,
    target: &Snapshot,
    codec: &dyn PluginCodec,
) -> Result<Pack, ModelError> {
    land(local, target, codec, false)
}

fn land(
    local: &Pack,
    target: &Snapshot,
    codec: &dyn PluginCodec,
    gate: bool,
) -> Result<Pack, ModelError> {
    let cfg = parse(&local.config_yaml)?;
    let current = snapshot_with(local, codec)?;
    let plugins = plugin_outcomes(local, &cfg, &current, target, codec, gate)?;
    let want = complete_orders(normalized(target), &cfg, &plugins);
    let prefs = target
        .get(&ItemId::new(ItemKind::ClientPrefs, ""))
        .map(|i| unflatten(&i.units));
    let files: BTreeMap<String, PluginFiles> = plugins
        .iter()
        .map(|(id, o)| (id.clone(), o.files.clone()))
        .collect();
    let (files, prefs) = (&files, &prefs);
    let finish = |text: String| Pack {
        manifest: local.manifest.clone(),
        config_yaml: text,
        plugins: files.clone(),
        client_prefs: prefs.clone(),
    };

    let edited = by_edits(&local.config_yaml, &cfg, &current, target, &want, &plugins)
        .map(finish)
        .ok()
        .filter(|p| matches(p, &want, codec));
    if let Some(p) = edited {
        return Ok(p);
    }
    let rebuilt = finish(rebuild(&local.config_yaml, &cfg, target, &want, &plugins)?);
    if matches(&rebuilt, &want, codec) {
        return Ok(rebuilt);
    }
    Err(ModelError::Verify {
        detail: first_difference(&rebuilt, &want, codec),
    })
}

/// 落下去的配置读回来拆一遍，和目标一样吗。**插件的脚本和设置不比**：待批准的插件读回来
/// 按本机原来批准的那一份算，那正是要的样子；它们的文件由这里直接写
fn matches(p: &Pack, want: &Snapshot, codec: &dyn PluginCodec) -> bool {
    match snapshot_with(p, codec) {
        Ok(got) => comparable(&normalized(&got)) == comparable(want),
        Err(_) => false,
    }
}

fn comparable(s: &Snapshot) -> Snapshot {
    let mut s = s.clone();
    for (id, item) in s.items.iter_mut() {
        if id.kind == ItemKind::Plugin {
            item.units.retain(|u, _| u == unit::ENABLED);
        }
    }
    s
}

fn first_difference(p: &Pack, want: &Snapshot, codec: &dyn PluginCodec) -> String {
    let got = match snapshot_with(p, codec) {
        Ok(s) => comparable(&normalized(&s)),
        Err(e) => return e.to_string(),
    };
    let want = comparable(want);
    for (id, w) in &want.items {
        match got.items.get(id) {
            None => return format!("{id} is missing"),
            Some(g) if g.units != w.units => return format!("{id} differs"),
            _ => {}
        }
    }
    match got.items.keys().find(|id| !want.items.contains_key(id)) {
        Some(id) => format!("{id} is extra"),
        None => "the order differs".into(),
    }
}

/// 落下去之后各种项目的先后：本机原来就有的按原来的先后、新的接在后面（按名字），再照
/// 目标里的顺序项目排 —— 顺序项目里没列出来的接在列出来的后面。目标里的顺序项目缺了谁、
/// 多了谁，落下去之后读回来都是这个样子，比较时按它
fn complete_orders(
    mut want: Snapshot,
    cfg: &Config,
    plugins: &BTreeMap<String, PluginOut>,
) -> Snapshot {
    for (kind, before) in [
        (
            ItemKind::Provider,
            cfg.providers
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
        ),
        (
            ItemKind::Route,
            cfg.routes.iter().map(|r| r.name.clone()).collect(),
        ),
        (
            ItemKind::Plugin,
            cfg.plugins.iter().map(|p| p.id.clone()).collect(),
        ),
    ] {
        let order_id = ItemId::new(kind.order().expect("these kinds have an order"), "");
        let present = |n: &String| {
            want.items.contains_key(&ItemId::new(kind, n.clone()))
                && (kind != ItemKind::Plugin || plugins.contains_key(n))
        };
        let mut all: Vec<String> = before.into_iter().filter(|n| present(n)).collect();
        for (id, _) in want.of_kind(kind) {
            if !all.contains(&id.name) && present(&id.name) {
                all.push(id.name.clone());
            }
        }
        let listed = want
            .get(&order_id)
            .map(|o| order_names(&o.units))
            .unwrap_or_default();
        let fin = arrange(&all, &listed);
        if fin.is_empty() {
            want.items.remove(&order_id);
        } else {
            want.items.insert(
                order_id,
                Item {
                    units: BTreeMap::from([(
                        unit::ALL.to_string(),
                        Json::Array(fin.into_iter().map(Json::String).collect()),
                    )]),
                    ..Default::default()
                },
            );
        }
    }
    want.items
        .retain(|id, _| id.kind != ItemKind::Plugin || plugins.contains_key(&id.name));
    want
}

// ─────────────────────────────────────────────────────────── 插件

struct PluginOut {
    entry: Plugin,
    files: PluginFiles,
    /// 配置里那一条和本机的不一样（或者是新的）
    changed: bool,
}

fn plugin_outcomes(
    local: &Pack,
    cfg: &Config,
    current: &Snapshot,
    target: &Snapshot,
    codec: &dyn PluginCodec,
    gate: bool,
) -> Result<BTreeMap<String, PluginOut>, ModelError> {
    let mut out = BTreeMap::new();
    for (id, t) in target.of_kind(ItemKind::Plugin) {
        let name = id.name.as_str();
        let le = cfg.plugins.iter().find(|p| p.id == name);
        let lf = local.plugins.get(name);
        let li = current.items.get(id);
        if let (Some(le), Some(lf), Some(li)) = (le, lf, li)
            && li.units == t.units
        {
            out.insert(
                name.to_string(),
                PluginOut {
                    entry: le.clone(),
                    files: lf.clone(),
                    changed: false,
                },
            );
            continue;
        }
        let enabled = t
            .units
            .get(unit::ENABLED)
            .and_then(Json::as_bool)
            .unwrap_or(true);
        let t_script = t.units.get(unit::SCRIPT);
        let local_approved = match (le, lf) {
            (Some(le), Some(lf)) => {
                let a = approved_source(le, lf);
                (sha256_hex(a) == le.sha256).then(|| a.to_vec())
            }
            _ => None,
        };
        let same_code = li.is_some_and(|li| li.units.get(unit::SCRIPT) == t_script) && lf.is_some();
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let base: Option<String> = if same_code {
            lf.map(|lf| text(le.map_or(&lf.script[..], |le| approved_source(le, lf))))
        } else if let Some(f) = t.files.as_ref().filter(|f| {
            let src = text(&f.script);
            let code = codec.split(&src).map_or(src, |(c, _)| c);
            // 默认插件没有脚本这个单位：带着的就是它在那一边的文件
            t_script.is_none_or(|s| s.as_str() == Some(code.as_str()))
        }) {
            Some(text(&f.script))
        } else if let Some(s) = t_script.and_then(Json::as_str) {
            Some(s.to_string())
        } else if let (Some(le), Some(lf)) = (le, lf) {
            // 默认插件：本机的那一份
            Some(text(approved_source(le, lf)))
        } else {
            codec.shipped(name)
        };
        let Some(base) = base else {
            // 默认插件，而这一份 Core 没有它：不加
            continue;
        };
        let source = match t.units.get(unit::SETTINGS) {
            Some(d) => codec.join(&base, d).ok_or_else(|| ModelError::Shape {
                item: id.to_string(),
                detail: "its settings do not fit its script".into(),
            })?,
            None => base,
        };
        // 本机批准过这份代码；打开它也是本机的人打开的
        let approved_here = same_code && local_approved.is_some();
        let turned_on = enabled && !le.is_some_and(|le| le.enabled);
        let gated = gate && codec.needs_approval(&source) && (!approved_here || turned_on);
        let pending_edit = match (le, lf) {
            (Some(le), Some(lf)) => sha256_hex(&lf.script) != le.sha256,
            _ => false,
        };
        let (files, sha256) = if gated {
            // 哈希写成一个谁都对不上的：插件停在「文件变了、待批准」，批准要走确认过的那条路。
            // 本机原来批准的底稿留着，界面拿它比出改了什么
            (
                PluginFiles {
                    script: source.into_bytes(),
                    approved: local_approved.clone(),
                },
                NOT_APPROVED.to_string(),
            )
        } else if same_code && pending_edit {
            // 本机有一处还没批准的改动：留着它，底稿和哈希换成新的
            let sha = sha256_hex(source.as_bytes());
            (
                PluginFiles {
                    script: lf.map(|f| f.script.clone()).unwrap_or_default(),
                    approved: Some(source.into_bytes()),
                },
                sha,
            )
        } else {
            let sha = sha256_hex(source.as_bytes());
            let bytes = source.into_bytes();
            (
                PluginFiles {
                    script: bytes.clone(),
                    approved: Some(bytes),
                },
                sha,
            )
        };
        let entry = Plugin {
            id: name.to_string(),
            file: Plugin::file_for(name),
            sha256,
            enabled,
        };
        let changed = le != Some(&entry);
        out.insert(
            name.to_string(),
            PluginOut {
                entry,
                files,
                changed,
            },
        );
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────── 按项目改

fn section_of(id: &ItemId) -> Option<(Section, String)> {
    let s = match id.kind {
        ItemKind::Provider => edit::PROVIDERS,
        ItemKind::Proxy => edit::PROXIES,
        ItemKind::Key => CLIENTS,
        ItemKind::Group => edit::GROUPS,
        ItemKind::Route => edit::ROUTES,
        ItemKind::PriceSheet => edit::PRICE_SHEETS,
        ItemKind::Plugin => edit::PLUGINS,
        ItemKind::SecurityRule => {
            let (guard, rule) = rule_name(&id.name)?;
            let s = match guard {
                "redact" => REDACT_RULES,
                "inspect_tools" => TOOL_RULES,
                _ => CONTENT_RULES,
            };
            return Some((s, rule.to_string()));
        }
        _ => return None,
    };
    Some((s, id.name.clone()))
}

/// 一项在配置里的样子（名字键在里面）
fn mapping_of(
    id: &ItemId,
    item: &Item,
    cfg: &Config,
    plugins: &BTreeMap<String, PluginOut>,
) -> Result<Mapping, ModelError> {
    let m = match id.kind {
        ItemKind::Provider => {
            let local = cfg.providers.iter().find(|p| p.name == id.name);
            provider_mapping(id, &item.units, local)?.1
        }
        ItemKind::Proxy => whole_mapping::<tw_config::Proxy>(id, &id.name, &item.units)?.1,
        ItemKind::Key => whole_mapping::<tw_config::Client>(id, &id.name, &item.units)?.1,
        ItemKind::Group => whole_mapping::<tw_engine::Group>(id, &id.name, &item.units)?.1,
        ItemKind::Route => whole_mapping::<tw_engine::RouteSet>(id, &id.name, &item.units)?.1,
        ItemKind::PriceSheet => whole_mapping::<tw_pricing::SheetDef>(id, &id.name, &item.units)?.1,
        ItemKind::SecurityRule => {
            let Some((guard, rule)) = rule_name(&id.name) else {
                return Err(ModelError::Shape {
                    item: id.to_string(),
                    detail: "the name is not <guard>/<rule>".into(),
                });
            };
            match guard {
                "redact" => whole_mapping::<tw_config::CustomRedactRule>(id, rule, &item.units)?.1,
                "inspect_tools" => {
                    whole_mapping::<tw_config::CustomToolRule>(id, rule, &item.units)?.1
                }
                _ => whole_mapping::<tw_config::CustomContentRule>(id, rule, &item.units)?.1,
            }
        }
        ItemKind::Plugin => {
            let Some(o) = plugins.get(&id.name) else {
                return Err(ModelError::Shape {
                    item: id.to_string(),
                    detail: "there is no script for it".into(),
                });
            };
            match serde_yaml_ng::to_value(&o.entry) {
                Ok(Yaml::Mapping(m)) => m,
                _ => {
                    return Err(ModelError::Shape {
                        item: id.to_string(),
                        detail: "it does not serialize to a mapping".into(),
                    });
                }
            }
        }
        _ => {
            return Err(ModelError::Shape {
                item: id.to_string(),
                detail: "it is not a list entry".into(),
            });
        }
    };
    Ok(m)
}

fn by_edits(
    text: &str,
    cfg: &Config,
    current: &Snapshot,
    target: &Snapshot,
    want: &Snapshot,
    plugins: &BTreeMap<String, PluginOut>,
) -> Result<String, ModelError> {
    let mut text = text.to_string();
    let wanted =
        |id: &ItemId| -> bool { id.kind != ItemKind::Plugin || plugins.contains_key(&id.name) };

    // 删：先删引用别人的，再删被引用的（文本上无所谓，读起来顺）
    for id in current.items.keys().rev() {
        if !id.kind.is_named() || (target.items.contains_key(id) && wanted(id)) {
            continue;
        }
        text = remove(&text, id)?;
    }
    // 改、加
    for (id, t) in &target.items {
        if !id.kind.is_named() || !wanted(id) {
            continue;
        }
        let existing = current.items.get(id);
        let same = match id.kind {
            ItemKind::Plugin => existing.is_some() && !plugins[&id.name].changed,
            _ => existing.is_some_and(|c| c.units == t.units),
        };
        if same {
            continue;
        }
        text = upsert(&text, id, t, existing.is_some(), cfg, plugins)?;
    }
    // 顺序
    for (kind, section) in [
        (ItemKind::Provider, edit::PROVIDERS),
        (ItemKind::Route, edit::ROUTES),
        (ItemKind::Plugin, edit::PLUGINS),
    ] {
        let Some(order) = kind.order().and_then(|o| target.get(&ItemId::new(o, ""))) else {
            continue;
        };
        let doc = edit::parse(&text)?;
        let now = names_in(&doc, section);
        let desired = arrange(&now, &order_names(&order.units));
        if desired != now {
            text = edit::reorder(&text, section, &desired)?;
        }
    }
    // 全局设置：逐个字段
    for section in global::ALL {
        let id = ItemId::new(ItemKind::Global, *section);
        let empty = BTreeMap::new();
        let l = current.get(&id).map_or(&empty, |i| &i.units);
        let t = want.get(&id).map_or(&empty, |i| &i.units);
        if l == t {
            continue;
        }
        let paths: BTreeSet<&String> = l.keys().chain(t.keys()).collect();
        for p in paths {
            let tv = t.get(p);
            if l.get(p) == tv {
                continue;
            }
            let steps: Vec<Step> = global_path(p).iter().map(Step::key).collect();
            text = edit::set(&text, &steps, tv.map(to_yaml).as_ref())?;
        }
    }
    Ok(text)
}

fn remove(text: &str, id: &ItemId) -> Result<String, ModelError> {
    if id.kind == ItemKind::Alias {
        return Ok(edit::remove_alias(text, &id.name)?);
    }
    let Some((section, name)) = section_of(id) else {
        return Ok(text.to_string());
    };
    Ok(edit::remove(text, section, &name)?)
}

fn upsert(
    text: &str,
    id: &ItemId,
    item: &Item,
    exists: bool,
    cfg: &Config,
    plugins: &BTreeMap<String, PluginOut>,
) -> Result<String, ModelError> {
    if id.kind == ItemKind::Alias {
        let alias = tw_config::Alias {
            name: id.name.clone(),
            models: alias_models(id, &item.units)?,
        };
        let current = exists.then_some(id.name.as_str());
        return Ok(edit::upsert_alias(text, current, &alias)?);
    }
    let Some((section, name)) = section_of(id) else {
        return Ok(text.to_string());
    };
    let m = mapping_of(id, item, cfg, plugins)?;
    let current = exists.then_some(name.as_str());
    Ok(edit::upsert(text, section, current, &m)?)
}

/// 一段列表里各项的名字，按写的顺序
fn names_in(doc: &Yaml, section: Section) -> Vec<String> {
    let mut cur = doc;
    for k in section.path {
        match cur.get(*k) {
            Some(v) => cur = v,
            None => return Vec::new(),
        }
    }
    cur.as_sequence()
        .into_iter()
        .flatten()
        .filter_map(|it| {
            it.get(section.key)
                .and_then(Yaml::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// 按 `order` 排 `now`：`order` 里有的按它的先后排在前面，没有的按原来的先后接在后面
fn arrange(now: &[String], order: &[String]) -> Vec<String> {
    let present: BTreeSet<&String> = now.iter().collect();
    let mut out: Vec<String> = order
        .iter()
        .filter(|n| present.contains(n))
        .cloned()
        .collect();
    let mut seen: BTreeSet<String> = out.iter().cloned().collect();
    for n in now {
        if seen.insert(n.clone()) {
            out.push(n.clone());
        }
    }
    // 同一个名字在 order 里写了两次：只留第一次
    let mut once = BTreeSet::new();
    out.retain(|n| once.insert(n.clone()));
    out
}

// ─────────────────────────────────────────────────────────── 整份重写

fn rebuild(
    text: &str,
    cfg: &Config,
    target: &Snapshot,
    want: &Snapshot,
    plugins: &BTreeMap<String, PluginOut>,
) -> Result<String, ModelError> {
    let doc: Yaml = serde_yaml_ng::from_str(text).unwrap_or(Yaml::Null);
    let mut out = Mapping::new();
    out.insert(
        key("version"),
        doc.get("version")
            .cloned()
            .unwrap_or(Yaml::Number(tw_config::SCHEMA_VERSION.into())),
    );
    let local = |k: &str| doc.get(k).cloned();

    // 一种项目在目标里的名字：原来就有的按原来的先后，新的接在后面；有顺序项目的照它排
    let names = |kind: ItemKind, before: Vec<String>| -> Vec<String> {
        let mut all: Vec<String> = before
            .into_iter()
            .filter(|n| target.items.contains_key(&ItemId::new(kind, n.clone())))
            .collect();
        for (id, _) in target.of_kind(kind) {
            if !all.contains(&id.name) {
                all.push(id.name.clone());
            }
        }
        if kind == ItemKind::Plugin {
            all.retain(|n| plugins.contains_key(n));
        }
        match kind.order().and_then(|o| target.get(&ItemId::new(o, ""))) {
            Some(o) => arrange(&all, &order_names(&o.units)),
            None => all,
        }
    };
    let list = |kind: ItemKind, before: Vec<String>| -> Result<Vec<Yaml>, ModelError> {
        names(kind, before)
            .into_iter()
            .map(|n| {
                let id = ItemId::new(kind, n);
                let item = &target.items[&id];
                mapping_of(&id, item, cfg, plugins).map(Yaml::Mapping)
            })
            .collect()
    };

    if let Some(v) = local("listen") {
        out.insert(key("listen"), v);
    }
    let seq = |v: Vec<Yaml>| (!v.is_empty()).then_some(Yaml::Sequence(v));
    if let Some(v) = seq(list(
        ItemKind::Key,
        cfg.clients.iter().map(|c| c.name.clone()).collect(),
    )?) {
        out.insert(key("clients"), v);
    }
    if let Some(v) = seq(list(
        ItemKind::Provider,
        cfg.providers.iter().map(|p| p.name.clone()).collect(),
    )?) {
        out.insert(key("providers"), v);
    }
    if let Some(v) = seq(list(
        ItemKind::Proxy,
        cfg.proxies.iter().map(|p| p.name.clone()).collect(),
    )?) {
        out.insert(key("proxies"), v);
    }

    // 全局设置：每一节按单位拼出来，读进类型核过
    let section = |name: &str| -> Result<Json, ModelError> {
        let mut v = Json::Object(Default::default());
        if let Some(item) = want.get(&ItemId::new(ItemKind::Global, name)) {
            for (path, value) in &item.units {
                let parts = global_path(path);
                super::items::set_path(&mut v, &parts[1..], value.clone());
            }
        }
        super::items::section_normalized(name, &v).map_err(|detail| ModelError::Shape {
            item: format!("global `{name}`"),
            detail,
        })
    };
    let mut pricing = section(global::PRICING)?;
    let sheets = list(
        ItemKind::PriceSheet,
        cfg.pricing.sheets.iter().map(|s| s.name.clone()).collect(),
    )?;
    let mut pricing_yaml = match to_yaml(&pricing.take()) {
        Yaml::Mapping(m) => m,
        _ => Mapping::new(),
    };
    if !sheets.is_empty() {
        pricing_yaml.insert(key("sheets"), Yaml::Sequence(sheets));
    }
    if !pricing_yaml.is_empty() {
        out.insert(key("pricing"), Yaml::Mapping(pricing_yaml));
    }
    let probes = to_yaml(&section(global::CLIENT_PROBES)?);
    if probes.as_mapping().is_some_and(|m| !m.is_empty()) {
        out.insert(key("client_probes"), probes);
    }
    let mut security = match to_yaml(&section(global::SECURITY)?) {
        Yaml::Mapping(m) => m,
        _ => Mapping::new(),
    };
    for (guard, before) in [
        (
            "redact",
            cfg.security
                .redact
                .custom
                .iter()
                .map(|r| r.name.clone())
                .collect::<Vec<_>>(),
        ),
        (
            "inspect_tools",
            cfg.security
                .inspect_tools
                .custom
                .iter()
                .map(|r| r.name.clone())
                .collect(),
        ),
        (
            "content",
            cfg.security
                .content
                .custom
                .iter()
                .map(|r| r.name.clone())
                .collect(),
        ),
    ] {
        let prefix = format!("{guard}/");
        let before = before.into_iter().map(|n| format!("{prefix}{n}")).collect();
        let own: Vec<Yaml> = names(ItemKind::SecurityRule, before)
            .into_iter()
            .filter(|n| n.starts_with(&prefix))
            .map(|n| {
                let id = ItemId::new(ItemKind::SecurityRule, n);
                mapping_of(&id, &target.items[&id], cfg, plugins).map(Yaml::Mapping)
            })
            .collect::<Result<_, _>>()?;
        if !own.is_empty() {
            let entry = security
                .entry(key(guard))
                .or_insert_with(|| Yaml::Mapping(Mapping::new()));
            if let Yaml::Mapping(g) = entry {
                g.insert(key("custom"), Yaml::Sequence(own));
            }
        }
    }
    if !security.is_empty() {
        out.insert(key("security"), Yaml::Mapping(security));
    }
    if let Some(v) = local("retention") {
        out.insert(key("retention"), v);
    }
    let failover = to_yaml(&section(global::FAILOVER)?);
    if failover.as_mapping().is_some_and(|m| !m.is_empty()) {
        out.insert(key("failover"), failover);
    }
    // 别名表：名字 → 一个模型是字符串，几个是列表
    let mut aliases = Mapping::new();
    for n in names(
        ItemKind::Alias,
        cfg.aliases.iter().map(|a| a.name.clone()).collect(),
    ) {
        let id = ItemId::new(ItemKind::Alias, n.clone());
        let models = alias_models(&id, &target.items[&id].units)?;
        let v = match models.as_slice() {
            [one] => Yaml::String(one.clone()),
            many => Yaml::Sequence(many.iter().cloned().map(Yaml::String).collect()),
        };
        aliases.insert(key(&n), v);
    }
    if !aliases.is_empty() {
        out.insert(key("aliases"), Yaml::Mapping(aliases));
    }
    if let Some(v) = seq(list(
        ItemKind::Group,
        cfg.groups.iter().map(|g| g.name.clone()).collect(),
    )?) {
        out.insert(key("groups"), v);
    }
    if let Some(v) = seq(list(
        ItemKind::Route,
        cfg.routes.iter().map(|r| r.name.clone()).collect(),
    )?) {
        out.insert(key("routes"), v);
    }
    for name in [global::DEFAULT_ROUTE, global::DEFAULT_KEY] {
        if let Some(v) = want
            .get(&ItemId::new(ItemKind::Global, name))
            .and_then(|i| i.units.get(name))
        {
            out.insert(key(name), to_yaml(v));
        }
    }
    if let Some(v) = seq(list(
        ItemKind::Plugin,
        cfg.plugins.iter().map(|p| p.id.clone()).collect(),
    )?) {
        out.insert(key("plugins"), v);
    }
    for k in LOCAL_SECTIONS {
        if !out.contains_key(key(k))
            && let Some(v) = local(k)
        {
            out.insert(key(k), v);
        }
    }
    serde_yaml_ng::to_string(&Yaml::Mapping(out)).map_err(|e| ModelError::Verify {
        detail: e.to_string(),
    })
}
