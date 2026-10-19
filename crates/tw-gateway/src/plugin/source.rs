//! 插件文件里的数据：出错时怎么办、范围、设置的值（契约附录四）。
//!
//! **插件的 JS 文件就是它的配置所在。**这几样写在文件的 manifest 里，配置文件里只有 id、
//! 文件、批准的哈希和开关。界面改它们就是改 manifest 那一段字面量，**文件别的字节一个
//! 不动**（`tw_plugin::literal`）。这里把它接到网关和控制面的类型上，错误换成带码的那句话。
//!
//! 这些都不编译、不起运行时：manifest 是纯数据，照着源码就读得出来。

use std::collections::BTreeMap;

use tw_api::{OnError, PluginScope, SettingValue};
use tw_plugin::literal::{self, RewriteError, Values};
use tw_types::{Msg, msg};

use crate::plugin::engine::{LoadError, MAX_SOURCE, Manifest};
use crate::plugin::set::Scope;

/// 改写插件文件里的出错时怎么办、范围和设置的值。`on_error` 和 `scope` 是改完的样子，
/// `settings` 只改提到的那几个。改完和原来一样就原样返回。
///
/// 范围里的模式去掉两头的空白；空着的、太长的、带控制字符的和加载时一样不收
pub fn rewrite(
    source: &str,
    on_error: OnError,
    scope: &PluginScope,
    settings: &BTreeMap<String, SettingValue>,
) -> Result<String, Msg> {
    if source.len() > MAX_SOURCE {
        return Err(LoadError::TooLarge.msg());
    }
    let trim = |l: &[String]| l.iter().map(|x| x.trim().to_string()).collect();
    let values = Values {
        on_error: match on_error {
            OnError::Reject => tw_plugin::OnError::Reject,
            OnError::Skip => tw_plugin::OnError::Skip,
        },
        scope: tw_plugin::Scope {
            clients: trim(&scope.clients),
            models: trim(&scope.models),
            upstreams: trim(&scope.upstreams),
        },
        settings: settings
            .iter()
            .map(|(k, v)| (k.clone(), value_json(v)))
            .collect(),
    };
    literal::rewrite(source, &values).map_err(|e| rewrite_error(&e))
}

fn value_json(v: &SettingValue) -> serde_json::Value {
    match v {
        SettingValue::Bool(b) => serde_json::Value::Bool(*b),
        SettingValue::Number(f) => serde_json::Value::from(*f),
        SettingValue::String(s) => serde_json::Value::String(s.clone()),
    }
}

/// 改写不了的原因，带码
fn rewrite_error(e: &RewriteError) -> Msg {
    match e {
        RewriteError::NotData(d) => LoadError::NotData {
            message: d.message.clone(),
            line: d.line,
            column: d.column,
        }
        .msg(),
        RewriteError::UnknownSetting(key) => setting_unknown(key),
        RewriteError::SettingType { key, kind } => msg!(
            "gw.plugin.setting_type", key = key, kind = kind.as_str() =>
            "Setting `{key}` has to be a {kind}."
        ),
        RewriteError::Invalid(detail) => LoadError::Manifest(detail.clone()).msg(),
    }
}

fn setting_unknown(key: &str) -> Msg {
    msg!(
        "gw.plugin.setting_unknown", key = key =>
        "Setting `{key}` is not one the plugin declares."
    )
}

/// 两份源码是不是**只差数据**：manifest 字面量以外的字节一模一样，manifest 里除了出错时
/// 怎么办、范围和设置的值也一模一样。哪一份不是 UTF-8、读不出 manifest 都不算
pub fn same_code(a: &[u8], b: &[u8]) -> bool {
    match (std::str::from_utf8(a), std::str::from_utf8(b)) {
        (Ok(a), Ok(b)) => literal::same_code(a, b),
        _ => false,
    }
}

/// 文件里写着的出错时怎么办和范围，**不编译**。插件加载不了（运行时起不来、新版 core 不认
/// 它的写法）时，它管哪些请求、出了错怎么办照样按文件说的来 —— 不因为编不了就变成「什么都管、
/// 一律拒绝」。读不出来是 None
pub fn declared(bytes: &[u8]) -> Option<(OnError, Scope)> {
    let values = literal::find(std::str::from_utf8(bytes).ok()?)
        .ok()?
        .values()?;
    Some((
        crate::plugin::sandbox::on_error(values.on_error),
        Scope {
            clients: values.scope.clients,
            models: values.scope.models,
            upstreams: values.scope.upstreams,
        },
    ))
}

