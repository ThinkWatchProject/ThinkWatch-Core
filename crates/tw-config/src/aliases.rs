//! 模型别名：一个名字 = 同一个模型在各家上游的名称。
//!
//! ```yaml
//! aliases:
//!   deepseek-v4.1: DeepSeek-v4.1-flash       # 单个写成字符串
//!   claude-sonnet-5:                         # 多个写成列表，有序
//!     - claude-sonnet-5
//!     - us.anthropic.claude-sonnet-5-v1:0
//!     - anthropic/claude-sonnet-5
//! ```
//!
//! **全局一张表。**别名不能指向别名，没有通配，没有兜底：哪家上游提供列表里的任一
//! 名称，就能服务这个别名，发过去用它自己的那个名称（按列表顺序取它有的第一个）。
//! 请求的名称在这张表里，就按别名处理 —— 某家上游恰好有同名的真模型、却没列进来，
//! 这个名称不会发给它。
//!
//! **保持书写顺序。**读进来是一个列表，写回去还是用户写的那个顺序：界面按它列，
//! 改一个别名不该让整张表重新排一遍。

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 一个别名：它的名字，和它在各家上游叫什么（有序）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Alias {
    pub name: String,
    pub models: Vec<String>,
    /// 只从客户端模型列表隐藏目标原名；不改变权限、准入或上游清单。
    pub hide_originals: bool,
}

/// 整张别名表，按书写顺序。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Aliases(pub Vec<Alias>);

impl Aliases {
    /// 叫这个名字的别名。**名字要完全相等** —— 别名没有通配
    pub fn find(&self, name: &str) -> Option<&Alias> {
        self.0.iter().find(|a| a.name == name)
    }

    /// 这个名字是不是一个别名
    pub fn contains(&self, name: &str) -> bool {
        self.find(name).is_some()
    }
}

impl std::ops::Deref for Aliases {
    type Target = [Alias];
    fn deref(&self) -> &[Alias] {
        &self.0
    }
}

impl FromIterator<Alias> for Aliases {
    fn from_iter<I: IntoIterator<Item = Alias>>(iter: I) -> Self {
        Aliases(iter.into_iter().collect())
    }
}

/// 一个名称写成字符串，几个写成列表 —— 和读进来时接受的两种写法一样。
impl Serialize for Aliases {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for a in &self.0 {
            if a.hide_originals {
                #[derive(Serialize)]
                struct Expanded<'a> {
                    models: &'a [String],
                    hide_originals: bool,
                }
                m.serialize_entry(
                    &a.name,
                    &Expanded {
                        models: &a.models,
                        hide_originals: true,
                    },
                )?;
                continue;
            }
            match a.models.as_slice() {
                [one] => m.serialize_entry(&a.name, one)?,
                many => m.serialize_entry(&a.name, many)?,
            }
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for Aliases {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Table;
        impl<'de> Visitor<'de> for Table {
            type Value = Aliases;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a map from each alias to a model name or a list of model names")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Aliases, E> {
                Ok(Aliases::default())
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Aliases, A::Error> {
                // **重名不在这里拒**：这里拒的话说出来的是解析器的话，而校验那一句
                // 带码、说得出是哪个别名（`config.alias_duplicate`）
                let mut out = Vec::new();
                while let Some((name, definition)) = map.next_entry::<String, Definition>()? {
                    let (models, hide_originals) = match definition {
                        Definition::Simple(Models(models)) => (models, false),
                        Definition::Expanded(Expanded {
                            models: Models(models),
                            hide_originals,
                        }) => (models, hide_originals),
                    };
                    out.push(Alias {
                        name,
                        models,
                        hide_originals,
                    });
                }
                Ok(Aliases(out))
            }
        }
        d.deserialize_map(Table)
    }
}

enum Definition {
    Simple(Models),
    Expanded(Expanded),
}

