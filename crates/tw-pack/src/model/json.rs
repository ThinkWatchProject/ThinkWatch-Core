//! JSON 与 YAML 之间来回，和按路径摊平一棵 JSON。
//!
//! 单位的值是 JSON（规范化、键有序）；写回配置用的是 YAML 的映射（键按结构体的字段顺序）。
//! 两边的标量一一对应：字符串、整数、浮点数、布尔、空。

use serde_json::Value as Json;
use serde_yaml_ng::{Mapping, Value as Yaml};

/// 一个可序列化的值 → JSON。失败是我们自己的类型写坏了
pub(crate) fn to_json<T: serde::Serialize>(v: &T) -> Result<Json, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

/// JSON → YAML。对象的键按 JSON 里的顺序（也就是字母序）
pub(crate) fn to_yaml(v: &Json) -> Yaml {
    match v {
        Json::Null => Yaml::Null,
        Json::Bool(b) => Yaml::Bool(*b),
        Json::Number(n) => {
            if let Some(u) = n.as_u64() {
                Yaml::Number(u.into())
            } else if let Some(i) = n.as_i64() {
                Yaml::Number(i.into())
            } else {
                Yaml::Number(n.as_f64().unwrap_or(0.0).into())
            }
        }
        Json::String(s) => Yaml::String(s.clone()),
        Json::Array(a) => Yaml::Sequence(a.iter().map(to_yaml).collect()),
        Json::Object(m) => Yaml::Mapping(
            m.iter()
                .map(|(k, v)| (Yaml::String(k.clone()), to_yaml(v)))
                .collect(),
        ),
    }
}

/// 一个 JSON 对象（单位拼起来的样子）→ 某个配置类型，再写成 YAML 映射：键按结构体的
/// 字段顺序，默认值不写。读不进那个类型就是这些单位不成形
pub(crate) fn typed_mapping<T>(fields: Yaml) -> Result<(T, Mapping), String>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let typed: T = serde_yaml_ng::from_value(fields).map_err(|e| e.to_string())?;
    match serde_yaml_ng::to_value(&typed).map_err(|e| e.to_string())? {
        Yaml::Mapping(m) => Ok((typed, m)),
        _ => Err("it does not serialize to a mapping".into()),
    }
}

/// 摊平：对象一层层展开成 `a.b.c` → 值；数组和别的标量是叶子，空对象也是叶子。
/// 键里的 `~` 和 `.` 转义成 `~0`、`~1`
pub(crate) fn flatten(prefix: &str, v: &Json, out: &mut std::collections::BTreeMap<String, Json>) {
    match v {
        Json::Object(m) if !m.is_empty() => {
            for (k, v) in m {
                let k = k.replace('~', "~0").replace('.', "~1");
                let path = if prefix.is_empty() {
                    k
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&path, v, out);
            }
        }
        other => {
            out.insert(prefix.to_string(), other.clone());
        }
    }
}

/// [`flatten`] 反过来。空路径是整棵树本身
pub(crate) fn unflatten(units: &std::collections::BTreeMap<String, Json>) -> Json {
    if let Some(root) = units.get("") {
        return root.clone();
    }
    let mut root = serde_json::Map::new();
    for (path, v) in units {
        let parts: Vec<String> = path
            .split('.')
            .map(|p| p.replace("~1", ".").replace("~0", "~"))
            .collect();
        let mut cur = &mut root;
        for (i, p) in parts.iter().enumerate() {
            if i + 1 == parts.len() {
                cur.insert(p.clone(), v.clone());
            } else {
                let next = cur
                    .entry(p.clone())
                    .or_insert_with(|| Json::Object(Default::default()));
                if !next.is_object() {
                    *next = Json::Object(Default::default());
                }
                cur = next.as_object_mut().expect("just made it an object");
            }
        }
    }
    Json::Object(root)
}

/// 映射里的一个键
pub(crate) fn key(k: &str) -> Yaml {
    Yaml::String(k.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flattening_and_back_gives_the_same_tree() {
        for v in [
            json!({"language": "zh", "menubar": {"show": "tokens", "rows": 2}, "list": [1, 2], "empty": {}}),
            json!({"a.b": {"c~d": true}}),
            json!("just a string"),
        ] {
            let mut flat = Default::default();
            flatten("", &v, &mut flat);
            assert_eq!(unflatten(&flat), v, "{flat:?}");
        }
    }

    #[test]
    fn numbers_keep_their_kind_through_yaml() {
        let v = json!({"u": 3, "i": -2, "f": 1.5});
        let y = to_yaml(&v);
        let back: Json = serde_json::to_value(&y).unwrap();
        assert_eq!(back, v);
    }
}
