//! 规则视图：管理界面上每一项防护的档位和每一条规则。**两个产品的管理接口返回同一份
//! JSON**（桌面版 `GET /security`，企业版 `GET /api/admin/security`）。
//!
//! 在此之前规则是看不见的：用户能做的只有在三个档位之间选一个，而看不见一条误报是哪条
//! 规则报的，就只能把整项关掉 —— 连真有用的那部分一起。现在每条规则都列得出来：按什么
//! 认（[`Matcher`]）、开没开、出厂开不开、命中之后做什么、出厂做什么。
//!
//! **视图是无损的**：企业版的管理界面按它拼回整份策略、整键写回。内置规则带着开关、出厂
//! 开关、处置、出厂处置（停用着的也带处置）；自定义规则带着存下来的原样写法（码位不改写
//! 成规范写法）、处置、开关和标签。
//!
//! 类型带着 serde；导出成 TypeScript 在 `ts` 特性后面（名字和桌面版控制面契约里的一致：
//! `GuardMode`、`SecurityRuleView`……）。

use serde::{Deserialize, Serialize};

use crate::content::{self, Codepoints};
use crate::policy::{
    ContentAction, ContentPolicy, DEFAULT_LABEL, LABEL_PREFIX, Mode, RedactPolicy, Security,
    ToolAction, ToolPolicy,
};
use crate::redact::rules as redact_rules;

/// 一条规则在第三档下做什么。工具调用审查是 `cut` / `record`，内容过滤是 `block` /
/// `strip` / `record`；出站脱敏的规则命中即替换，没有这一项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum RuleAction {
    /// 切断这个工具调用所在的流（工具调用审查）
    #[serde(rename = "cut")]
    Cut,
    /// 拒绝这个请求，不发出去（内容过滤）
    #[serde(rename = "block")]
    Block,
    /// 删掉命中的字再发出（内容过滤）
    #[serde(rename = "strip")]
    Strip,
    /// 只记录
    #[serde(rename = "record")]
    Record,
}

words!(RuleAction {
    Cut = "cut",
    Block = "block",
    Strip = "strip",
    Record = "record",
});

impl RuleAction {
    /// 工具调用审查认的那两个
    pub fn tool(self) -> Option<ToolAction> {
        ToolAction::from_slug(self.slug())
    }
    /// 内容过滤认的那三个
    pub fn content(self) -> Option<ContentAction> {
        ContentAction::from_slug(self.slug())
    }
}

impl From<ToolAction> for RuleAction {
    fn from(a: ToolAction) -> Self {
        match a {
            ToolAction::Cut => RuleAction::Cut,
            ToolAction::Record => RuleAction::Record,
        }
    }
}

impl From<ContentAction> for RuleAction {
    fn from(a: ContentAction) -> Self {
        match a {
            ContentAction::Block => RuleAction::Block,
            ContentAction::Strip => RuleAction::Strip,
            ContentAction::Record => RuleAction::Record,
        }
    }
}

/// 一条规则按什么认。**给界面说明用**，界面按类型写成自己的话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Matcher {
    /// 以 `prefix` 开头，其后至少还有 `min_tail` 个字符
    Prefix { prefix: String, min_tail: usize },
    /// `sk-` 开头的 OpenAI 老式密钥：全长至少 `min_len`，字母和数字都有
    OpenaiLegacy { min_len: usize },
    /// PEM 私钥块，BEGIN 到对应的 END 整段
    Pem,
    /// 三段 base64url，首段解码后含 `"alg"`
    Jwt,
    /// `协议://用户:口令@主机` 里的口令
    ConnString,
    /// RFC1918 私有地址，不含回环
    PrivateIp,
    /// 以这几个后缀结尾的域名
    DomainSuffix { suffixes: Vec<String> },
    /// 18 位的中华人民共和国居民身份证号码：头两位是省级行政区划代码，第 7–14 位
    /// 是 `born_since` 年 1 月 1 日到今天之间的真实日期，末位是对得上的
    /// ISO 7064 MOD 11-2 校验码（`0`–`9` 或 `X`）。15 位的老号码不认
    CnResidentId { born_since: u16 },
    /// 卡号：开头和位数属于其中一家卡组织，并且通过 Luhn 校验。连着写的，或者
    /// 四位一组、用一个空格或一个连字符隔开的（最后一组可以不足四位；American
    /// Express 另有 4-6-5、Diners Club 另有 4-6-4）。公开的测试卡号不算
    BankCard { networks: Vec<CardNetwork> },
    /// 邮箱地址：`本地部分@域名`，域名至少两段、最后一段是两个以上的字母。URL 里的
    /// 用户名和 `icon@2x.png` 这类文件名不算
    Email,
    /// 中国大陆手机号：11 位数字，`1` 开头、第二位是 `3`–`9`，前后不紧挨别的数字
    CnMobilePhone,
    /// 正则表达式：工具调用审查的全部规则，和各项防护的自定义规则
    Regex { pattern: String },
    /// 不分大小写的子串：内容过滤的关键词规则
    Contains { text: String },
    /// 这几段码位里的字符，一项一个。内置规则是规范写法（`U+200B`、`U+E0000–U+E007F`），
    /// 自定义规则是它存着的写法（各项用 `, ` 连起来就是存着的那一份的意思）
    Codepoints { ranges: Vec<String> },
    /// 代码里实现的内置检查，没有可展示的模式：工具调用审查的「凭据发往陌生主机」
    /// （`credential-to-network`）、「上传本地文件到外部主机」（`file-to-network`）。
    /// `check` 是稳定的检查名，界面按它给出说明
    Builtin { check: String },
}

