//! `plugins` 一节：装了哪些脚本插件、批准的是哪一份、管哪些请求。
//!
//! **这里只有数据。**插件文件里写的 manifest（名字、权限、设置项）要编译才读得出来，
//! 那是网关加载插件时的事：设置的键和类型对不对得上 manifest、文件还是不是批准的那
//! 一份，都在那里查，查出问题只让那一个插件停用，**不挡配置换入**。这里查的是不看
//! 插件文件也能判断的那些：id 的写法、重名、文件路径、哈希的写法、范围里的空模式、
//! 设置值的类型。
//!
//! 文件由 core 写：`plugins/<id>.js` 是插件，`plugins/.approved/<id>.js` 是批准时
//! 的那一份（给界面显示改了什么）。路径都相对配置文件所在的目录。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 插件文件所在的目录，相对配置文件所在的目录。
pub const DIR: &str = "plugins";

/// 批准过的那一份放在哪个子目录。点开头：它不是插件，是插件的底稿
pub const APPROVED_DIR: &str = ".approved";

/// id 最长多少个字符
pub const ID_MAX: usize = 40;

/// 不能当 id 的词：控制面上 `/plugins/order`、`/plugins/inspect` 是两个固定的端点，
/// 叫这两个名字的插件会和它们撞在同一个路径上
pub const RESERVED_IDS: &[&str] = &["order", "inspect"];

/// 一个装上了的插件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    /// 小写字母、数字和连字符，1 到 40 个字符，不重复
    pub id: String,
    /// 插件文件，相对配置文件所在的目录。**只能是 `plugins/<id>.js`**：文件是 core
    /// 写的，指到别处的路径只会让「替换源码」去写一个不该写的文件
    pub file: String,
    /// 批准过的那一份的 SHA-256，64 个小写十六进制字符。**文件的哈希和它不一样，
    /// 插件就不跑**
    pub sha256: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// 插件出错、文件变了、加载不了时，它管的请求怎么办
    #[serde(default)]
    pub on_error: PluginOnError,
    /// 管哪些请求。装上时照插件建议的填，之后以这里为准
    #[serde(default, skip_serializing_if = "PluginScope::is_empty")]
    pub scope: PluginScope,
    /// 设置的值：字符串、数字或 true/false。**没写的取插件的默认值**
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, serde_yaml_ng::Value>,
}

fn yes() -> bool {
    true
}

/// 插件出错时这个请求怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginOnError {
    /// 拒绝这个请求。**不写就是它**：插件管不了的请求不该悄悄照原样发出去
    #[default]
    Reject,
    /// 跳过这个插件，请求照常
    Skip,
}

impl PluginOnError {
    pub fn slug(&self) -> &'static str {
        match self {
            PluginOnError::Reject => "reject",
            PluginOnError::Skip => "skip",
        }
    }
}

/// 插件管哪些请求。**每张单子里都是 `*` 通配**（不分大小写），空着是「都管」。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginScope {
    /// 客户端应用：`claude-code`、`codex`……
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clients: Vec<String>,
    /// 发给上游的模型：路由规则改了名的，按改名之后的
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// 发往的上游。请求和回答都按它：请求钩子每发往一个上游跑一次
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<String>,
}

impl PluginScope {
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty() && self.models.is_empty() && self.upstreams.is_empty()
    }

    fn patterns(&self) -> impl Iterator<Item = &String> {
        self.clients
            .iter()
            .chain(&self.models)
            .chain(&self.upstreams)
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

/// 设置值能不能交给插件：字符串、数字、true/false
pub fn valid_setting(v: &serde_yaml_ng::Value) -> bool {
    matches!(
        v,
        serde_yaml_ng::Value::String(_)
            | serde_yaml_ng::Value::Number(_)
            | serde_yaml_ng::Value::Bool(_)
    )
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
        if p.scope.patterns().any(|x| x.trim().is_empty()) {
            return Err(E::BlankPluginPattern { id: p.id.clone() });
        }
        if let Some((key, _)) = p.settings.iter().find(|(_, v)| !valid_setting(v)) {
            return Err(E::PluginSettingType {
                id: p.id.clone(),
                key: key.clone(),
            });
        }
    }
    Ok(())
}