impl<'de> Deserialize<'de> for Definition {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Definition;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a model name, a list of model names, or {models, hide_originals}")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Definition, A::Error> {
                Expanded::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(Definition::Expanded)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Definition, A::Error> {
                Models::deserialize(de::value::SeqAccessDeserializer::new(seq))
                    .map(Definition::Simple)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Definition, E> {
                Ok(Definition::Simple(Models(Vec::new())))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Definition, E> {
                Models::deserialize(de::value::StrDeserializer::<E>::new(v)).map(Definition::Simple)
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Definition, E> {
                Models::deserialize(de::value::StringDeserializer::<E>::new(v))
                    .map(Definition::Simple)
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Definition, E> {
                Models::deserialize(de::value::BoolDeserializer::<E>::new(v))
                    .map(Definition::Simple)
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Definition, E> {
                Models::deserialize(de::value::I64Deserializer::<E>::new(v)).map(Definition::Simple)
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Definition, E> {
                Models::deserialize(de::value::U64Deserializer::<E>::new(v)).map(Definition::Simple)
            }
            fn visit_i128<E: de::Error>(self, v: i128) -> Result<Definition, E> {
                Models::deserialize(de::value::I128Deserializer::<E>::new(v))
                    .map(Definition::Simple)
            }
            fn visit_u128<E: de::Error>(self, v: u128) -> Result<Definition, E> {
                Models::deserialize(de::value::U128Deserializer::<E>::new(v))
                    .map(Definition::Simple)
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Definition, E> {
                Models::deserialize(de::value::F64Deserializer::<E>::new(v)).map(Definition::Simple)
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Expanded {
    models: Models,
    #[serde(default)]
    hide_originals: bool,
}

/// 一个别名的值：一个名称，或者一组名称。
struct Models(Vec<String>);

impl<'de> Deserialize<'de> for Models {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Models;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a model name or a list of model names")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Models, E> {
                Ok(Models(vec![v.to_string()]))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Models, E> {
                Ok(Models(vec![v]))
            }
            // 没加引号、YAML 读成数或真假的模型名（`x: 1.5`）：照名字收下，见
            // `tw_engine::scalar_name`。列表里的、别名的名字本身读的是字符串，本来就是原文
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Models, E> {
                Ok(Models(vec![v.to_string()]))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Models, E> {
                Ok(Models(vec![v.to_string()]))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Models, E> {
                Ok(Models(vec![v.to_string()]))
            }
            fn visit_i128<E: de::Error>(self, v: i128) -> Result<Models, E> {
                Ok(Models(vec![v.to_string()]))
            }
            fn visit_u128<E: de::Error>(self, v: u128) -> Result<Models, E> {
                Ok(Models(vec![v.to_string()]))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Models, E> {
                Ok(Models(vec![tw_engine::scalar_name(v)]))
            }
            /// `x:` 后面什么都没写：读成空列表，让校验说「这个别名没有模型」，
            /// 比一句「类型不对」好懂
            fn visit_unit<E: de::Error>(self) -> Result<Models, E> {
                Ok(Models(Vec::new()))
            }
            fn visit_none<E: de::Error>(self) -> Result<Models, E> {
                Ok(Models(Vec::new()))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Models, A::Error> {
                let mut out = Vec::new();
                while let Some(m) = seq.next_element::<String>()? {
                    out.push(m);
                }
                Ok(Models(out))
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expanded_definition_preserves_policy_and_rejects_unknown_fields() {
        let a: Aliases = serde_yaml_ng::from_str(
            "short: {models: [raw-a, raw-b], hide_originals: true}\nlegacy: raw-c\n",
        )
        .unwrap();
        assert!(a[0].hide_originals);
        assert!(!a[1].hide_originals);
        let text = serde_yaml_ng::to_string(&a).unwrap();
        assert!(text.contains("hide_originals: true"));
        assert!(text.contains("legacy: raw-c"));
        assert_eq!(serde_yaml_ng::from_str::<Aliases>(&text).unwrap(), a);
        let plain: Aliases = serde_yaml_ng::from_str("short: {models: raw-a}").unwrap();
        assert_eq!(serde_yaml_ng::to_string(&plain).unwrap(), "short: raw-a\n");
        let err = serde_yaml_ng::from_str::<Aliases>("short: {models: raw-a, hide_original: true}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `hide_original`"), "{err}");
    }

    const TABLE: &str = "\
deepseek-v4.1: DeepSeek-v4.1-flash
claude-sonnet-5:
  - claude-sonnet-5
  - us.anthropic.claude-sonnet-5-v1:0
  - anthropic/claude-sonnet-5
a-model: [x]
";

    fn alias(name: &str, models: &[&str]) -> Alias {
        Alias {
            name: name.into(),
            models: models.iter().map(|m| m.to_string()).collect(),
            hide_originals: false,
        }
    }

    #[test]
    fn a_string_or_a_list_reads_in_the_order_it_is_written() {
        let t: Aliases = serde_yaml_ng::from_str(TABLE).unwrap();
        assert_eq!(
            t.0,
            [
                alias("deepseek-v4.1", &["DeepSeek-v4.1-flash"]),
                alias(
                    "claude-sonnet-5",
                    &[
                        "claude-sonnet-5",
                        "us.anthropic.claude-sonnet-5-v1:0",
                        "anthropic/claude-sonnet-5"
                    ]
                ),
                alias("a-model", &["x"]),
            ]
        );
        assert_eq!(
            t.find("deepseek-v4.1").unwrap().models,
            ["DeepSeek-v4.1-flash"]
        );
        assert!(t.contains("a-model"));
        assert!(!t.contains("a-*"), "没有通配");
    }

    /// 写回去还是那个顺序；一个名称写成字符串
    #[test]
    fn it_writes_back_in_the_same_order_and_shape() {
        let t: Aliases = serde_yaml_ng::from_str(TABLE).unwrap();
        let out = serde_yaml_ng::to_string(&t).unwrap();
        assert_eq!(
            out,
            "deepseek-v4.1: DeepSeek-v4.1-flash\nclaude-sonnet-5:\n- claude-sonnet-5\n- us.anthropic.claude-sonnet-5-v1:0\n- anthropic/claude-sonnet-5\na-model: x\n"
        );
        let back: Aliases = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(back, t);
    }

    /// 没加引号的数和真假是名字：别名的名字、列表里的模型名读的是原文，单个模型名照名字
    /// 收下 —— 手写的配置不该因为 YAML 把 `1.5` 读成了数就读不进
    #[test]
    fn names_that_look_like_numbers_or_bools_read_as_names() {
        let t: Aliases = serde_yaml_ng::from_str(
            "x: 1.5\n2024: 7\ny: true\nz: [1.50, 0x1F, false]\nw: -3\nv: 4.0\n",
        )
        .unwrap();
        assert_eq!(
            t.0,
            [
                alias("x", &["1.5"]),
                alias("2024", &["7"]),
                alias("y", &["true"]),
                alias("z", &["1.50", "0x1F", "false"]),
                alias("w", &["-3"]),
                alias("v", &["4.0"]),
            ]
        );
        // 写回去是带引号的字符串，再读进来不变
        let out = serde_yaml_ng::to_string(&t).unwrap();
        let back: Aliases = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(back, t, "{out}");
    }

    #[test]
    fn an_empty_value_reads_as_no_models_and_a_wrong_type_says_what_to_write() {
        let t: Aliases = serde_yaml_ng::from_str("x:\ny: []\n").unwrap();
        assert_eq!(t.0, [alias("x", &[]), alias("y", &[])]);
        let e = serde_yaml_ng::from_str::<Aliases>("x: {a: b}\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown field `a`"), "{e}");
        let e = serde_yaml_ng::from_str::<Aliases>("[a, b]\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("a map from each alias"), "{e}");
    }
}