/// 一家卡组织认哪些卡号：以哪几段开头、一共几位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CardNetwork {
    /// 英文名（`UnionPay`、`Visa` …）。界面按它查自己的名称表
    pub name: String,
    pub prefixes: Vec<CardPrefix>,
    /// 一共几位
    pub lengths: Vec<u8>,
}

/// 卡号开头的一段，含两头、两头位数相同：`51`–`55`。只有一个数时两头相同。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct CardPrefix {
    pub from: u32,
    pub to: u32,
}

/// 一条规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "SecurityRuleView"))]
pub struct RuleView {
    /// 内置规则的 id，或者自定义规则的名字
    pub id: String,
    #[serde(default)]
    pub custom: bool,
    /// 英文名。界面按 id 查自己的名称表，查不到才用它；自定义规则就是名字
    pub name: String,
    /// 为什么值得看一眼（英文）。规则名说得清的、自定义规则没有
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
    /// 类别。出站脱敏：`api-keys` … `personal` / `internal` / `custom`；工具调用审查：
    /// `command` / `custom`；内容过滤：`invisible` / `injection` / `persona` / `chinese` /
    /// `custom`
    pub kind: String,
    pub matcher: Matcher,
    pub enabled: bool,
    /// 出厂时开不开。自定义规则是 `true`
    pub on_by_default: bool,
    /// 工具调用审查、内容过滤：第三档下做什么
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<RuleAction>,
    /// 内置规则出厂时第三档下做什么。和 `action` 不一样就是改过
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_action: Option<RuleAction>,
    /// 出站脱敏：占位符里的标签，`SECRET` 换成 `<<TW_SECRET_1>>`。内置和自定义的都有，
    /// 别的防护没有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// 一项防护的档位和规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct GuardDetail {
    /// `off` / `observe` / `enforce`
    pub mode: Mode,
    /// 按界面上的顺序：内置的在前，自定义的在后
    pub rules: Vec<RuleView>,
}

/// 三项防护。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct SecurityDetail {
    pub redact: GuardDetail,
    pub inspect_tools: GuardDetail,
    pub content: GuardDetail,
}

/// 一份策略的视图。
pub fn detail(s: &Security) -> SecurityDetail {
    SecurityDetail {
        redact: redact(&s.redact),
        inspect_tools: inspect_tools(&s.inspect_tools),
        content: content(&s.content),
    }
}