impl From<PluginOnError> for tw_api::OnError {
    fn from(o: PluginOnError) -> Self {
        match o {
            PluginOnError::Reject => Self::Reject,
            PluginOnError::Skip => Self::Skip,
        }
    }
}

impl From<tw_api::OnError> for PluginOnError {
    fn from(o: tw_api::OnError) -> Self {
        match o {
            tw_api::OnError::Reject => Self::Reject,
            tw_api::OnError::Skip => Self::Skip,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "6f1c000000000000000000000000000000000000000000000000000000000abc";

    fn parse(yaml: &str) -> Result<crate::Config, String> {
        let text = format!(
            "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: c\n    key: tw-k\nplugins:\n{yaml}"
        );
        crate::try_parse(&text).map_err(|r| format!("{}: {}", r.message.code, r.message.text))
    }

    fn entry(id: &str) -> String {
        format!("  - id: {id}\n    file: plugins/{id}.js\n    sha256: {HASH}\n")
    }

    #[test]
    fn the_shortest_entry_runs_enabled_and_rejects_on_error() {
        let cfg = parse(&entry("add-date")).unwrap();
        let p = &cfg.plugins[0];
        assert_eq!(p.id, "add-date");
        assert!(p.enabled);
        assert_eq!(p.on_error, PluginOnError::Reject);
        assert!(p.scope.is_empty() && p.settings.is_empty());
    }

    #[test]
    fn every_field_reads_back() {
        let cfg = parse(&format!(
            "{}    enabled: false\n    on_error: skip\n    scope: {{ clients: [claude-code], models: [\"claude-*\"], upstreams: [anthropic] }}\n    settings: {{ note: hi, count: 3, loud: true }}\n",
            entry("x1")
        ))
        .unwrap();
        let p = &cfg.plugins[0];
        assert!(!p.enabled);
        assert_eq!(p.on_error, PluginOnError::Skip);
        assert_eq!(p.scope.models, ["claude-*"]);
        assert_eq!(p.settings["count"], serde_yaml_ng::Value::from(3));
        assert_eq!(p.settings["loud"], serde_yaml_ng::Value::from(true));
    }

    /// 写错的字段名是错误，不是空操作 —— 和别的段落一样
    #[test]
    fn an_unknown_field_is_refused() {
        let e = parse(&format!("{}    onerror: skip\n", entry("x1"))).unwrap_err();
        assert!(e.contains("onerror"), "{e}");
        let e = parse(&format!("{}    scope: {{ model: [a] }}\n", entry("x1"))).unwrap_err();
        assert!(e.contains("model"), "{e}");
        let e = parse(&format!("{}    on_error: ignore\n", entry("x1"))).unwrap_err();
        assert!(e.contains("ignore"), "{e}");
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

    /// `/plugins/order` 和 `/plugins/inspect` 是控制面上两个固定的端点
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

    /// 文件是 core 写的：指到别处的路径只会让「替换源码」去写一个不该写的文件
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
    fn a_scope_pattern_cannot_be_blank() {
        let e = parse(&format!("{}    scope: {{ models: [\" \"] }}\n", entry("a"))).unwrap_err();
        assert!(e.starts_with("config.plugin.blank_pattern"), "{e}");
    }

    #[test]
    fn a_setting_is_a_string_a_number_or_a_boolean() {
        for bad in ["[1, 2]", "{ a: 1 }", "null"] {
            let e = parse(&format!("{}    settings: {{ note: {bad} }}\n", entry("a"))).unwrap_err();
            assert!(e.starts_with("config.plugin.setting_type"), "{bad}: {e}");
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
}
