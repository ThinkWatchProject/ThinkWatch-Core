//! 一次重载尝试的五个关卡。
//!
//! **桌面工具和服务器在这里有个重要差异：服务器可以拒绝启动，桌面工具
//! 不行。**你手抖打错一个字母，不能让所有 AI 客户端瞬间断线。
//!
//! ```text
//! ① 语法解析    失败 → 保持当前配置，报错并定位到行
//! ② schema 校验 失败 → 同上
//! ③ 语义校验    route 指向一个删掉的 group？provider 重名？→ 同上
//! ④ 构建运行时对象
//! ⑤ 原子换入
//! ```
//!
//! 第 ③ 步值得单独强调：**语法正确但语义错误的配置最危险** —— YAML
//! 完全合法，运行时却会把请求路由到空处。这类检查必须在换入之前做完。
//!
//! 这个模块只负责 ①②③ 并把失败说清楚。④⑤ 归数据面，因为运行时对象
//! （provider 池、Client、规则树）是它的东西。

use crate::{Config, SCHEMA_VERSION, ValidationError, validate};

/// 卡在哪一关。**分开是为了让用户知道该看哪儿**：语法错要看那一行，
/// 语义错要看整段配置的逻辑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Syntax,
    Schema,
    Semantics,
}

impl Stage {
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Syntax => "语法",
            Stage::Schema => "字段",
            Stage::Semantics => "语义",
        }
    }
}

/// 一次失败的重载。
///
/// **它不是 `Err`，是一个要展示给人看的东西** —— 重载失败之后网关照常
/// 在跑，所以这是一条信息，不是一次崩溃。
#[derive(Debug, Clone, PartialEq)]
pub struct Rejected {
    pub stage: Stage,
    pub message: String,
    /// 1 起。**只有语法和字段错误有行号** —— 语义错误是整份配置的
    /// 问题，硬指一行只会误导。
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// 出错那一行的原文，已脱敏。UI 直接显示它，用户不必自己去数行。
    pub excerpt: Option<String>,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}错误", self.stage.label())?;
        if let Some(l) = self.line {
            write!(f, "（第 {l} 行）")?;
        }
        write!(f, "：{}", self.message)
    }
}

/// 试着把一份文本当成配置读进来。
///
/// **成功了也只是「这份配置是好的」，不代表已经生效** —— 换入是调用方
/// 的事，因为只有它知道运行时对象建不建得起来。
pub fn try_parse(text: &str) -> Result<Config, Rejected> {
    let cfg: Config = match serde_yaml_ng::from_str(text) {
        Ok(c) => c,
        Err(e) => {
            let loc = e.location();
            let line = loc.as_ref().map(|l| l.line());
            return Err(Rejected {
                // serde 把「YAML 语法坏了」和「字段名不认识」混在同一个
                // 错误类型里，但对用户是两件事：前者要看标点，后者要看
                // 拼写。按错误文本分开。
                stage: if is_field_error(&e.to_string()) {
                    Stage::Schema
                } else {
                    Stage::Syntax
                },
                message: e.to_string(),
                line,
                column: loc.as_ref().map(|l| l.column()),
                excerpt: line.and_then(|l| excerpt_of(text, l)),
            });
        }
    };
    if let Err(e) = validate(&cfg) {
        return Err(Rejected {
            stage: stage_of(&e),
            message: e.to_string(),
            // 语义错误没有一个诚实的行号。**编一个出来比不给更糟** ——
            // 用户会盯着那一行看半天，而问题在别处。
            line: None,
            column: None,
            excerpt: None,
        });
    }
    Ok(cfg)
}

fn is_field_error(m: &str) -> bool {
    m.contains("unknown field")
        || m.contains("missing field")
        || m.contains("invalid type")
        || m.contains("invalid value")
        || m.contains("unknown variant")
        || m.contains("duplicate entry")
}

fn stage_of(e: &ValidationError) -> Stage {
    match e {
        // 「schema 太新」说的是这份文件的格式版本，属于字段层面
        ValidationError::SchemaTooNew { .. } => Stage::Schema,
        _ => Stage::Semantics,
    }
}

/// 取出错那一行，**顺便脱敏**。
///
/// 出错的那一行完全可能就是写着密钥的那一行 —— 而这段文字要进日志、
/// 进界面、可能被用户复制到 issue 里（统一脱敏）。
fn excerpt_of(text: &str, line: usize) -> Option<String> {
    let raw = text.lines().nth(line.checked_sub(1)?)?;
    let masked = tw_secret::mask_line(raw);
    // 超长的行截断。**按字符边界截**，按字节切多字节字符会 panic。
    const MAX: usize = 160;
    if masked.chars().count() <= MAX {
        return Some(masked);
    }
    let cut: String = masked.chars().take(MAX).collect();
    Some(format!("{cut}…"))
}