/// 一条内置脱敏规则按什么认
pub fn matcher(m: &redact_rules::Matcher) -> Matcher {
    use redact_rules::Matcher as M;
    match *m {
        M::Prefix { prefix, min_tail } => Matcher::Prefix {
            prefix: prefix.to_string(),
            min_tail,
        },
        M::OpenaiLegacy { min_len } => Matcher::OpenaiLegacy { min_len },
        M::Pem => Matcher::Pem,
        M::Jwt => Matcher::Jwt,
        M::ConnString => Matcher::ConnString,
        M::PrivateIp => Matcher::PrivateIp,
        M::DomainSuffix { suffixes } => Matcher::DomainSuffix {
            suffixes: suffixes.iter().map(|s| s.to_string()).collect(),
        },
        M::CnResidentId { born_since } => Matcher::CnResidentId { born_since },
        M::BankCard { networks } => Matcher::BankCard {
            networks: networks
                .iter()
                .map(|n| CardNetwork {
                    name: n.name.to_string(),
                    prefixes: n
                        .prefixes
                        .iter()
                        .map(|&(from, to)| CardPrefix { from, to })
                        .collect(),
                    lengths: n.lengths.to_vec(),
                })
                .collect(),
        },
        M::Email => Matcher::Email,
        M::CnMobilePhone => Matcher::CnMobilePhone,
    }
}

/// 内置脱敏规则的标签，去掉占位符里的前缀：`TW_ID_NUMBER` → `ID_NUMBER`；没写的是
/// [`DEFAULT_LABEL`]
fn builtin_label(b: &redact_rules::Builtin) -> String {
    b.label
        .map(|l| l.strip_prefix(LABEL_PREFIX).unwrap_or(l))
        .unwrap_or(DEFAULT_LABEL)
        .to_string()
}

/// 出站脱敏：全部内置规则（按界面上的顺序）和自定义规则。
pub fn redact(p: &RedactPolicy) -> GuardDetail {
    let mut rules: Vec<RuleView> = redact_rules::BUILTINS
        .iter()
        .map(|b| RuleView {
            id: b.id.to_string(),
            custom: false,
            name: b.name.to_string(),
            why: String::new(),
            kind: b.kind.slug().to_string(),
            matcher: matcher(&b.matcher),
            enabled: p.builtin_on(b),
            on_by_default: b.on_by_default,
            action: None,
            default_action: None,
            label: Some(builtin_label(b)),
        })
        .collect();
    rules.extend(p.custom.iter().map(|c| RuleView {
        id: c.name.clone(),
        custom: true,
        name: c.name.clone(),
        why: String::new(),
        kind: "custom".into(),
        matcher: Matcher::Regex {
            pattern: c.pattern.clone(),
        },
        enabled: !c.disabled,
        on_by_default: true,
        action: None,
        default_action: None,
        label: Some(c.label.clone().unwrap_or_else(|| DEFAULT_LABEL.to_string())),
    }));
    GuardDetail {
        mode: p.mode,
        rules,
    }
}

/// 工具调用审查：内置的危险命令规则和自定义规则。
pub fn inspect_tools(p: &ToolPolicy) -> GuardDetail {
    let builtin = &crate::tools::rules::builtin().dangerous;
    let mut rules: Vec<RuleView> = builtin
        .iter()
        .map(|r| RuleView {
            id: r.id.clone(),
            custom: false,
            name: r.name.clone(),
            why: r.why.clone(),
            kind: "command".into(),
            // 代码实现的规则没有可展示的正则，给界面一个专门的 matcher
            matcher: match &r.check {
                Some(check) => Matcher::Builtin {
                    check: check.clone(),
                },
                None => Matcher::Regex {
                    pattern: r.pattern.clone(),
                },
            },
            enabled: !p.disable.contains(&r.id),
            on_by_default: true,
            action: Some(p.builtin_action(r).into()),
            default_action: Some(ToolAction::factory(r).into()),
            label: None,
        })
        .collect();
    rules.extend(p.custom.iter().map(|c| RuleView {
        id: c.name.clone(),
        custom: true,
        name: c.name.clone(),
        why: String::new(),
        kind: "custom".into(),
        matcher: Matcher::Regex {
            pattern: c.pattern.clone(),
        },
        enabled: !c.disabled,
        on_by_default: true,
        action: Some(c.action.into()),
        default_action: None,
        label: None,
    }));
    GuardDetail {
        mode: p.mode,
        rules,
    }
}

/// 内容规则按什么认。码位：内置规则写成规范写法，自定义规则照它写的样子一项一项给
/// （界面拼回去存的就是这一份）；写坏了的（绕过校验写进去的）整段原样给
fn content_matcher(matching: content::Match, pattern: &str, custom: bool) -> Matcher {
    match matching {
        content::Match::Contains => Matcher::Contains {
            text: pattern.to_string(),
        },
        content::Match::Regex => Matcher::Regex {
            pattern: pattern.to_string(),
        },
        content::Match::Codepoints => Matcher::Codepoints {
            ranges: match Codepoints::parse(pattern) {
                Ok(c) if custom => c.written().to_vec(),
                Ok(c) => c.canonical(),
                Err(_) => vec![pattern.to_string()],
            },
        },
    }
}

