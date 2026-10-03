//! `plugins` 一节：装了哪些脚本插件、批准的是哪一份、开着没有。
//!
//! **这里只有数据，而且只有这四样**：id、文件、批准的哈希、开关。插件出错时怎么办、管哪些
//! 请求、设置的值都写在插件文件自己的 manifest 里（契约附录四）—— 文件就是它的配置所在，
//! 界面改这几样是改那个文件。manifest 要编译才读得出来，那是网关加载插件时的事：文件还是
//! 不是批准的那一份、manifest 合不合规矩，都在那里查，查出问题只让那一个插件停用，**不挡
//! 配置换入**。这里查的是不看插件文件也能判断的：id 的写法、重名、文件路径、哈希的写法。
//!
//! 0.58.0 把出错时怎么办、范围和设置写在这里（`on_error`、`scope`、`settings`）。**读到了
//! 不认**：不报错、也不起作用，配置照样加载；下一次写插件这一节时去掉（[`drop_legacy`]）。
//!
//! 文件由 core 写：`plugins/<id>.js` 是插件，`plugins/.approved/<id>.js` 是批准时
//! 的那一份（给界面显示改了什么）。路径都相对配置文件所在的目录。

use std::path::{Path, PathBuf};

use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// 插件文件所在的目录，相对配置文件所在的目录。
pub const DIR: &str = "plugins";

/// 批准过的那一份放在哪个子目录。点开头：它不是插件，是插件的底稿
pub const APPROVED_DIR: &str = ".approved";

/// id 最长多少个字符
pub const ID_MAX: usize = 40;

/// 不能当 id 的词：控制面上 `/plugins/` 底下这几个是固定的端点（排顺序、试编、改写、
/// 确认过的装），叫这几个名字的插件会和它们撞在同一个路径上
pub const RESERVED_IDS: &[&str] = &["order", "inspect", "rewrite", "confirmed"];

/// 0.58.0 写在这里、现在挪进了插件文件的字段。读到了不认，写插件这一节时去掉
pub const LEGACY_FIELDS: &[&str] = &["on_error", "scope", "settings"];

/// 一个装上了的插件。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plugin {
    /// 小写字母、数字和连字符，1 到 40 个字符，不重复
    pub id: String,
    /// 插件文件，相对配置文件所在的目录。**只能是 `plugins/<id>.js`**：文件是 core
    /// 写的，指到别处的路径只会让保存插件去写一个不该写的文件
    pub file: String,
    /// 批准过的那一份的 SHA-256，64 个小写十六进制字符。**文件的哈希和它不一样，
    /// 插件就不跑**
    pub sha256: String,
    /// 不写是开着
    pub enabled: bool,
}

/// 手写的读法，为了**旧字段不认也不报错**（[`LEGACY_FIELDS`]），而别的写错的字段照样报错
/// —— 和 `deny_unknown_fields` 一样说出是哪一个。字段表就是上面那四个：配置手册照它核对
impl<'de> Deserialize<'de> for Plugin {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        const FIELDS: &[&str] = &["id", "file", "sha256", "enabled"];

        struct Fields;

        impl<'de> Visitor<'de> for Fields {
            type Value = Plugin;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a plugin entry")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Plugin, A::Error> {
                fn once<T, E: de::Error>(
                    slot: &mut Option<T>,
                    name: &'static str,
                    v: T,
                ) -> Result<(), E> {
                    if slot.is_some() {
                        return Err(E::duplicate_field(name));
                    }
                    *slot = Some(v);
                    Ok(())
                }
                let (mut id, mut file, mut sha256, mut enabled) = (None, None, None, None);
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "id" => once(&mut id, "id", map.next_value()?)?,
                        "file" => once(&mut file, "file", map.next_value()?)?,
                        "sha256" => once(&mut sha256, "sha256", map.next_value()?)?,
                        "enabled" => once(&mut enabled, "enabled", map.next_value()?)?,
                        k if LEGACY_FIELDS.contains(&k) => {
                            map.next_value::<IgnoredAny>()?;
                        }
                        k => return Err(de::Error::unknown_field(k, FIELDS)),
                    }
                }
                Ok(Plugin {
                    id: id.ok_or_else(|| de::Error::missing_field("id"))?,
                    file: file.ok_or_else(|| de::Error::missing_field("file"))?,
                    sha256: sha256.ok_or_else(|| de::Error::missing_field("sha256"))?,
                    enabled: enabled.unwrap_or(true),
                })
            }
        }

        d.deserialize_struct("Plugin", FIELDS, Fields)
    }
}