/// 文件里写着的整份 manifest，**不编译、不起沙箱**（[`tw_plugin::declared_manifest`]）：
/// 和编出来的同一套规矩，只是说不出导出了哪些钩子 —— `hooks` 一律是空的。
///
/// **只拿来显示**：一个插件都没打开时，停用的插件不编（见 [`crate::plugin::load`]），缓存里
/// 又没有它编过的那一份（core 升级过、缓存没了），列表就照这一份说它是什么。它跑不了，也不
/// 写进缓存（缓存里只放编出来的）；要跑它、按它的权限做判断，先真的编一遍。
///
/// 读不出来（不是纯数据、不合规矩、太大）是 None
pub fn display_manifest(bytes: &[u8]) -> Option<Manifest> {
    if bytes.len() > MAX_SOURCE {
        return None;
    }
    let m = tw_plugin::declared_manifest(bytes).ok()?;
    Some(crate::plugin::sandbox::manifest(&m))
}

/// 把 `from` 里**写明了的**那几样搬到 `into` 上：出错时怎么办（`on_error`）、范围（`match`），
/// 以及 `into` 还声明着、类型也没变的设置的值（`value`）。默认插件换成新版时用：用户设过的
/// 留着，新版新加的设置取新版写的值。
///
/// **只搬写明了的**：没写的是旧版出厂的样子，换成新版的 —— 0.58 发的那一版设置写的是
/// `default`，没有 `value`，搬过去就成了空值。哪一份读不出来、搬过去不合规矩，是 None
pub fn carry_over(from: &[u8], into: &str) -> Option<String> {
    let old = literal::find(std::str::from_utf8(from).ok()?).ok()?;
    let was = old.values()?;
    let new = literal::find(into).ok()?.values()?;
    let written = |key: &str| old.data.get(key).is_some_and(|d| *d != literal::Data::Null);
    let settings = new
        .settings
        .iter()
        .filter_map(|(k, v)| {
            let value = old.data.get("settings")?.get(k)?.get("value")?.to_json();
            same_type(&value, v).then(|| (k.clone(), value))
        })
        .collect();
    literal::rewrite(
        into,
        &Values {
            on_error: if written("on_error") {
                was.on_error
            } else {
                new.on_error
            },
            scope: if written("match") {
                was.scope
            } else {
                new.scope
            },
            settings,
        },
    )
    .ok()
}

fn same_type(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value::{Bool, Number, String};
    matches!(
        (a, b),
        (String(_), String(_)) | (Number(_), Number(_)) | (Bool(_), Bool(_))
    )
}

/// `m` 是 `source` 只改了数据之前的那一份编出来的 manifest：照 `source` 里写着的，换上出错时
/// 怎么办、范围和设置的值。**不编译**：只改了数据的两份，别的都一样（[`same_code`]）。
/// `source` 读不出来是 None
pub fn with_values(m: &Manifest, source: &str) -> Option<Manifest> {
    let (on_error, scope) = declared(source.as_bytes())?;
    let values = literal::find(source).ok()?.values()?;
    let mut out = m.clone();
    out.on_error = on_error;
    out.scope = scope;
    for spec in &mut out.settings {
        let (_, v) = values.settings.iter().find(|(k, _)| *k == spec.key)?;
        spec.value = v.clone();
    }
    Some(out)
}

/// 把一份源码拆成**代码**和**数据**，给配置包、同步按这两样分开比较（`tw_pack::model`）。
///
/// 数据是出错时怎么办、范围、每个设置的值；代码是完整的源码，manifest 字面量里的数据去掉，
/// 字面量按固定的样子重写（[`literal::write`]）—— 只差数据的两份源码拆出来的代码一字不差，
/// 字面量原来怎么排版都一样。代码本身还是一份能读的源码：数据都是不写时的值。读不出 manifest
/// 是 None
pub fn split_data(source: &str) -> Option<(String, serde_json::Value)> {
    let lit = literal::find(source).ok()?;
    let values = lit.values()?;
    let literal::Data::Object(fields) = &lit.data else {
        return None;
    };
    let code_fields = fields
        .iter()
        .filter(|(k, _)| k != "on_error" && k != "match")
        .map(|(k, v)| {
            let v = match (k.as_str(), v) {
                ("settings", literal::Data::Object(specs)) => literal::Data::Object(
                    specs
                        .iter()
                        .map(|(name, spec)| {
                            let spec = match spec {
                                literal::Data::Object(f) => literal::Data::Object(
                                    f.iter().filter(|(k, _)| k != "value").cloned().collect(),
                                ),
                                other => other.clone(),
                            };
                            (name.clone(), spec)
                        })
                        .collect(),
                ),
                _ => v.clone(),
            };
            (k.clone(), v)
        })
        .collect();
    let newline = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let code = format!(
        "{}{}{}",
        &source[..lit.span.start],
        literal::write(&literal::Data::Object(code_fields), "", newline),
        &source[lit.span.end..]
    );
    let data = serde_json::json!({
        "on_error": values.on_error.as_str(),
        "match": {
            "clients": values.scope.clients,
            "models": values.scope.models,
            "upstreams": values.scope.upstreams,
        },
        "settings": values.settings.into_iter().collect::<serde_json::Map<_, _>>(),
    });
    Some((code, data))
}

