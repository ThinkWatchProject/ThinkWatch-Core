//! 哨兵：**在我们完全不存在之后仍然有效的那一层**。
//!
//! 想清楚这个场景：用户接管了五个客户端，用了三个月，然后把 App 拖进
//! 废纸篓。于是五个客户端的 `base_url` 全都指向一个已经没人监听的端口，
//! **所有 AI 客户端同时失效**，而他很可能已经忘了是什么改的。
//!
//! **macOS 上删除应用没有卸载钩子。**拖进废纸篓就是拖进废纸篓 —— 我们
//! 没有任何机会做清理。所以这件事必须在写第一个字节到用户配置文件
//! **之前**就设计好。
//!
//! 这一层的成本几乎为零：**把原值写进注释**。用户打开文件，看一眼就知道
//! 该改回什么 —— 把「不可恢复」变成「看一眼就能恢复」。

use serde::{Deserialize, Serialize};

pub const BEGIN: &str = "=== ThinkWatch: begin ===";
pub const END: &str = "=== ThinkWatch: end ===";

/// 这个字段原本是什么样。**三态，不是「有值/没值」两态。**
///
/// 第三态存在的理由很具体：原值可能是用户自己的 API key。写进同一个
/// 文件的注释里没问题（它本来就在那个文件里、同一套权限）；但抄进
/// **旁文件**就不一样了 —— 那是一份我们新造出来的、多一处的密钥副本，
/// 而那个目录很可能被 dotfile 管理器提交进 git。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Was {
    /// 原本没有这个字段。**还原时要删掉它，而不是写一个空串进去。**
    Missing,
    Value(String),
    /// 原值是密钥。旁文件里只留一个指针，真值去全文备份里取。
    Secret(String),
}

impl Was {
    pub fn value(&self) -> Option<&str> {
        match self {
            Was::Missing => None,
            Was::Value(v) | Was::Secret(v) => Some(v),
        }
    }
    fn tag(&self) -> &'static str {
        match self {
            Was::Missing => "missing",
            Was::Value(_) => "value",
            Was::Secret(_) => "secret",
        }
    }
    fn note(&self) -> &'static str {
        match self {
            Was::Missing => "the original configuration had no such field; restoring removes it",
            Was::Value(_) => {
                "the original configuration had this field; restoring writes `was` back"
            }
            Was::Secret(_) => {
                "the original value is a secret and is not in this file; take it from the full backup named above"
            }
        }
    }
}

/// 一个被我们改过的字段，连同它原来的样子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Original {
    /// 字段在配置里的路径，人话形式（`env.ANTHROPIC_BASE_URL`）
    pub field: String,
    /// 同一条路径的分段形式，机器用
    pub path: Vec<String>,
    pub was: Was,
}

impl Original {
    pub fn new(path: &[String], was: Was) -> Self {
        Self {
            field: path.join("."),
            path: path.to_vec(),
            was,
        }
    }
    pub fn missing(field: &str) -> Self {
        Self::new(&[field.to_string()], Was::Missing)
    }
    pub fn value(field: &str, v: impl Into<String>) -> Self {
        Self::new(&[field.to_string()], Was::Value(v.into()))
    }
    pub fn secret(field: &str, v: impl Into<String>) -> Self {
        Self::new(&[field.to_string()], Was::Secret(v.into()))
    }
}

/// 给注释型配置（TOML / JSONC / YAML）用的哨兵块。
///
/// 密钥原值**照写** —— 它本来就在这个文件里，同一套权限，注释里再写一遍
/// 不多一处暴露；而少写它，用户就没法手动还原了。
///
/// **「要手动还原」那一句是给我们不在了之后的人看的。**没有它，用户看到
/// 一段注释也不知道自己能做什么。
pub fn comment_block(prefix: &str, originals: &[Original]) -> String {
    let mut out = String::new();
    out.push_str(&format!("{prefix} {BEGIN}\n"));
    for o in originals {
        match &o.was {
            Was::Missing => out.push_str(&format!("{prefix} no {} originally\n", o.field)),
            Was::Value(v) | Was::Secret(v) => {
                out.push_str(&format!("{prefix} was {}: {v}\n", o.field))
            }
        }
    }
    out.push_str(&format!(
        "{prefix} to restore by hand: put the fields above back to what they were (delete the ones that had no value), then delete this block\n"
    ));
    out.push_str(&format!("{prefix} {END}\n"));
    out
}