/// 内容过滤：全部内置规则（隐藏字符一组在最前）和自定义规则。
pub fn content(p: &ContentPolicy) -> GuardDetail {
    let mut rules: Vec<RuleView> = content::builtins()
        .iter()
        .map(|b| RuleView {
            id: b.id.clone(),
            custom: false,
            name: b.name.clone(),
            why: b.why.clone(),
            kind: b.group.clone(),
            matcher: content_matcher(b.matching, &b.pattern, false),
            enabled: p.builtin_on(b),
            on_by_default: b.on_by_default,
            action: Some(p.builtin_action(b).into()),
            default_action: Some(ContentAction::factory(b).into()),
            label: None,
        })
        .collect();
    rules.extend(p.custom.iter().map(|c| RuleView {
        id: c.name.clone(),
        custom: true,
        name: c.name.clone(),
        why: String::new(),
        kind: "custom".into(),
        matcher: content_matcher(c.matching.engine(), &c.pattern, true),
        enabled: !c.disabled,
        on_by_default: true,
        action: Some(c.action.into()),
        default_action: None,
        label: None,
    }));
    GuardDetail {
        mode: p.mode,
        rules,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{CustomContentRule, CustomRedactRule, CustomToolRule};

    #[test]
    fn every_builtin_redaction_rule_is_listed_with_its_default_and_its_label() {
        let v = redact(&Default::default());
        assert_eq!(v.mode, Mode::Observe);
        assert_eq!(v.rules.len(), redact_rules::BUILTINS.len());
        let ip = v.rules.iter().find(|r| r.id == "internal-ip").unwrap();
        assert!(!ip.enabled && !ip.on_by_default);
        let key = v
            .rules
            .iter()
            .find(|r| r.id == "anthropic-api-key")
            .unwrap();
        assert!(key.enabled);
        assert_eq!(key.label.as_deref(), Some("SECRET"));
        assert_eq!(
            key.matcher,
            Matcher::Prefix {
                prefix: "sk-ant-".into(),
                min_tail: 20
            }
        );
        let id = v.rules.iter().find(|r| r.id == "cn-resident-id").unwrap();
        assert!(id.enabled && id.on_by_default);
        assert_eq!(id.kind, "personal");
        assert_eq!(id.label.as_deref(), Some("ID_NUMBER"));
        assert_eq!(id.matcher, Matcher::CnResidentId { born_since: 1900 });
        let card = v.rules.iter().find(|r| r.id == "bank-card").unwrap();
        let Matcher::BankCard { networks } = &card.matcher else {
            panic!("{:?}", card.matcher);
        };
        let amex = networks
            .iter()
            .find(|n| n.name == "American Express")
            .unwrap();
        assert_eq!(
            amex.prefixes,
            vec![
                CardPrefix { from: 34, to: 34 },
                CardPrefix { from: 37, to: 37 }
            ]
        );
        assert_eq!(amex.lengths, vec![15]);
        // 邮箱和手机号出厂关着，标签说明白是什么
        for (rule, label, matcher) in [
            ("email", "EMAIL", Matcher::Email),
            ("cn-mobile-phone", "PHONE", Matcher::CnMobilePhone),
        ] {
            let r = v.rules.iter().find(|r| r.id == rule).unwrap();
            assert!(
                !r.enabled && !r.on_by_default && r.kind == "personal",
                "{rule}"
            );
            assert_eq!(r.label.as_deref(), Some(label));
            assert_eq!(r.matcher, matcher);
        }
    }

    #[test]
    fn switched_rules_and_custom_labels_show_up_as_they_are() {
        let v = redact(&RedactPolicy {
            enable: vec!["email".into()],
            disable: vec!["jwt".into()],
            custom: vec![
                CustomRedactRule {
                    name: "项目号".into(),
                    pattern: r"PRJ-\d{6}".into(),
                    label: Some("PROJECT".into()),
                    disabled: false,
                },
                CustomRedactRule {
                    name: "不写标签".into(),
                    pattern: "x".into(),
                    label: None,
                    disabled: true,
                },
            ],
            ..Default::default()
        });
        let r = |id: &str| v.rules.iter().find(|r| r.id == id).unwrap().clone();
        assert!(r("email").enabled && !r("jwt").enabled);
        assert_eq!(r("项目号").label.as_deref(), Some("PROJECT"));
        assert_eq!(r("不写标签").label.as_deref(), Some("SECRET"));
        assert!(!r("不写标签").enabled && r("不写标签").custom);
    }

    #[test]
    fn tool_rules_say_what_they_do_in_the_third_mode() {
        let v = inspect_tools(&ToolPolicy {
            actions: [("rm-rf-root".to_string(), ToolAction::Cut)].into(),
            custom: vec![CustomToolRule {
                name: "删除集群资源".into(),
                pattern: r"kubectl\s+delete".into(),
                action: ToolAction::Cut,
                disabled: true,
            }],
            ..Default::default()
        });
        let curl = v.rules.iter().find(|r| r.id == "curl-pipe-sh").unwrap();
        assert_eq!(curl.action, Some(RuleAction::Cut));
        assert!(!curl.why.is_empty());
        let rm = v.rules.iter().find(|r| r.id == "rm-rf-root").unwrap();
        assert_eq!(
            (rm.action, rm.default_action),
            (Some(RuleAction::Cut), Some(RuleAction::Record))
        );
        let mine = v.rules.last().unwrap();
        assert!(mine.custom && !mine.enabled && mine.label.is_none());
        assert_eq!(mine.action, Some(RuleAction::Cut));
    }

    #[test]
    fn the_code_backed_tool_rules_are_listed_with_a_builtin_matcher() {
        // 代码实现的两条规则（凭据外传、上传本地文件）在规则表里照样列得出来：
        // 带专门的 matcher（没有正则可展示），处置按出厂（A 切断、B 仅记录）
        let v = inspect_tools(&ToolPolicy::default());
        let a = v
            .rules
            .iter()
            .find(|r| r.id == "secret-to-unknown-host")
            .expect("凭据外传规则应当在表里");
        assert_eq!(
            a.matcher,
            Matcher::Builtin {
                check: "credential-to-network".into()
            }
        );
        assert_eq!(a.action, Some(RuleAction::Cut), "高危，拦截档下切断");
        assert!(!a.why.is_empty());
        let b = v
            .rules
            .iter()
            .find(|r| r.id == "upload-file-to-host")
            .expect("上传文件规则应当在表里");
        assert_eq!(
            b.matcher,
            Matcher::Builtin {
                check: "file-to-network".into()
            }
        );
        assert_eq!(b.action, Some(RuleAction::Record), "出厂只记录");
        // 经过一趟 JSON 还认得回来
        let json = serde_json::to_value(&a.matcher).unwrap();
        assert_eq!(json["kind"], "builtin");
        assert_eq!(json["check"], "credential-to-network");
    }

    #[test]
    fn content_rules_list_the_hidden_characters_first_with_their_code_points() {
        let v = content(&ContentPolicy {
            enable: vec!["zero-width".into()],
            actions: [("unicode-tags".to_string(), ContentAction::Block)].into(),
            custom: vec![CustomContentRule {
                name: "码位".into(),
                pattern: "u+e000-u+f8ff, u+fffd".into(),
                matching: crate::policy::ContentMatch::Codepoints,
                action: ContentAction::Strip,
                disabled: false,
            }],
            ..Default::default()
        });
        let first: Vec<&str> = v.rules.iter().take(4).map(|r| r.id.as_str()).collect();
        assert_eq!(
            first,
            ["unicode-tags", "bidi-controls", "zero-width", "private-use"]
        );
        let tags = &v.rules[0];
        assert_eq!(tags.kind, "invisible");
        assert!(!tags.why.is_empty());
        assert_eq!(
            tags.matcher,
            Matcher::Codepoints {
                ranges: vec!["U+E0000–U+E007F".into()]
            }
        );
        assert_eq!(
            (tags.action, tags.default_action),
            (Some(RuleAction::Block), Some(RuleAction::Strip))
        );
        assert!(
            v.rules[2].enabled && !v.rules[2].on_by_default,
            "打开的出厂关着的"
        );
        assert!(!v.rules[3].enabled);
        // 出厂写 warn 的是仅记录
        let act = v.rules.iter().find(|r| r.id == "act-as").unwrap();
        assert_eq!(act.default_action, Some(RuleAction::Record));
        assert_eq!(
            v.rules
                .iter()
                .find(|r| r.id == "jailbreak")
                .unwrap()
                .matcher,
            Matcher::Contains {
                text: "jailbreak".into()
            }
        );
        let mine = v.rules.last().unwrap();
        assert_eq!(
            mine.matcher,
            Matcher::Codepoints {
                ranges: vec!["u+e000-u+f8ff".into(), "u+fffd".into()]
            },
            "自定义规则照它写的样子给"
        );
        assert_eq!(mine.action, Some(RuleAction::Strip));
    }

    /// 企业版的界面按视图拼回整份策略、整键写回：拼回来的得和原来的一样
    #[test]
    fn a_policy_rebuilt_from_its_view_is_the_same_policy() {
        use crate::policy::{
            ContentMatch, CustomContentRule, CustomRedactRule, CustomToolRule, ToolAction,
        };
        let original = Security {
            redact: RedactPolicy {
                mode: Mode::Enforce,
                enable: vec!["email".into()],
                disable: vec!["jwt".into()],
                custom: vec![
                    CustomRedactRule {
                        name: "项目号".into(),
                        pattern: r"PRJ-\d{6}".into(),
                        label: Some("PROJECT".into()),
                        disabled: false,
                    },
                    CustomRedactRule {
                        name: "不写标签".into(),
                        pattern: "x".into(),
                        label: None,
                        disabled: true,
                    },
                ],
            },
            inspect_tools: ToolPolicy {
                mode: Mode::Off,
                disable: vec!["chmod-777".into()],
                // 停用着的那条也改过处置：视图里照样要带着
                actions: [
                    ("chmod-777".to_string(), ToolAction::Cut),
                    ("rm-rf-root".to_string(), ToolAction::Cut),
                ]
                .into(),
                custom: vec![CustomToolRule {
                    name: "删除集群资源".into(),
                    pattern: r"kubectl\s+delete".into(),
                    action: ToolAction::Cut,
                    disabled: false,
                }],
                ..Default::default()
            },
            content: ContentPolicy {
                mode: Mode::Enforce,
                enable: vec!["zero-width".into()],
                disable: vec!["unicode-tags".into()],
                actions: [
                    ("unicode-tags".to_string(), ContentAction::Block),
                    ("act-as".to_string(), ContentAction::Strip),
                ]
                .into(),
                custom: vec![
                    CustomContentRule {
                        name: "关键词".into(),
                        pattern: " project-x ".into(),
                        matching: ContentMatch::Contains,
                        action: ContentAction::Block,
                        disabled: false,
                    },
                    CustomContentRule {
                        name: "正则".into(),
                        pattern: r"secret\s+plan".into(),
                        matching: ContentMatch::Regex,
                        action: ContentAction::Record,
                        disabled: true,
                    },
                    CustomContentRule {
                        name: "码位".into(),
                        pattern: "u+e000-u+f8ff，U+FFFD".into(),
                        matching: ContentMatch::Codepoints,
                        action: ContentAction::Strip,
                        disabled: false,
                    },
                ],
            },
        };
        // 经过一趟 JSON，像界面拿到的那样
        let view: SecurityDetail =
            serde_json::from_value(serde_json::to_value(detail(&original)).unwrap()).unwrap();

        fn lists(rules: &[RuleView]) -> (Vec<String>, Vec<String>) {
            let builtin = rules.iter().filter(|r| !r.custom);
            let enable = builtin
                .clone()
                .filter(|r| r.enabled && !r.on_by_default)
                .map(|r| r.id.clone())
                .collect();
            let disable = builtin
                .filter(|r| !r.enabled && r.on_by_default)
                .map(|r| r.id.clone())
                .collect();
            (enable, disable)
        }
        fn changed<A: Copy>(
            rules: &[RuleView],
            of: impl Fn(RuleAction) -> Option<A>,
        ) -> std::collections::BTreeMap<String, A> {
            rules
                .iter()
                .filter(|r| !r.custom && r.action != r.default_action)
                .map(|r| (r.id.clone(), of(r.action.unwrap()).unwrap()))
                .collect()
        }
        let pattern = |m: &Matcher| match m {
            Matcher::Regex { pattern } => (pattern.clone(), ContentMatch::Regex),
            Matcher::Contains { text } => (text.clone(), ContentMatch::Contains),
            Matcher::Codepoints { ranges } => (ranges.join(", "), ContentMatch::Codepoints),
            other => panic!("自定义规则不会是 {other:?}"),
        };

        let (enable, disable) = lists(&view.redact.rules);
        let redact = RedactPolicy {
            mode: view.redact.mode,
            enable,
            disable,
            custom: view
                .redact
                .rules
                .iter()
                .filter(|r| r.custom)
                .map(|r| CustomRedactRule {
                    name: r.id.clone(),
                    pattern: pattern(&r.matcher).0,
                    label: r.label.clone(),
                    disabled: !r.enabled,
                })
                .collect(),
        };
        let (enable, disable) = lists(&view.inspect_tools.rules);
        let inspect_tools = ToolPolicy {
            mode: view.inspect_tools.mode,
            enable,
            disable,
            actions: changed(&view.inspect_tools.rules, RuleAction::tool),
            custom: view
                .inspect_tools
                .rules
                .iter()
                .filter(|r| r.custom)
                .map(|r| CustomToolRule {
                    name: r.id.clone(),
                    pattern: pattern(&r.matcher).0,
                    action: r.action.and_then(RuleAction::tool).unwrap(),
                    disabled: !r.enabled,
                })
                .collect(),
        };
        let (enable, disable) = lists(&view.content.rules);
        let content = ContentPolicy {
            mode: view.content.mode,
            enable,
            disable,
            actions: changed(&view.content.rules, RuleAction::content),
            custom: view
                .content
                .rules
                .iter()
                .filter(|r| r.custom)
                .map(|r| {
                    let (pattern, matching) = pattern(&r.matcher);
                    CustomContentRule {
                        name: r.id.clone(),
                        pattern,
                        matching,
                        action: r.action.and_then(RuleAction::content).unwrap(),
                        disabled: !r.enabled,
                    }
                })
                .collect(),
        };
        // 界面写回去的 JSON，存进去再读出来
        let rebuilt: Security = serde_json::from_value(
            serde_json::to_value(Security {
                redact,
                inspect_tools,
                content,
            })
            .unwrap(),
        )
        .unwrap();
        // 码位换了分隔符写，意思一样；别的一字不差
        let codepoints = |s: &Security| {
            s.content
                .custom
                .iter()
                .find(|c| c.matching == ContentMatch::Codepoints)
                .map(|c| Codepoints::parse(&c.pattern).unwrap().canonical())
        };
        assert_eq!(codepoints(&rebuilt), codepoints(&original));
        let mut a = original.clone();
        let mut b = rebuilt.clone();
        for s in [&mut a, &mut b] {
            for c in &mut s.content.custom {
                if c.matching == ContentMatch::Codepoints {
                    c.pattern = Codepoints::parse(&c.pattern).unwrap().written().join(", ");
                }
            }
        }
        assert_eq!(a, b);
        rebuilt.check().unwrap();
    }

    #[test]
    fn the_json_is_the_one_both_products_send() {
        let v = serde_json::to_value(detail(&Security::default())).unwrap();
        assert_eq!(v["redact"]["mode"], "observe");
        let tags = &v["content"]["rules"][0];
        assert_eq!(tags["matcher"]["kind"], "codepoints");
        assert_eq!(tags["action"], "strip");
        assert!(tags.get("label").is_none(), "没有的不写");
        let key = &v["redact"]["rules"][0];
        assert_eq!(key["label"], "SECRET");
        assert!(key.get("action").is_none() && key.get("why").is_none());
        assert_eq!(
            v["redact"]["rules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == "cn-mobile-phone")
                .unwrap()["matcher"],
            serde_json::json!({"kind": "cn-mobile-phone"})
        );
        // 读得回来
        let back: SecurityDetail = serde_json::from_value(v).unwrap();
        assert_eq!(back, detail(&Security::default()));
        for &a in RuleAction::ALL {
            assert_eq!(RuleAction::from_slug(a.slug()), Some(a));
            assert_eq!(serde_json::to_value(a).unwrap(), a.slug());
        }
    }
}