/// [`split_data`] 反过来：把数据写进源码，**只换 manifest 那一段**。数据和源码声明的对不上
/// （设置不在、类型不对）是 None
pub fn join_data(source: &str, data: &serde_json::Value) -> Option<String> {
    let strings = |v: Option<&serde_json::Value>| -> Option<Vec<String>> {
        match v {
            None | Some(serde_json::Value::Null) => Some(Vec::new()),
            Some(serde_json::Value::Array(a)) => {
                a.iter().map(|x| x.as_str().map(str::to_string)).collect()
            }
            _ => None,
        }
    };
    let on_error = match data.get("on_error").and_then(serde_json::Value::as_str) {
        Some(s) => tw_plugin::OnError::from_manifest(s)?,
        None => tw_plugin::OnError::Reject,
    };
    let m = data.get("match");
    let values = Values {
        on_error,
        scope: tw_plugin::Scope {
            clients: strings(m.and_then(|m| m.get("clients")))?,
            models: strings(m.and_then(|m| m.get("models")))?,
            upstreams: strings(m.and_then(|m| m.get("upstreams")))?,
        },
        settings: match data.get("settings") {
            Some(serde_json::Value::Object(o)) => {
                o.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            }
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(_) => return None,
        },
    };
    literal::rewrite(source, &values).ok()
}