impl Plugin {
    /// 这个 id 的插件文件写在配置里的样子：`plugins/<id>.js`
    pub fn file_for(id: &str) -> String {
        format!("{DIR}/{id}.js")
    }

    /// 插件文件在磁盘上的位置。`dir` 是配置文件所在的目录
    pub fn path_in(&self, dir: &Path) -> PathBuf {
        dir.join(&self.file)
    }
}

/// 插件文件的目录。`dir` 是配置文件所在的目录
pub fn dir_in(dir: &Path) -> PathBuf {
    dir.join(DIR)
}

/// 批准过的那一份在哪儿：`plugins/.approved/<id>.js`
pub fn approved_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(DIR).join(APPROVED_DIR).join(format!("{id}.js"))
}

/// 插件文件的位置：`plugins/<id>.js`
pub fn file_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(Plugin::file_for(id))
}

/// id 写得对不对：小写字母、数字、连字符，1 到 [`ID_MAX`] 个字符。保留词另查
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= ID_MAX
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// 哈希写得对不对：64 个小写十六进制字符
pub fn valid_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 查一份 `plugins`。哪一条不对就说哪一条。
pub(crate) fn check(plugins: &[Plugin]) -> Result<(), crate::ValidationError> {
    use crate::ValidationError as E;
    let mut seen = std::collections::HashSet::new();
    for p in plugins {
        if !valid_id(&p.id) {
            return Err(E::PluginId { id: p.id.clone() });
        }
        if RESERVED_IDS.contains(&p.id.as_str()) {
            return Err(E::PluginIdReserved { id: p.id.clone() });
        }
        if !seen.insert(p.id.as_str()) {
            return Err(E::DuplicatePlugin { id: p.id.clone() });
        }
        if p.file != Plugin::file_for(&p.id) {
            return Err(E::PluginFile {
                id: p.id.clone(),
                file: p.file.clone(),
            });
        }
        if !valid_sha256(&p.sha256) {
            return Err(E::PluginSha256 { id: p.id.clone() });
        }
    }
    Ok(())
}

