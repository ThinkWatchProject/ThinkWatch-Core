//! 规则目录（`data/redact.yaml`）和代码对得上：名字、说明、分组都齐，数据里每条代码判据都有
//! 实现、每个实现都在数据里。

use std::collections::HashSet;

use super::{BUILTINS, DETECTORS, KIND_NAMES, Kind, Matcher, builtin};

/// 界面文案的规矩：书面陈述，不出现「你」「我」
fn written(zh: &str) -> bool {
    !zh.contains('你') && !zh.contains('我')
}

#[test]
fn every_rule_has_a_name_and_says_what_it_matches_in_both_languages() {
    let mut ids = HashSet::new();
    for b in BUILTINS {
        assert!(ids.insert(b.id), "{} 出现了两次", b.id);
        for (field, text) in [
            ("name", b.name),
            ("name_zh", b.name_zh),
            ("about", b.about),
            ("about_zh", b.about_zh),
        ] {
            assert!(!text.trim().is_empty(), "{} 没有 {field}", b.id);
            // 反引号成对：界面按它切出代码样式的那几段
            assert_eq!(
                text.matches('`').count() % 2,
                0,
                "{} 的 {field}：{text}",
                b.id
            );
        }
        assert!(written(b.name_zh) && written(b.about_zh), "{}", b.id);
    }
}

#[test]
fn every_kind_has_names_in_the_order_the_ui_groups_by() {
    // 新加一个 Kind 而这里没跟上，下面这个 match 就编译不过
    let all = [
        Kind::Saved,
        Kind::AiServices,
        Kind::DevPlatforms,
        Kind::Cloud,
        Kind::Business,
        Kind::PrivateKeys,
        Kind::Jwt,
        Kind::ConnStrings,
        Kind::InContext,
        Kind::Crypto,
        Kind::IdDocuments,
        Kind::ForeignIds,
        Kind::Financial,
        Kind::Contact,
        Kind::Location,
        Kind::Vehicle,
        Kind::Device,
        Kind::LabeledPersonal,
        Kind::Internal,
        Kind::Custom,
    ];
    for k in all {
        match k {
            Kind::Saved
            | Kind::AiServices
            | Kind::DevPlatforms
            | Kind::Cloud
            | Kind::Business
            | Kind::PrivateKeys
            | Kind::Jwt
            | Kind::ConnStrings
            | Kind::InContext
            | Kind::Crypto
            | Kind::IdDocuments
            | Kind::ForeignIds
            | Kind::Financial
            | Kind::Contact
            | Kind::Location
            | Kind::Vehicle
            | Kind::Device
            | Kind::LabeledPersonal
            | Kind::Internal
            | Kind::Custom => {}
        }
    }
    let listed: Vec<Kind> = KIND_NAMES.iter().map(|k| k.kind).collect();
    assert_eq!(listed, all, "数据文件里分组的顺序就是 Kind 的顺序");
    for k in KIND_NAMES {
        assert!(!k.name.trim().is_empty() && !k.name_zh.trim().is_empty());
        assert!(written(k.name_zh));
        assert_eq!(k.kind.name(), k.name);
        assert_eq!(k.kind.name_zh(), k.name_zh);
    }
    assert_eq!(Kind::AiServices.name_zh(), "AI 服务");
}

/// 判据在代码里的规则：数据里每一条都在 [`DETECTORS`] 里（编译期就查过，这里再对一遍值），
/// [`DETECTORS`] 里每一条都在数据里
#[test]
fn every_detector_has_a_rule_in_the_data() {
    let mut seen = HashSet::new();
    for &(id, m) in DETECTORS {
        assert!(seen.insert(id), "{id} 在 DETECTORS 里出现了两次");
        let b = builtin(id).unwrap_or_else(|| panic!("{id} 有判据，数据文件里没有这条规则"));
        assert_eq!(b.matcher, m, "{id}");
        assert!(
            !matches!(m, Matcher::Prefix { .. } | Matcher::Prefixes { .. }),
            "{id}：前缀类的判据写在数据文件里"
        );
    }
    for b in BUILTINS {
        let prefix = matches!(b.matcher, Matcher::Prefix { .. } | Matcher::Prefixes { .. });
        assert_eq!(prefix, !seen.contains(b.id), "{}", b.id);
    }
}

