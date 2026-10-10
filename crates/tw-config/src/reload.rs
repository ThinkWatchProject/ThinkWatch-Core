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

use tw_types::{Msg, msg};

use crate::{Config, ValidationError, validate};

/// 卡在哪一关。**分开是为了让用户知道该看哪儿**：语法错要看那一行，
/// 语义错要看整段配置的逻辑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Syntax,
    Schema,
    Semantics,
}

impl Stage {
    /// 控制面发给界面的值。
    pub fn slug(&self) -> &'static str {
        match self {
            Stage::Syntax => "syntax",
            Stage::Schema => "schema",
            Stage::Semantics => "semantics",
        }
    }
    /// 命令行和日志里的说法，后面接「错误」。
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Syntax => "syntax",
            Stage::Schema => "schema",
            Stage::Semantics => "semantics",
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
    /// 为什么。语义错是带码的一句话；语法和字段错是 serde 的原话，码是
    /// [`UNPARSABLE`]、句子只有 `{detail}` —— 那句话我们翻不了，给人看时
    /// 由 [`Rejected::msg`] 在外面补上哪一关、哪一行。
    ///
    /// **装箱**：它是 `try_parse` 的 `Err` 侧，而一条 `Msg` 有三个堆上的字段，
    /// 不装箱的话成功那一路每次都要抬着这么大一块走。
    pub message: Box<Msg>,
    /// 1 起。**只有语法和字段错误有行号** —— 语义错误是整份配置的
    /// 问题，硬指一行只会误导。
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// 出错那一行的原文，已脱敏。UI 直接显示它，用户不必自己去数行。
    pub excerpt: Option<String>,
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} error", self.stage.label())?;
        if let Some(l) = self.line {
            write!(f, " (line {l})")?;
        }
        write!(f, ": {}", self.message)
    }
}

/// serde 读不进来时 [`Rejected::message`] 的码。
pub const UNPARSABLE: &str = "config.unparsable";

impl Rejected {
    /// 给人看的那句话，带码。
    ///
    /// **语义错直接就是那一句**：它说清了是哪个上游、哪条规则，前面再垫
    /// 一句「语义错误」只会把人要找的那半句挤到后面去。语法和字段错的
    /// 原话是 serde 的英文，能翻的只有外面那一层：哪一关、第几行。
    pub fn msg(&self) -> Msg {
        if self.message.code != UNPARSABLE {
            return (*self.message).clone();
        }
        let stage = self.stage.slug();
        let detail = &self.message.text;
        match self.line {
            Some(line) => msg!(
                "config.rejected_at", stage = stage, line = line, detail = detail =>
                "{stage} error (line {line}): {detail}"
            ),
            None => msg!(
                "config.rejected", stage = stage, detail = detail =>
                "{stage} error: {detail}"
            ),
        }
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
                message: Box::new(
                    field_msg(&e.to_string())
                        .unwrap_or_else(|| msg!(UNPARSABLE, detail = e => "{detail}")),
                ),
                line,
                column: loc.as_ref().map(|l| l.column()),
                excerpt: line.and_then(|l| excerpt_of(text, l)),
            });
        }
    };
    if let Err(e) = validate(&cfg) {
        return Err(Rejected {
            stage: stage_of(&e),
            message: Box::new(e.msg()),
            // 语义错误没有一个诚实的行号。**编一个出来比不给更糟** ——
            // 用户会盯着那一行看半天，而问题在别处。
            line: None,
            column: None,
            excerpt: None,
        });
    }
    Ok(cfg)
}