/// 去掉插件这一节里 0.58.0 留下的旧字段（[`LEGACY_FIELDS`]），别的一个字节不动。**写插件
/// 这一节的每一次都先过它**：配置在第一次写插件时就变成现在的样子。没有旧字段就原样返回
pub fn drop_legacy(text: &str) -> Result<String, crate::edit::EditError> {
    let doc = crate::edit::parse(text)?;
    let Some(items) = doc.get(DIR).and_then(serde_yaml_ng::Value::as_sequence) else {
        return Ok(text.to_string());
    };
    let mut out = text.to_string();
    for item in items {
        let Some(m) = item.as_mapping() else { continue };
        if !LEGACY_FIELDS.iter().any(|k| m.contains_key(*k)) {
            continue;
        }
        let Some(id) = m.get("id").and_then(serde_yaml_ng::Value::as_str) else {
            continue;
        };
        let mut kept = m.clone();
        for k in LEGACY_FIELDS {
            kept.remove(*k);
        }
        out = crate::edit::upsert(&out, crate::edit::PLUGINS, Some(id), &kept)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "6f1c000000000000000000000000000000000000000000000000000000000abc";

    const HEAD: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: c\n    key: tw-k\n";

    fn parse(yaml: &str) -> Result<crate::Config, String> {
        let text = format!("{HEAD}plugins:\n{yaml}");
        crate::try_parse(&text).map_err(|r| format!("{}: {}", r.message.code, r.message.text))
    }

    fn entry(id: &str) -> String {
        format!("  - id: {id}\n    file: plugins/{id}.js\n    sha256: {HASH}\n")
    }

    #[test]
    fn the_shortest_entry_runs_enabled() {
        let cfg = parse(&entry("add-date")).unwrap();
        let p = &cfg.plugins[0];
        assert_eq!(p.id, "add-date");
        assert!(p.enabled);
        let cfg = parse(&format!("{}    enabled: false\n", entry("x1"))).unwrap();
        assert!(!cfg.plugins[0].enabled);
    }

    /// 0.58.0 写的配置：出错时怎么办、范围、设置还在这里。**读到了不认、不报错** —— 写错了的
    /// 也一样（它们本来就不再起作用），配置照样加载
    #[test]
    fn the_fields_written_by_0_58_are_ignored_whatever_they_hold() {
        for legacy in [
            "    on_error: skip\n    scope: { clients: [claude-code], models: [\"claude-*\"] }\n    settings: { note: hi, count: 3, loud: true }\n",
            "    on_error: ignore\n",
            "    scope: { model: [a], models: [\" \"] }\n",
            "    settings: { note: [1, 2], other: { a: 1 }, nothing: null }\n",
            "    settings: \"not even a map\"\n",
        ] {
            let cfg = parse(&format!("{}{legacy}", entry("x1")))
                .unwrap_or_else(|e| panic!("{legacy:?}: {e}"));
            let p = &cfg.plugins[0];
            assert_eq!(
                (p.id.as_str(), p.sha256.as_str(), p.enabled),
                ("x1", HASH, true),
                "{legacy:?}"
            );
        }
    }

    /// 写错的字段名是错误，不是空操作 —— 和别的段落一样；同一个字段写两遍也是
    #[test]
    fn an_unknown_or_repeated_field_is_refused() {
        for (extra, says) in [
            ("    onerror: skip\n", "onerror"),
            ("    scopes: {}\n", "scopes"),
            ("    enabled: true\n    enabled: false\n", "enabled"),
        ] {
            let e = parse(&format!("{}{extra}", entry("x1"))).unwrap_err();
            assert!(e.contains(says), "{extra:?}: {e}");
        }
        let e = parse("  - id: x1\n    sha256: 6f1c\n").unwrap_err();
        assert!(e.contains("file"), "{e}");
    }

    #[test]
    fn ids_are_lowercase_words_up_to_forty_characters() {
        for bad in ["Add-Date", "add_date", "add.date", &"a".repeat(41)] {
            let yaml =
                format!("  - id: \"{bad}\"\n    file: plugins/{bad}.js\n    sha256: {HASH}\n");
            let e = parse(&yaml).unwrap_err();
            assert!(e.starts_with("config.plugin.bad_id"), "{bad}: {e}");
        }
        assert!(parse(&entry(&"a".repeat(40))).is_ok());
        assert!(parse(&entry("a-1-b")).is_ok());
    }

    /// `/plugins/order`、`/plugins/inspect`、`/plugins/rewrite`、`/plugins/confirmed` 是控制面上
    /// 几个固定的端点
    #[test]
    fn the_words_the_control_plane_uses_are_not_ids() {
        for id in RESERVED_IDS {
            let e = parse(&entry(id)).unwrap_err();
            assert!(e.starts_with("config.plugin.reserved_id"), "{id}: {e}");
        }
    }

    /// 控制面上 `/plugins/` 底下每一个固定的词都不能当 id：不然那个插件的
    /// `/plugins/{id}` 和固定的端点落在同一个路径上
    #[test]
    fn every_fixed_word_under_plugins_on_the_control_plane_is_reserved() {
        let fixed: std::collections::BTreeSet<&str> = tw_api::ep::ALL
            .iter()
            .filter_map(|e| e.path.strip_prefix("/plugins/"))
            .map(|rest| rest.split('/').next().unwrap_or_default())
            .filter(|seg| !seg.starts_with('{'))
            .collect();
        assert_eq!(
            fixed,
            RESERVED_IDS.iter().copied().collect(),
            "the reserved ids and the control plane disagree"
        );
    }

    #[test]
    fn an_id_appears_once() {
        let e = parse(&format!("{}{}", entry("a"), entry("a"))).unwrap_err();
        assert!(e.starts_with("config.plugin.duplicate"), "{e}");
    }

    /// 文件是 core 写的：指到别处的路径只会让保存插件去写一个不该写的文件
    #[test]
    fn the_file_is_the_one_core_writes_for_that_id() {
        for file in [
            "plugins/other.js",
            "../plugins/a.js",
            "/etc/a.js",
            "plugins/a.mjs",
            "plugins/.approved/a.js",
        ] {
            let yaml = format!("  - id: a\n    file: {file}\n    sha256: {HASH}\n");
            let e = parse(&yaml).unwrap_err();
            assert!(e.starts_with("config.plugin.file"), "{file}: {e}");
        }
    }

    #[test]
    fn the_hash_is_sixty_four_lowercase_hex_characters() {
        for bad in ["abc", &HASH.to_uppercase(), &format!("{}g", &HASH[..63])] {
            let yaml = format!("  - id: a\n    file: plugins/a.js\n    sha256: \"{bad}\"\n");
            let e = parse(&yaml).unwrap_err();
            assert!(e.starts_with("config.plugin.sha256"), "{bad}: {e}");
        }
    }

    #[test]
    fn paths_are_under_the_plugins_directory_of_the_config() {
        let dir = Path::new("/home/u/.thinkwatch");
        assert_eq!(
            file_path(dir, "a"),
            Path::new("/home/u/.thinkwatch/plugins/a.js")
        );
        assert_eq!(
            approved_path(dir, "a"),
            Path::new("/home/u/.thinkwatch/plugins/.approved/a.js")
        );
        assert_eq!(dir_in(dir), Path::new("/home/u/.thinkwatch/plugins"));
    }

    /// 不写 `plugins` 就是没有插件，写回去也不多出这一行
    #[test]
    fn no_plugins_means_no_section_in_the_file() {
        let cfg = crate::Config::default();
        assert!(cfg.plugins.is_empty());
        let out = serde_yaml_ng::to_string(&cfg).unwrap();
        assert!(!out.contains("plugins"), "{out}");
    }

    /// 旧字段去掉：每一条里的、块式的和行内的都去，别的字节（注释、别的字段）一个不动；
    /// 没有旧字段就原样
    #[test]
    fn dropping_the_legacy_fields_leaves_everything_else_alone() {
        let text = format!(
            "{HEAD}# 插件\nplugins:\n  # 第一个\n  - id: a\n    file: plugins/a.js\n    sha256: {HASH}\n    enabled: false  # 停用\n    on_error: skip\n    scope:\n      models: [\"claude-*\"]\n    settings:\n      note: hi\n  - id: b\n    file: plugins/b.js\n    sha256: {HASH}\n  - {{id: c, file: plugins/c.js, sha256: {HASH}, settings: {{x: 1}}}}\n"
        );
        let out = drop_legacy(&text).unwrap();
        assert_eq!(
            out,
            format!(
                "{HEAD}# 插件\nplugins:\n  # 第一个\n  - id: a\n    file: plugins/a.js\n    sha256: {HASH}\n    enabled: false  # 停用\n  - id: b\n    file: plugins/b.js\n    sha256: {HASH}\n  - id: c\n    file: plugins/c.js\n    sha256: {HASH}\n"
            )
        );
        let before = crate::try_parse(&text).unwrap();
        let after = crate::try_parse(&out).unwrap();
        assert_eq!(before.plugins, after.plugins);
        assert_eq!(drop_legacy(&out).unwrap(), out);
        let none = format!("{HEAD}plugins:\n{}", entry("a"));
        assert_eq!(drop_legacy(&none).unwrap(), none);
        assert_eq!(drop_legacy(HEAD).unwrap(), HEAD);
    }
}