/// 代码判据带着的那几张表（变量名结尾、字段名、域名后缀、卡组织）和说明里写的是同一份：
/// 表改了、说明没跟上，这里挂
#[test]
fn a_description_lists_what_its_detector_lists() {
    let quoted = |s: &str| format!("`{s}`");
    for b in BUILTINS {
        let (en, zh) = (b.about, b.about_zh);
        match b.matcher {
            Matcher::Assignment { suffixes: items }
            | Matcher::Labeled { names: items }
            | Matcher::DomainSuffix { suffixes: items } => {
                for i in items {
                    assert!(
                        en.contains(&quoted(i)) && zh.contains(&quoted(i)),
                        "{} 的说明里没有 {i}",
                        b.id
                    );
                }
            }
            Matcher::BankCard { networks } => {
                for n in networks {
                    assert!(en.contains(n.name), "{} 的说明里没有 {}", b.id, n.name);
                }
            }
            Matcher::OpenaiLegacy { min_len } => {
                let n = min_len.to_string();
                assert!(en.contains(&n) && zh.contains(&n), "{}", b.id);
            }
            Matcher::CnResidentId { born_since } => {
                let n = born_since.to_string();
                assert!(en.contains(&n) && zh.contains(&n), "{}", b.id);
            }
            _ => {}
        }
    }
}

/// 按位置认的只换值那一段，说明里要写明
#[test]
fn rules_that_replace_only_the_value_say_so() {
    for b in BUILTINS {
        let placed = matches!(
            b.matcher,
            Matcher::ConnString | Matcher::Assignment { .. } | Matcher::Labeled { .. }
        ) || matches!(
            b.id,
            "auth-header-credential" | "command-line-password" | "url-token" | "bot-webhook"
        );
        if placed {
            assert!(b.about.contains("only the"), "{}：{}", b.id, b.about);
            assert!(b.about_zh.contains("只换"), "{}：{}", b.id, b.about_zh);
        }
    }
}

/// 前缀类规则的说明是按前缀生成的：尾巴要求一样的连成一段
#[test]
fn a_prefix_rule_is_described_from_its_prefixes() {
    let vault = builtin("vault-token").unwrap();
    assert_eq!(
        vault.about,
        "Starts with `hvs.`, followed by at least 90 characters; \
         starts with `hvb.`, followed by at least 138 characters"
    );
    assert_eq!(
        vault.about_zh,
        "`hvs.` 开头，其后至少 90 个字符；`hvb.` 开头，其后至少 138 个字符"
    );
    let shopify = builtin("shopify-token").unwrap();
    assert_eq!(
        shopify.about,
        "Starts with `shpat_`, `shpca_`, `shppa_` or `shpss_`, followed by at least 32 hex characters"
    );
    let key = builtin("anthropic-api-key").unwrap();
    assert_eq!(
        key.about,
        "Starts with `sk-ant-`, followed by at least 20 characters"
    );
    assert_eq!(key.about_zh, "`sk-ant-` 开头，其后至少 20 个字符");
}

/// 服务商域名只给拿到就能用的凭据写：别的规则不参与「凭据发往陌生主机」，写了也没人用
#[test]
fn only_credentials_name_their_provider_hosts() {
    for b in BUILTINS {
        if !b.hosts.is_empty() {
            assert!(b.kind.is_credential(), "{}", b.id);
        }
        for h in b.hosts {
            assert!(
                !h.is_empty() && !h.starts_with('.') && h.contains('.') && *h == h.to_lowercase(),
                "{}：{h}",
                b.id
            );
        }
    }
    assert_eq!(
        builtin("anthropic-api-key").unwrap().hosts,
        ["anthropic.com"]
    );
    assert!(builtin("private-key").unwrap().hosts.is_empty());
}