/// 把哨兵块从文件里摘掉。还原的最后一步。
///
/// **认不出来就原样返回**，不做任何猜测 —— 用户可能自己编辑过那段注释，
/// 那时宁可留下几行注释，也不能删掉别的东西。
pub fn strip(text: &str, prefix: &str) -> String {
    let begin = format!("{prefix} {BEGIN}");
    let end = format!("{prefix} {END}");
    let Some(b) = text.find(&begin) else {
        return text.to_string();
    };
    let Some(e) = text[b..].find(&end).map(|i| b + i + end.len()) else {
        return text.to_string();
    };
    let start = text[..b].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let stop = text[e..]
        .find('\n')
        .map(|i| e + i + 1)
        .unwrap_or(text.len());
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&text[stop..]);
    out
}

/// 旁文件里的一个字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarField {
    pub field: String,
    /// 机器用的路径分段。**不能靠切 `field` 里的点号还原它** —— 键名
    /// 本身就可能带点（`[projects."/Users/x/a.b"]` 这种），拆错了就会
    /// 去改一个不存在的字段，然后「还原成功」地什么都没还原。
    pub path: Vec<String>,
    /// 机器读的标记：`missing` / `value` / `secret`
    pub was: String,
    #[serde(rename = "was_value", skip_serializing_if = "Option::is_none", default)]
    pub value: Option<String>,
    #[serde(rename = "note")]
    pub note: String,
}

/// 严格 JSON 装不下注释，退化成同目录的一个旁文件。
///
/// **接管确认框里要说明这个文件的用途** —— 一个用户没让你建、又看不出
/// 是干什么的文件，比没有更糟。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarRecord {
    /// 人话说明，**放在第一个字段** —— 用户打开这个文件，第一眼看到的
    /// 应该是「这是什么」
    #[serde(rename = "what_this_file_is")]
    pub what: String,
    #[serde(rename = "how_to_restore_by_hand")]
    pub how: String,
    pub client: String,
    pub adopted_at_ms: u64,
    /// 接管前那一刻的全文备份。密钥类原值只在这里，不在本文件里。
    #[serde(rename = "full_backup")]
    pub backup: String,
    /// 这个配置文件本来不存在，是接管时新建的。
    ///
    /// 还原时要能把它**整个删掉** —— 否则「还原」之后会留下一个用户
    /// 从来没有过的文件。但只在它还是空的时候删：用户可能在这期间往
    /// 里加了自己的东西。
    #[serde(rename = "file_created_by_us")]
    pub created_file: bool,
    pub originals: Vec<SidecarField>,
}

/// 旁文件的名字后缀。
///
/// 贴着配置文件本身命名（`settings.json` → `settings.json.thinkwatch.json`），
/// 而不是在目录里放一个固定名字的文件：**一眼能看出它在说谁**，而且
/// 两个客户端共用一个目录时也不会撞。`.aider.conf.yml` 就在 home 根目录，
/// 那儿放一个泛泛的 `.thinkwatch-takeover.json` 既容易撞、又说不清。
pub const SIDECAR_SUFFIX: &str = ".thinkwatch.json";

/// 某份配置文件对应的旁文件路径。
pub fn sidecar_path(config: &std::path::Path) -> std::path::PathBuf {
    let name = config
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("config");
    config.with_file_name(format!("{name}{SIDECAR_SUFFIX}"))
}