/// 安全模式下磁盘上那份读不了时，core 临时顶上的配置，连同读不了的原因。
///
/// **只有控制面的钥匙取自原文**：桌面端从同一个文件读它来连控制面，两边对不上就连不上，
/// 用户也就看不到错在哪一行。其余全是默认值 —— 安全模式不起数据面，用不着上游和密钥。
/// 原文读得进来（不需要顶）、或者连钥匙都找不到（YAML 坏到解析不了，起了控制面也没人
/// 进得来）都是 `None`
pub fn stand_in(text: &str) -> Option<(Config, Rejected)> {
    let r = try_parse(text).err()?;
    let key = crate::control_key::raw_in(text).ok().flatten()?;
    let cfg = Config {
        listen: crate::Listen {
            control: crate::ControlListen {
                key: Some(key),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    Some((cfg, r))
}

/// serde 最常见的两种字段错，换成带码、带参数的一句话：取值不在可选范围里、字段名不认识。
///
/// **原话是英文，而这两种恰恰最常见**：手改配置写错一个取值、拼错一个字段名，界面上就只有
/// 一句「unknown variant `passthrough`, expected `intercept` or `forward`」。拆出字段、写下的
/// 值和可选的几个，界面就能说成自己的话。认不出的照旧走 [`UNPARSABLE`]。
///
/// serde_yaml 的原话形如 ``client_probes.titling: unknown variant `passthrough`, expected
/// `intercept` or `forward` at line 48 column 12``：冒号前是字段的路径（顶层没有），末尾是
/// 位置（行号另有字段，这里去掉）。
fn field_msg(e: &str) -> Option<Msg> {
    let e = e.split(" at line ").next()?;
    let (path, rest) = match e.find(": unknown ") {
        Some(i) => (&e[..i], &e[i + 2..]),
        None => ("", e),
    };
    if let Some(r) = rest.strip_prefix("unknown variant `") {
        let (value, after) = r.split_once('`')?;
        let expected = quoted(after.split_once("expected ")?.1)?;
        if path.is_empty() {
            return None;
        }
        return Some(msg!(
            "config.unknown_variant", field = path, value = value, expected = expected =>
            "{field} cannot be {value}; expected one of: {expected}"
        ));
    }
    if let Some(r) = rest.strip_prefix("unknown field `") {
        let (name, after) = r.split_once('`')?;
        let expected = quoted(after.split_once("expected ")?.1)?;
        let field = if path.is_empty() {
            name.to_string()
        } else {
            format!("{path}.{name}")
        };
        return Some(msg!(
            "config.unknown_field", field = field, expected = expected =>
            "{field} is not a known field; known fields: {expected}"
        ));
    }
    None
}

/// 「`a`, `b` or `c`」里反引号括起来的那几个，用逗号连起来。一个都没有是 `None`
fn quoted(s: &str) -> Option<String> {
    let all: Vec<&str> = s.split('`').skip(1).step_by(2).collect();
    (!all.is_empty()).then(|| all.join(", "))
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
    // 控制面的钥匙整个换掉：`mask_line` 留头留尾，而它是那扇门的全部凭据
    let masked = tw_secret::mask_line(&crate::control_key::mask_hex_runs(raw));
    // 超长的行截断。**按字符边界截**，按字节切多字节字符会 panic。
    const MAX: usize = 160;
    if masked.chars().count() <= MAX {
        return Some(masked);
    }
    let cut: String = masked.chars().take(MAX).collect();
    Some(format!("{cut}…"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: c\n    key: tw-k\n";

    #[test]
    fn a_good_config_goes_through() {
        assert!(try_parse(GOOD).is_ok());
    }

    /// 没有钥匙的配置不收：它一旦换进来，下一条连接谁都进不来。
    #[test]
    fn a_config_without_a_control_key_is_refused_as_a_semantic_error() {
        let r = try_parse("version: 1\nclients:\n  - name: c\n    key: tw-k\n").unwrap_err();
        assert_eq!(r.stage, Stage::Semantics, "{r:?}");
        assert_eq!(r.message.code, "config.control_key_missing");
    }

    #[test]
    fn a_syntax_error_points_at_the_line() {
        // 「手抖打错一个字母」的典型：引号没闭合。
        let bad = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: \"c\n    key: tw-k\n";
        let r = try_parse(bad).unwrap_err();
        assert_eq!(r.stage, Stage::Syntax, "{r:?}");
        assert!(r.line.is_some(), "语法错必须给行号：{r:?}");
    }

    /// 名字没加引号、YAML 读成了数：0.61.0 读得进（`to` 是字符串），现在也要读得进 ——
    /// 读不进就是安全模式。上游叫 `2024`、规则转发给它，别名的模型叫 `1.5`
    #[test]
    fn names_written_as_bare_numbers_still_load() {
        let text = format!(
            "{GOOD}providers:
  - name: 2024
    base_url: https://a.example
    key: sk
aliases:
  x: 1.5
routes:
  - name: default
    rules:
      - name: r
        to: 2024
"
        );
        let cfg = try_parse(&text).unwrap_or_else(|e| panic!("{e:?}\n{text}"));
        assert_eq!(cfg.providers[0].name, "2024");
        assert_eq!(cfg.aliases[0].models, ["1.5"]);
        assert_eq!(
            cfg.routes[0].rules[0].to,
            Some(tw_engine::Target::Name("2024".into()))
        );
    }

    #[test]
    fn a_config_without_providers_still_loads() {
        // 零 provider 是首次运行的正常状态，而逼用户写一行 `providers: []`
        // 只是为了让解析器高兴。`listen` 那一节只有控制面的钥匙。
        let cfg = try_parse(GOOD).expect("最小配置该能加载");
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn a_config_with_no_clients_at_all_gets_the_helpful_message_not_serdes() {
        // serde 的「missing field `clients`」说不出下一步做什么。
        let r = try_parse("version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\n").unwrap_err();
        assert_eq!(r.stage, Stage::Semantics, "{r:?}");
        assert!(r.message.text.contains("generated on first start"), "{r:?}");
    }

    #[test]
    fn a_misspelled_field_is_a_field_error_not_a_syntax_error() {
        // 这两件事对用户完全不同：一个要看标点，一个要看拼写。
        let bad = "version: 1\nclients:\n  - name: c\n    kye: tw-k\n";
        let r = try_parse(bad).unwrap_err();
        assert_eq!(r.stage, Stage::Schema, "{r:?}");
        assert!(r.message.text.contains("kye"), "{r:?}");
        assert_eq!(r.line, Some(4), "{r:?}");
    }

    #[test]
    fn a_config_that_does_not_load_is_stood_in_for_with_its_own_control_key() {
        let bad = format!("{GOOD}client_probes:\n  titling: passthrough\n");
        let (cfg, r) = stand_in(&bad).expect("钥匙在，就该顶得上");
        assert_eq!(
            cfg.listen.control.key.as_deref(),
            Some("c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00")
        );
        assert!(cfg.providers.is_empty() && cfg.clients.is_empty());
        assert_eq!(r.line, Some(9));
        // 读得进来的不用顶；连钥匙都找不到的顶了也没人进得来
        assert!(stand_in(GOOD).is_none());
        assert!(stand_in("version: 1\nclients: [\n").is_none());
    }

    #[test]
    fn a_value_outside_its_choices_names_the_field_the_value_and_the_choices() {
        // 手改配置最常见的一种错：写了一个已经不存在的取值。界面要能翻译这句话
        let bad = format!("{GOOD}client_probes:\n  titling: passthrough\n");
        let r = try_parse(&bad).unwrap_err();
        assert_eq!(r.stage, Stage::Schema, "{r:?}");
        assert_eq!(r.message.code, "config.unknown_variant", "{r:?}");
        let args = &r.message.args;
        assert_eq!(args["field"], "client_probes.titling");
        assert_eq!(args["value"], "passthrough");
        assert_eq!(args["expected"], "intercept, forward");
        assert_eq!(r.line, Some(9), "{r:?}");
        assert_eq!(r.excerpt.as_deref(), Some("  titling: passthrough"));
    }

    #[test]
    fn a_misspelled_field_says_where_and_what_is_known() {
        let r = try_parse("version: 1\nclients:\n  - name: c\n    kye: tw-k\n").unwrap_err();
        assert_eq!(r.message.code, "config.unknown_field", "{r:?}");
        let args = &r.message.args;
        assert_eq!(args["field"], "clients[0].kye");
        assert!(args["expected"].contains("key"), "{r:?}");
        // 带码的这两种直接就是那句话，不再垫一句「第几行有字段错误」：行号在 `line` 里
        assert_eq!(r.msg().code, "config.unknown_field");
    }

    #[test]
    fn other_serde_errors_keep_the_original_sentence() {
        assert!(field_msg("invalid type: string \"x\", expected u16 at line 3 column 9").is_none());
        // 顶层没有路径的取值错，说不出是哪个字段，也不拆
        assert!(field_msg("unknown variant `x`, expected `a` or `b`").is_none());
    }

    #[test]
    fn a_semantic_error_has_no_line_number_because_there_is_no_honest_one() {
        // **编一个行号出来比不给更糟** —— 用户会盯着那一行看半天。
        let bad = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: a\n    base_url: https://x\n    key: k\n  - name: a\n    base_url: https://y\n    key: k\n";
        let r = try_parse(bad).unwrap_err();
        assert_eq!(r.stage, Stage::Semantics, "{r:?}");
        assert!(r.line.is_none(), "语义错不该编行号：{r:?}");
        assert!(r.message.text.contains("appears twice"), "{r:?}");
    }

    #[test]
    fn a_route_pointing_at_a_deleted_group_is_caught_before_anything_swaps_in() {
        // **语法正确但语义错误的配置最危险**：YAML 完全合法，运行时却会
        // 把请求路由到空处。
        let bad = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: c\n    key: tw-k\nproviders:\n  - name: a\n    base_url: https://x\n    key: k\nroutes:\n  - name: 默认\n    rules:\n      - name: r\n        to: 已经删掉的组\n";
        let r = try_parse(bad).unwrap_err();
        assert_eq!(r.stage, Stage::Semantics);
        assert!(r.message.text.contains("已经删掉的组"), "{r:?}");
    }

    #[test]
    fn a_config_from_a_newer_version_is_a_field_error_with_an_upgrade_hint() {
        let bad = "version: 999\nclients:\n  - name: c\n    key: tw-k\n";
        let r = try_parse(bad).unwrap_err();
        assert_eq!(r.stage, Stage::Schema);
        assert!(r.message.text.contains("Upgrade the app"), "{r:?}");
    }

    #[test]
    fn the_excerpt_never_carries_a_key_in_the_clear() {
        // 出错那一行完全可能就是写着密钥的那一行，而这段文字要进日志、
        // 进界面、可能被复制到 issue 里。
        let bad = "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - name: c\n    key: sk-ant-verysecretvalue\n    kye: 1\n";
        let r = try_parse(bad).unwrap_err();
        let ex = r.excerpt.unwrap_or_default();
        assert!(!ex.contains("verysecretvalue"), "密钥原文进了摘录：{ex}");
    }

    #[test]
    fn a_very_long_line_is_cut_on_a_character_boundary() {
        // 按字节切多字节字符会 panic —— 这个项目栽过两次。
        let long = "很".repeat(500);
        let bad = format!(
            "version: 1\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nx: {long}\nclients:\n  - name: c\n    kye: 1\n"
        );
        let r = try_parse(&bad).unwrap_err();
        // 不 panic 就算过；顺便确认真的截了
        if let Some(ex) = r.excerpt {
            assert!(ex.chars().count() <= 161, "{}", ex.chars().count());
        }
    }

    /// 一家上游的 `balance:`：`auto`、`off` 和每一种来源都读得进；不写就是 `auto`，
    /// 而且写回去时不写出来
    #[test]
    fn every_balance_setting_loads() {
        use crate::BalanceSetting as B;
        let with = |value: &str| {
            format!(
                "{GOOD}providers:\n  - name: relay\n    base_url: https://relay.example\n    key: sk\n    balance: {value}\n"
            )
        };
        for (value, want) in [
            ("auto", B::Auto),
            ("off", B::Off),
            ("openrouter", B::Openrouter),
            ("deepseek", B::Deepseek),
            ("moonshot", B::Moonshot),
            ("sub2api", B::Sub2api),
            ("newapi", B::Newapi),
            ("thinkwatch", B::Thinkwatch),
        ] {
            let cfg = try_parse(&with(value)).unwrap_or_else(|e| panic!("{value}: {e}"));
            assert_eq!(cfg.providers[0].balance, want, "{value}");
            // 和契约里的那个词一一对应
            assert_eq!(tw_api::BalanceSetting::from(want).slug(), value);
            assert_eq!(
                crate::BalanceSetting::from(tw_api::BalanceSetting::from(want)),
                want
            );
        }
        let plain = format!(
            "{GOOD}providers:\n  - name: relay\n    base_url: https://relay.example\n    key: sk\n"
        );
        let cfg = try_parse(&plain).unwrap();
        assert_eq!(cfg.providers[0].balance, B::Auto);
        let written = serde_yaml_ng::to_string(&cfg.providers[0]).unwrap();
        assert!(!written.contains("balance"), "{written}");
    }

    /// 不认识的取值：加载不了，说清楚是哪个字段、写了什么、能写哪几个、在第几行；一键修复
    /// 删掉这一行，回到 `auto`
    #[test]
    fn an_unknown_balance_setting_is_refused_and_repaired_to_auto() {
        let text = format!(
            "{GOOD}providers:\n  - name: relay\n    base_url: https://relay.example\n    key: sk\n    balance: wallet\n"
        );
        let r = try_parse(&text).unwrap_err();
        assert_eq!(r.stage, Stage::Schema, "{r:?}");
        assert_eq!(r.message.code, "config.unknown_variant", "{r:?}");
        assert_eq!(r.message.arg("field"), "providers[0].balance");
        assert_eq!(r.message.arg("value"), "wallet");
        assert_eq!(
            r.message.arg("expected"),
            "auto, off, openrouter, deepseek, moonshot, sub2api, newapi, thinkwatch"
        );
        assert_eq!(r.line, Some(text.lines().count()));
        let fix = crate::repair::repair(&text).expect("一键修复改得了");
        assert_eq!(fix.fixes[0].field, "providers[0].balance");
        assert_eq!(
            try_parse(&fix.text).unwrap().providers[0].balance,
            crate::BalanceSetting::Auto
        );
    }

    /// 一家上游的 `signed_in:`：`zai`、`bigmodel` 读得进、原样写回去；不写（这一项出现之前
    /// 的配置、手填的密钥）就是没有，写回去时也不写出来
    #[test]
    fn the_sign_in_mark_loads_and_round_trips() {
        use crate::SignedIn as S;
        let with = |extra: &str| {
            format!(
                "{GOOD}providers:\n  - name: glm\n    base_url: https://api.z.ai/api/anthropic\n    protocol: anthropic\n    key: id.secret\n{extra}"
            )
        };
        for (value, want) in [("zai", S::Zai), ("bigmodel", S::Bigmodel)] {
            let cfg = try_parse(&with(&format!("    signed_in: {value}\n")))
                .unwrap_or_else(|e| panic!("{value}: {e}"));
            assert_eq!(cfg.providers[0].signed_in, Some(want), "{value}");
            // 和契约里的那个词一一对应
            assert_eq!(tw_api::ZaiFamily::from(want).slug(), value);
            assert_eq!(S::from(tw_api::ZaiFamily::from(want)), want);
            let written = serde_yaml_ng::to_string(&cfg.providers[0]).unwrap();
            assert!(
                written.contains(&format!("signed_in: {value}")),
                "{written}"
            );
            let back: crate::Provider = serde_yaml_ng::from_str(&written).unwrap();
            assert_eq!(back.signed_in, Some(want));
        }
        let cfg = try_parse(&with("")).unwrap();
        assert_eq!(cfg.providers[0].signed_in, None);
        let written = serde_yaml_ng::to_string(&cfg.providers[0]).unwrap();
        assert!(!written.contains("signed_in"), "{written}");
    }

    /// 不认识的取值：加载不了，说清楚写了什么、能写哪几个；一键修复删掉这一行
    #[test]
    fn an_unknown_sign_in_mark_is_refused_and_repaired_away() {
        let text = format!(
            "{GOOD}providers:\n  - name: glm\n    base_url: https://api.z.ai/api/anthropic\n    key: id.secret\n    signed_in: chatgpt\n"
        );
        let r = try_parse(&text).unwrap_err();
        assert_eq!(r.message.code, "config.unknown_variant", "{r:?}");
        assert_eq!(r.message.arg("field"), "providers[0].signed_in");
        assert_eq!(r.message.arg("value"), "chatgpt");
        assert_eq!(r.message.arg("expected"), "zai, bigmodel");
        let fix = crate::repair::repair(&text).expect("一键修复改得了");
        assert_eq!(fix.fixes[0].field, "providers[0].signed_in");
        assert_eq!(try_parse(&fix.text).unwrap().providers[0].signed_in, None);
    }

    #[test]
    fn the_display_form_reads_like_a_sentence_a_person_can_act_on() {
        let bad = "version: 1\nclients:\n  - name: c\n    kye: tw-k\n";
        let s = try_parse(bad).unwrap_err().to_string();
        assert!(s.starts_with("schema error (line 4): "), "{s}");
    }
}