/// 这个 schema 版本我们认吗。
pub fn schema_supported(version: u32) -> bool {
    version <= SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "version: 1\nclients:\n  - name: c\n    key: tw-k\n";

    #[test]
    fn a_good_config_goes_through() {
        assert!(try_parse(GOOD).is_ok());
    }

    #[test]
    fn a_syntax_error_points_at_the_line() {
        // 「手抖打错一个字母」的典型：引号没闭合。
        let bad = "version: 1\nclients:\n  - name: \"c\n    key: tw-k\n";
        let r = try_parse(bad).unwrap_err();
        assert_eq!(r.stage, Stage::Syntax, "{r:?}");
        assert!(r.line.is_some(), "语法错必须给行号：{r:?}");
    }

    #[test]
    fn a_config_with_neither_providers_nor_a_listen_block_still_loads() {
        // 「六行」。零 provider 是首次运行的正常状态，而逼用户
        // 写一行 `providers: []` 只是为了让解析器高兴。
        let cfg = try_parse(GOOD).expect("最小配置该能加载");
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn a_config_with_no_clients_at_all_gets_the_helpful_message_not_serdes() {
        // serde 的「missing field `clients`」说不出下一步做什么。
        let r = try_parse("version: 1\n").unwrap_err();
        assert_eq!(r.stage, Stage::Semantics, "{r:?}");
        assert!(r.message.contains("首次启动"), "{r:?}");
    }

    #[test]
    fn a_misspelled_field_is_a_field_error_not_a_syntax_error() {
        // 这两件事对用户完全不同：一个要看标点，一个要看拼写。
        let bad = "version: 1\nclients:\n  - name: c\n    kye: tw-k\n";
        let r = try_parse(bad).unwrap_err();
        assert_eq!(r.stage, Stage::Schema, "{r:?}");
        assert!(r.message.contains("kye"), "{r:?}");
        assert_eq!(r.line, Some(4), "{r:?}");
    }

    #[test]
    fn a_semantic_error_has_no_line_number_because_there_is_no_honest_one() {
        // **编一个行号出来比不给更糟** —— 用户会盯着那一行看半天。
        let bad = "version: 1\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: a\n    base_url: https://x\n    key: k\n  - name: a\n    base_url: https://y\n    key: k\n";
        let r = try_parse(bad).unwrap_err();
        assert_eq!(r.stage, Stage::Semantics, "{r:?}");
        assert!(r.line.is_none(), "语义错不该编行号：{r:?}");
        assert!(r.message.contains("重复"), "{r:?}");
    }

    #[test]
    fn a_route_pointing_at_a_deleted_group_is_caught_before_anything_swaps_in() {
        // **语法正确但语义错误的配置最危险**：YAML 完全合法，运行时却会
        // 把请求路由到空处。
        let bad = "version: 1\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: a\n    base_url: https://x\n    key: k\nroutes:\n  - name: r\n    to: 已经删掉的组\n";
        let r = try_parse(bad).unwrap_err();
        assert_eq!(r.stage, Stage::Semantics);
        assert!(r.message.contains("已经删掉的组"), "{r:?}");
    }

    #[test]
    fn a_config_from_a_newer_version_is_a_field_error_with_an_upgrade_hint() {
        let bad = "version: 999\nclients:\n  - name: c\n    key: tw-k\n";
        let r = try_parse(bad).unwrap_err();
        assert_eq!(r.stage, Stage::Schema);
        assert!(r.message.contains("升级"), "{r:?}");
        assert!(!schema_supported(999));
        assert!(schema_supported(1));
    }

    #[test]
    fn the_excerpt_never_carries_a_key_in_the_clear() {
        // 出错那一行完全可能就是写着密钥的那一行，而这段文字要进日志、
        // 进界面、可能被复制到 issue 里。
        let bad =
            "version: 1\nclients:\n  - name: c\n    key: sk-ant-verysecretvalue\n    kye: 1\n";
        let r = try_parse(bad).unwrap_err();
        let ex = r.excerpt.unwrap_or_default();
        assert!(!ex.contains("verysecretvalue"), "密钥原文进了摘录：{ex}");
    }

    #[test]
    fn a_very_long_line_is_cut_on_a_character_boundary() {
        // 按字节切多字节字符会 panic —— 这个项目栽过两次。
        let long = "很".repeat(500);
        let bad = format!("version: 1\nx: {long}\nclients:\n  - name: c\n    kye: 1\n");
        let r = try_parse(&bad).unwrap_err();
        // 不 panic 就算过；顺便确认真的截了
        if let Some(ex) = r.excerpt {
            assert!(ex.chars().count() <= 161, "{}", ex.chars().count());
        }
    }

    #[test]
    fn the_display_form_reads_like_a_sentence_a_person_can_act_on() {
        let bad = "version: 1\nclients:\n  - name: c\n    kye: tw-k\n";
        let s = try_parse(bad).unwrap_err().to_string();
        assert!(s.starts_with("字段错误（第 4 行）："), "{s}");
    }
}