impl SidecarRecord {
    pub fn new(
        client: &str,
        at_ms: u64,
        backup: &str,
        created_file: bool,
        originals: &[Original],
    ) -> Self {
        Self {
            what: format!(
                "Written by ThinkWatch Lite when it pointed {client} at the gateway. It records \
                 the fields that were changed and what they were before."
            ),
            how: "Put every field under `originals` back to its `was_value`. A `was` of \
                  `missing` means the field was not there to begin with, so delete it; a `was` of \
                  `secret` means the original is a secret, which is in the file named by \
                  `full_backup`. Then delete this file."
                .to_string(),
            client: client.to_string(),
            adopted_at_ms: at_ms,
            backup: backup.to_string(),
            created_file,
            originals: originals
                .iter()
                .map(|o| SidecarField {
                    field: o.field.clone(),
                    path: o.path.clone(),
                    was: o.was.tag().to_string(),
                    // **密钥不抄进来。**这是个新文件，抄一份就是多一处
                    // 泄漏面，而这个目录很可能被 dotfile 管理器提交进 git。
                    value: match &o.was {
                        Was::Value(v) => Some(v.clone()),
                        Was::Missing | Was::Secret(_) => None,
                    },
                    note: o.was.note().to_string(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn originals() -> Vec<Original> {
        vec![
            Original::value("env.ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
            Original::missing("env.CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Original::secret("env.ANTHROPIC_AUTH_TOKEN", "sk-ant-用户自己的密钥"),
        ]
    }

    #[test]
    fn the_comment_block_tells_a_stranger_what_to_do() {
        // **它要在我们完全不存在之后仍然有效。**用户打开文件时，我们
        // 可能已经在废纸篓里了 —— 那段注释是他唯一的线索。
        let b = comment_block("#", &originals());
        assert!(b.contains("https://api.anthropic.com"), "原值没写进去：{b}");
        assert!(b.contains("to restore by hand"), "没告诉他能做什么：{b}");
        assert!(b.contains(BEGIN) && b.contains(END), "{b}");
    }

    #[test]
    fn a_field_that_did_not_exist_says_so_rather_than_showing_an_empty_value() {
        // **「原值是空字符串」和「原本没有这个字段」是两件事。**还原时
        // 前者要写一个空串，后者要删掉 —— 写反了会留下一个用户从没有过
        // 的字段。
        let b = comment_block("#", &originals());
        assert!(
            b.contains("no env.CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY originally"),
            "{b}"
        );
        assert!(
            !b.contains("was env.CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            "{b}"
        );
    }

    #[test]
    fn the_comment_keeps_the_secret_but_the_sidecar_does_not() {
        // 注释在同一个文件里、同一套权限 —— 写它不多一处暴露，而少写
        // 它用户就没法手动还原。旁文件是我们新造的，抄进去就是多一份
        // 密钥副本，何况那个目录常常被 dotfile 管理器提交进 git。
        let b = comment_block("#", &originals());
        assert!(b.contains("sk-ant-用户自己的密钥"), "{b}");

        let rec = SidecarRecord::new(
            "claude-code",
            1,
            "~/.thinkwatch/backups/1/x",
            false,
            &originals(),
        );
        let j = serde_json::to_string(&rec).unwrap();
        assert!(
            !j.contains("sk-ant-用户自己的密钥"),
            "密钥被抄进旁文件了：{j}"
        );
        let f = rec
            .originals
            .iter()
            .find(|f| f.field.ends_with("AUTH_TOKEN"))
            .unwrap();
        assert_eq!(f.was, "secret");
        assert!(f.note.contains("full backup"), "{}", f.note);
        assert!(rec.backup.contains("backups"), "{}", rec.backup);
    }

    #[test]
    fn the_sidecar_explains_itself_before_anything_else() {
        let rec = SidecarRecord::new("claude-code", 1, "b", false, &originals());
        let j = serde_json::to_string_pretty(&rec).unwrap();
        let first = j.lines().nth(1).unwrap();
        assert!(first.contains("what_this_file_is"), "{first}");
        assert!(j.contains("how_to_restore_by_hand"), "{j}");
    }

    #[test]
    fn the_sidecar_round_trips() {
        let rec = SidecarRecord::new("codex", 42, "b", true, &originals());
        let back: SidecarRecord =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn stripping_removes_exactly_the_block_and_nothing_else() {
        let block = comment_block("#", &originals());
        let src = format!("model = \"x\"\n\n{block}model_provider = \"tw\"\n");
        let out = strip(&src, "#");
        assert_eq!(out, "model = \"x\"\n\nmodel_provider = \"tw\"\n");
    }

    #[test]
    fn a_block_the_user_has_edited_is_left_alone_rather_than_guessed_at() {
        // 认不出来就不动。宁可留下几行注释，也不能删掉别的东西。
        let src = format!("# {BEGIN}\n# 用户把结束标记删了\nmodel = \"x\"\n");
        assert_eq!(strip(&src, "#"), src);
        assert_eq!(strip("完全没有哨兵\n", "#"), "完全没有哨兵\n");
    }

    #[test]
    fn the_sidecar_sits_next_to_the_file_it_describes() {
        // 一眼能看出它在说谁，而且两个客户端共用一个目录也不会撞。
        let p = sidecar_path(std::path::Path::new("/Users/x/.claude/settings.json"));
        assert_eq!(
            p,
            std::path::Path::new("/Users/x/.claude/settings.json.thinkwatch.json")
        );
        let p = sidecar_path(std::path::Path::new("/Users/x/.aider.conf.yml"));
        assert_eq!(
            p,
            std::path::Path::new("/Users/x/.aider.conf.yml.thinkwatch.json")
        );
    }

    #[test]
    fn the_comment_prefix_follows_the_file_format() {
        assert!(
            comment_block("#", &originals())
                .lines()
                .all(|l| l.starts_with('#'))
        );
    }
}