/// 这份源码的插件改不改得了回答里的工具调用（权限有 `reply_tool_calls`），**不编译**，照
/// 文件里写着的 manifest 判断。读不出来的按改得了算
pub fn steers_declared(source: &[u8]) -> bool {
    display_manifest(source)
        .is_none_or(|m| m.permissions.contains(&tw_api::Permission::ReplyToolCalls))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SRC: &str = "// 外面的注释\nexport const manifest = {\n  name: \"x\",\n  api: 1,\n  permissions: [\"system\"],\n  settings: {\n    note: { type: \"string\", label: \"Note\", value: \"a\" },\n    days: { type: \"number\", label: \"Days\", value: 1 },\n  },\n};\nexport function onRequest(req) { return req; }\n";

    fn scope(models: &[&str]) -> PluginScope {
        PluginScope {
            clients: Vec::new(),
            models: models.iter().map(|s| s.to_string()).collect(),
            upstreams: Vec::new(),
        }
    }

    #[test]
    fn a_rewrite_trims_patterns_and_keeps_the_rest_of_the_file() {
        let out = rewrite(
            SRC,
            OnError::Skip,
            &scope(&["  claude-*  "]),
            &BTreeMap::from([("days".to_string(), SettingValue::Number(3.0))]),
        )
        .unwrap();
        assert!(out.starts_with("// 外面的注释\nexport const manifest = {\n"));
        assert!(out.ends_with("};\nexport function onRequest(req) { return req; }\n"));
        assert!(out.contains("models: [\"claude-*\"]"), "{out}");
        assert!(out.contains("on_error: \"skip\""), "{out}");
        assert!(out.contains("value: 3 }"), "{out}");
        assert!(same_code(SRC.as_bytes(), out.as_bytes()));
        assert_eq!(
            declared(out.as_bytes()),
            Some((
                OnError::Skip,
                Scope {
                    clients: Vec::new(),
                    models: vec!["claude-*".into()],
                    upstreams: Vec::new(),
                }
            ))
        );
    }

    #[test]
    fn rewrite_errors_have_their_codes() {
        let none = BTreeMap::new();
        let e = rewrite(
            SRC,
            OnError::Reject,
            &scope(&[]),
            &BTreeMap::from([("colour".to_string(), SettingValue::String("red".into()))]),
        )
        .unwrap_err();
        assert_eq!(
            (e.code.as_str(), e.arg("key")),
            ("gw.plugin.setting_unknown", "colour")
        );
        let e = rewrite(
            SRC,
            OnError::Reject,
            &scope(&[]),
            &BTreeMap::from([("days".to_string(), SettingValue::Bool(true))]),
        )
        .unwrap_err();
        assert_eq!(
            (e.code.as_str(), e.arg("kind")),
            ("gw.plugin.setting_type", "number")
        );
        let e = rewrite(SRC, OnError::Reject, &scope(&[&"x".repeat(201)]), &none).unwrap_err();
        assert_eq!(e.code, "gw.plugin.manifest");
        let e = rewrite(
            "export const manifest = { name: x };",
            OnError::Reject,
            &scope(&[]),
            &none,
        )
        .unwrap_err();
        assert_eq!(
            (e.code.as_str(), e.arg("line")),
            ("gw.plugin.manifest_not_data_at", "1")
        );
        let e = rewrite("const a = 1;", OnError::Reject, &scope(&[]), &none).unwrap_err();
        assert_eq!(e.code, "gw.plugin.manifest_not_data");
    }

    /// 新版保留用户设过的：出错时怎么办、范围、还在而且类型没变的设置；新版新加的设置取新版的值
    #[test]
    fn carrying_over_keeps_what_the_user_set() {
        let mine = rewrite(
            SRC,
            OnError::Skip,
            &scope(&["deepseek*"]),
            &BTreeMap::from([
                ("note".to_string(), SettingValue::String("我的".into())),
                ("days".to_string(), SettingValue::Number(9.0)),
            ]),
        )
        .unwrap();
        let newer = SRC
            .replace(
                "days: { type: \"number\", label: \"Days\", value: 1 }",
                "days: { type: \"string\", label: \"Days\", value: \"1\" },\n    loud: { type: \"boolean\", label: \"Loud\", value: true }",
            )
            .replace("return req;", "return undefined;");
        let out = carry_over(mine.as_bytes(), &newer).unwrap();
        let v = literal::find(&out).unwrap().values().unwrap();
        assert_eq!(v.on_error, tw_plugin::OnError::Skip);
        assert_eq!(v.scope.models, ["deepseek*"]);
        assert_eq!(
            v.settings,
            [
                ("note".to_string(), json!("我的")),
                ("days".to_string(), json!("1")),
                ("loud".to_string(), json!(true)),
            ]
        );
        assert!(same_code(newer.as_bytes(), out.as_bytes()));
        assert!(out.contains("return undefined;"));
        assert_eq!(carry_over(b"not a plugin", &newer), None);

        // 旧版没写的不搬：新版的出厂值留着（0.58 的设置写的是 `default`，没有 `value`）
        let old = "export const manifest = { name: \"x\", api: 1, permissions: [\"system\"], settings: { note: { type: \"string\", label: \"Note\", default: \"old\" } } };";
        let newer = rewrite(SRC, OnError::Skip, &scope(&["claude-*"]), &BTreeMap::new()).unwrap();
        let out = carry_over(old.as_bytes(), &newer).unwrap();
        assert_eq!(out, newer);
    }

    #[test]
    fn values_are_put_on_a_compiled_manifest_without_compiling() {
        let load = |s: &str| {
            crate::plugin::engine::Engine::load(&crate::plugin::fake::FakeEngine, s.as_bytes())
                .unwrap()
                .manifest()
                .clone()
        };
        let m = load(SRC);
        let out = rewrite(
            SRC,
            OnError::Skip,
            &scope(&["a*"]),
            &BTreeMap::from([("note".to_string(), SettingValue::String("b".into()))]),
        )
        .unwrap();
        let patched = with_values(&m, &out).unwrap();
        assert_eq!(patched, load(&out));
        assert_eq!(patched.on_error, OnError::Skip);
        assert_eq!(patched.settings[0].value, json!("b"));
    }

    /// 只差数据的两份源码拆出来代码一样；数据写回去就是原来的数据
    #[test]
    fn code_and_data_split_apart_and_join_back() {
        let a = SRC;
        let b = rewrite(
            SRC,
            OnError::Skip,
            &scope(&["claude-*"]),
            &BTreeMap::from([("note".to_string(), SettingValue::String("b".into()))]),
        )
        .unwrap();
        let (code_a, data_a) = split_data(a).unwrap();
        let (code_b, data_b) = split_data(&b).unwrap();
        assert_eq!(code_a, code_b, "只差数据，代码就该一样");
        assert_ne!(data_a, data_b);
        assert_eq!(data_b["on_error"], "skip");
        assert_eq!(data_b["match"]["models"], json!(["claude-*"]));
        assert_eq!(data_b["settings"]["note"], "b");
        // 代码本身读得出 manifest，数据都是不写时的值
        let (_, empty) = split_data(&code_a).unwrap();
        assert_eq!(empty["settings"]["note"], "");
        // 数据写回代码：和原来那一份数据一样
        let back = join_data(&code_a, &data_b).unwrap();
        assert_eq!(split_data(&back).unwrap().1, data_b);
        assert!(back.contains("export function onRequest(req) { return req; }"));
        // 数据写回原文件：只差数据时原样返回
        assert_eq!(join_data(a, &data_a).unwrap(), a);
        // 数据和源码对不上
        assert!(join_data(a, &json!({"settings": {"unknown": 1}})).is_none());
        assert!(split_data("no manifest here").is_none());
        assert!(steers_declared(b"no manifest here"));
        assert!(!steers_declared(SRC.as_bytes()));
    }
}
