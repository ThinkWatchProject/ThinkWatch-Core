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
}
