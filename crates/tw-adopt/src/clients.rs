//! 本机上有哪些 AI 客户端（DESIGN.md §7.11）。
//!
//! **两张表的成员不一样。**「能接管 API 端点」和「有 MCP 要管」是两件
//! 事：Claude Code 两张表都在，Claude Desktop 只在第二张（它是订阅制，
//! 接管不了，但它的 MCP 配置是危险度第二高的攻击面）。初稿把两张表混成
//! 一张，就漏掉了后者 —— 而漏掉它等于扫描留了个洞。
//!
//! 这个文件只管第一张表。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::json::Val;

/// 配置文件是什么格式。**决定了怎么做字段级合并，以及哨兵往哪儿放。**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    /// 严格 JSON，**装不下注释** —— 哨兵退化成同目录的旁文件（§7.15）
    Json,
    Toml,
    Yaml,
}

/// 配置改完什么时候生效。
///
/// **这个差别真的会让用户困惑**（§7.11），而它直接决定观察窗口的行为：
/// 对需要重开终端的客户端，「五分钟没收到请求」是完全正常的 —— 用户
/// 可能一整天都没重开过终端。那时弹「是不是没生效」是狼来了。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TakesEffect {
    /// 热重载，下一个请求就走新配置
    Immediately,
    /// **必须关掉终端重开**，否则永远不生效
    OnRestart,
}

impl TakesEffect {
    /// 接管完成那一屏要说的话。**在那一刻说，不是等五分钟后再说。**
    pub fn note(&self) -> &'static str {
        match self {
            TakesEffect::Immediately => "下一个请求就会走新配置。",
            TakesEffect::OnRestart => "需要关掉终端重开才生效 —— 在那之前收不到请求是正常的。",
        }
    }
    /// 该不该设「还没收到请求」的超时提示。
    pub fn warns_when_silent(&self) -> bool {
        matches!(self, TakesEffect::Immediately)
    }
}

/// 我们对这一条了解到什么程度。**要显示在界面上。**
///
/// DESIGN.md §7.11 的表格自己就标了「前五个在这台机器上实测存在，后四个
/// 是查证过字段但本机没装」。把这个区别丢掉，等于把「我跑过」和「我读过
/// 文档」说成同一件事 —— 而它们出错的概率差一个数量级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verified {
    /// 在本机用一个本地嗅探器实跑验证过：请求真的到了，头长这样
    Measured,
    /// 字段名从上游二进制或文档里查过，但没有实跑
    FieldsOnly,
}

impl Verified {
    pub fn note(&self) -> &'static str {
        match self {
            Verified::Measured => "本机实测过",
            Verified::FieldsOnly => "字段查证过，但没有在本机实跑验证",
        }
    }
}

/// 一个能接管的客户端。
#[derive(Debug, Clone)]
pub struct Client {
    pub id: &'static str,
    pub name: &'static str,
    /// 配置文件，相对 `$HOME`
    pub config: &'static str,
    pub format: Format,
    pub takes_effect: TakesEffect,
    /// 优先级比主配置更高、会盖住我们的那些文件（§7.11 的诊断链）。
    ///
    /// **检测阶段就要扫**：cc-switch 的 #6828 就是栽在
    /// `settings.local.json` 上 —— 我们写了 `settings.json`，而那边的
    /// 残留把它遮住了，用户看到的是「接管了但没生效」。
    pub shadowed_by: &'static [&'static str],
    /// 接管的代价。**接管确认对话框要把它们列出来，不能等用户自己发现**
    /// —— 这些不是我们的 bug，但用户会算到我们头上（§7.11）。
    pub costs: &'static [&'static str],
    pub verified: Verified,
}

/// 网关这一侧的地址和钥匙。
#[derive(Debug, Clone)]
pub struct Gateway {
    /// 形如 `http://127.0.0.1:8080`，**不带尾斜杠、不带 `/v1`**
    pub base: String,
    /// 给这个客户端的专属密钥。`None` = 网关不要求鉴权。
    ///
    /// 专属密钥的意义在于**客户端识别**，这样规则里才能写
    /// `when: { client: claude-code }`（§7.11）。
    pub key: Option<String>,
}

impl Gateway {
    fn v1(&self) -> String {
        format!("{}/v1", self.base.trim_end_matches('/'))
    }
}

/// 我们要往配置里写的一个字段。
///
/// **这张表就是那个白名单，而它枚举的是「我们要写什么」** —— 有限、封闭、
/// 不会增长。cc-switch 的白名单枚举的是「要保留什么」，那是个它不控制、
/// 还在增长的集合，所以 147 个 commit 之后整个撤回了（§7.11）。
#[derive(Debug, Clone)]
pub struct Edit {
    pub path: Vec<String>,
    pub value: Val,
    /// 这个值是密钥。决定它进不进旁文件、要不要提示权限。
    pub secret: bool,
}

fn e(path: &[&str], value: Val) -> Edit {
    Edit {
        path: path.iter().map(|s| s.to_string()).collect(),
        value,
        secret: false,
    }
}
fn secret(path: &[&str], value: Val) -> Edit {
    Edit {
        path: path.iter().map(|s| s.to_string()).collect(),
        value,
        secret: true,
    }
}

/// 我们给自己在各客户端里用的 provider id。
///
/// Codex 的 `openai` / `ollama` / `lmstudio` 是保留 id，不能撞（§7.11）。
pub const PROVIDER_ID: &str = "thinkwatch";

/// 表一：能接管 API 端点的。
///
/// 字段名都对应上游当前文档，不是猜的（§7.11）。
pub fn adoptable() -> Vec<Client> {
    vec![
        Client {
            id: "claude-code",
            name: "Claude Code",
            config: ".claude/settings.json",
            format: Format::Json,
            takes_effect: TakesEffect::Immediately,
            // **`settings.local.json` 优先级更高。**cc-switch #6828 栽在这里
            shadowed_by: &[".claude/settings.local.json"],
            costs: &[
                "Remote Control 和语音输入会被禁用 —— 只要 base URL 不是官方域名就会。",
                "MCP tool search 默认关闭。",
                "它可能会弹一次自己的欢迎页，点掉就行。",
            ],
            verified: Verified::FieldsOnly,
        },
        Client {
            id: "codex",
            name: "Codex CLI",
            config: ".codex/config.toml",
            format: Format::Toml,
            // **读环境变量的，必须关掉终端重开**（§7.11）
            takes_effect: TakesEffect::OnRestart,
            // 项目级的 .codex/config.toml 会忽略 model_provider，
            // 所以它不构成遮蔽 —— 但它确实存在，值得在诊断里提一句
            shadowed_by: &[],
            costs: &[
                "它不问我们要模型列表，所以自定义模型名对它无效 —— 那个清单来自本地的模型目录文件。",
                "改完要关掉终端重开。",
            ],
            verified: Verified::Measured,
        },
        Client {
            id: "opencode",
            name: "opencode",
            config: ".config/opencode/opencode.json",
            format: Format::Json,
            takes_effect: TakesEffect::OnRestart,
            shadowed_by: &[],
            costs: &["改完要重开。"],
            verified: Verified::FieldsOnly,
        },
        Client {
            id: "zed",
            name: "Zed",
            config: ".config/zed/settings.json",
            format: Format::Json,
            takes_effect: TakesEffect::Immediately,
            shadowed_by: &[],
            // **只算部分接管**：Zed 的密钥走它自己的凭据存储，不在
            // settings.json 里，我们写不进去。
            costs: &["密钥要你自己在 Zed 的界面里填一次 —— 它不放在配置文件里，我们够不着。"],
            verified: Verified::FieldsOnly,
        },
        Client {
            id: "aider",
            name: "Aider",
            config: ".aider.conf.yml",
            format: Format::Yaml,
            takes_effect: TakesEffect::OnRestart,
            // **三层查找，后面的覆盖前面的**（home → 仓库根 → cwd）。
            // 我们只写 home 那一份，所以项目里的会盖住它
            shadowed_by: &[],
            costs: &[
                "它按 home → 仓库根 → 当前目录的顺序找配置，后面的会盖住前面的 —— 我们只写 home 那一份。",
                "改完要重开。",
            ],
            verified: Verified::FieldsOnly,
        },
    ]
}

/// 接管不了、只能给指引的（§7.11）。
///
/// **不假装能接管。**Cursor 没有可写的配置文件，而且即使手动改了，
/// Tab 补全和 inline edit 仍然走它自己的后端 —— 显示成「已接管」会让
/// 用户以为所有流量都在我们这儿。
pub struct ManualOnly {
    pub name: &'static str,
    pub how: &'static str,
    pub caveat: &'static str,
}

pub fn manual_only() -> Vec<ManualOnly> {
    vec![
        ManualOnly {
            name: "Cursor",
            how: "设置 → Models → Override OpenAI Base URL，填我们的地址。",
            caveat: "即使改了，Tab 补全和 inline edit 仍然走 Cursor 自己的后端，不会经过我们。所以它只能算「部分接管」。",
        },
        ManualOnly {
            name: "Continue",
            how: "在 ~/.continue/config.yaml 的 models: 列表里加一条，把 apiBase 指向我们的地址。",
            // **接管它要往一个 YAML 列表里插一个新条目**，那是结构性
            // 改写，不是替换一个标量。我们的 YAML 补丁只做后者
            // （见 crate::yaml 开头那段）。宁可少接管一个客户端，也不
            // 要写一段我们自己没把握的结构。
            caveat: "它的接管要往一个 YAML 列表里插新条目，属于结构性改写 —— 我们的增量写入只做「替换一个已有的值」，所以这一条给指引不代劳。",
        },
        ManualOnly {
            name: "Gemini CLI",
            how: "在你的 shell 配置里 export GOOGLE_GEMINI_BASE_URL=<我们的地址>。",
            // 它只认环境变量，没有可写的配置字段。改 .zshrc 超出了
            // 「只改 endpoint 和 key 字段」的边界（§7.11）——
            // **报告是我们的职责，修改是他的权利。**
            caveat: "它只读环境变量，没有可写的配置字段。改 shell 配置文件超出了我们该动的范围，所以这一条只给命令，不代劳。",
        },
    ]
}

/// 接管这个客户端要写哪些字段。
pub fn edits(client: &Client, gw: &Gateway) -> Vec<Edit> {
    match client.id {
        // 统一写 `ANTHROPIC_AUTH_TOKEN` 而不是 `ANTHROPIC_API_KEY`：
        // 后者在交互模式下要用户去 /config 点一次确认，**被拒绝是静默
        // 忽略的** —— 接管会看起来「没生效」却查不出原因（§7.11）。
        "claude-code" => {
            let mut v = vec![
                e(&["env", "ANTHROPIC_BASE_URL"], Val::s(&gw.base)),
                // 不设它，Claude Code 根本不会来问我们的 /v1/models（§3.9）
                e(
                    &["env", "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"],
                    Val::s("1"),
                ),
            ];
            if let Some(k) = &gw.key {
                v.push(secret(&["env", "ANTHROPIC_AUTH_TOKEN"], Val::s(k)));
            }
            v
        }
        // 下面这几个字段名是从 codex 0.139.0 的 ModelProviderInfo 里读出来
        // 的，并且用一个本地嗅探器实跑验证过：请求真的落在
        // `POST /v1/responses`，`http_headers` 原样送达，
        // `experimental_bearer_token` 变成 `Authorization: Bearer`。
        // 一个 GET 都没有 —— 它确实不问我们要模型列表（§3.9）。
        "codex" => {
            let p = |k: &str| {
                vec![
                    "model_providers".to_string(),
                    PROVIDER_ID.to_string(),
                    k.to_string(),
                ]
            };
            let mut v = vec![
                Edit {
                    path: vec!["model_provider".into()],
                    value: Val::s(PROVIDER_ID),
                    secret: false,
                },
                Edit {
                    path: p("name"),
                    value: Val::s("ThinkWatch"),
                    secret: false,
                },
                Edit {
                    path: p("base_url"),
                    value: Val::s(gw.v1()),
                    secret: false,
                },
                // `wire_api = "chat"` 已经被上游移除，只剩 responses
                Edit {
                    path: p("wire_api"),
                    value: Val::s("responses"),
                    secret: false,
                },
                // **不写 `env_key`。**实测：不配任何密钥它也照发请求；
                // 而 env_key 指向一个没 export 的变量反而会让它起不来。
                Edit {
                    path: p("http_headers"),
                    value: Val::Obj(vec![("X-ThinkWatch-Client".into(), Val::s("codex"))]),
                    secret: false,
                },
            ];
            if let Some(k) = &gw.key {
                // 名字带 experimental_，上游可能改。**改了也只是丢掉鉴权，
                // 上面那个 http_headers 仍然认得出是谁发的。**
                v.push(Edit {
                    path: p("experimental_bearer_token"),
                    value: Val::s(k),
                    secret: true,
                });
            }
            v
        }
        "opencode" => {
            let p = |k: &str| {
                vec![
                    "provider".to_string(),
                    PROVIDER_ID.to_string(),
                    "options".to_string(),
                    k.to_string(),
                ]
            };
            let mut v = vec![
                Edit {
                    path: vec!["provider".into(), PROVIDER_ID.into(), "name".into()],
                    value: Val::s("ThinkWatch"),
                    secret: false,
                },
                Edit {
                    path: p("baseURL"),
                    value: Val::s(gw.v1()),
                    secret: false,
                },
            ];
            if let Some(k) = &gw.key {
                v.push(Edit {
                    path: p("apiKey"),
                    value: Val::s(k),
                    secret: true,
                });
            }
            v
        }
        // 密钥不在这里 —— Zed 走它自己的凭据存储，所以这条只写端点
        "zed" => vec![e(
            &[
                "language_models",
                "openai_compatible",
                "ThinkWatch",
                "api_url",
            ],
            Val::s(gw.v1()),
        )],
        "aider" => {
            let mut v = vec![e(&["openai-api-base"], Val::s(gw.v1()))];
            if let Some(k) = &gw.key {
                v.push(secret(&["openai-api-key"], Val::s(k)));
            }
            v
        }
        _ => Vec::new(),
    }
}

impl Client {
    pub fn config_path(&self, home: &std::path::Path) -> PathBuf {
        home.join(self.config)
    }
    pub fn shadow_paths(&self, home: &std::path::Path) -> Vec<PathBuf> {
        self.shadowed_by.iter().map(|p| home.join(p)).collect()
    }
    /// 注释前缀。JSON 没有 —— 那时哨兵走旁文件。
    pub fn comment_prefix(&self) -> Option<&'static str> {
        match self.format {
            Format::Json => None,
            Format::Toml | Format::Yaml => Some("#"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_client_has_a_distinct_id_and_a_real_path() {
        let cs = adoptable();
        let mut ids: Vec<_> = cs.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "有重复的 id");
        for c in &cs {
            assert!(
                !c.config.starts_with('/'),
                "{} 的路径该是相对 home 的",
                c.id
            );
            assert!(!c.name.is_empty());
        }
    }

    #[test]
    fn the_clients_that_need_a_restart_do_not_get_a_silence_warning() {
        // **对需要重开终端的客户端，五分钟收不到请求是完全正常的** ——
        // 用户可能一整天都没重开过终端。那时弹「是不是没生效」是狼来了，
        // 被误报几次之后真正该看的那次也不会看了（§7.11）。
        for c in adoptable() {
            if c.takes_effect == TakesEffect::OnRestart {
                assert!(
                    !c.takes_effect.warns_when_silent(),
                    "{} 需要重开却设了超时提示",
                    c.id
                );
                assert!(
                    c.takes_effect.note().contains("重开"),
                    "{} 没在接管那一刻说清要重开",
                    c.id
                );
            }
        }
    }

    #[test]
    fn claude_code_knows_about_the_file_that_shadows_it() {
        // cc-switch 的 #6828 就是栽在这里：我们写了 settings.json，而
        // settings.local.json 里的残留把它遮住了。
        let cc = adoptable()
            .into_iter()
            .find(|c| c.id == "claude-code")
            .unwrap();
        assert!(
            cc.shadowed_by.iter().any(|p| p.contains("settings.local")),
            "{:?}",
            cc.shadowed_by
        );
    }

    #[test]
    fn adoption_costs_are_stated_where_they_exist() {
        // **接管确认对话框要把它们列出来，不能等用户自己发现** ——
        // 这些不是我们的 bug，但用户会算到我们头上（§7.11）。
        let cc = adoptable()
            .into_iter()
            .find(|c| c.id == "claude-code")
            .unwrap();
        assert!(!cc.costs.is_empty());
        assert!(
            cc.costs.iter().any(|c| c.contains("Remote Control")),
            "{:?}",
            cc.costs
        );
    }

    #[test]
    fn json_clients_have_no_comment_prefix_so_they_use_the_sidecar() {
        // 严格 JSON 装不下注释 —— 哨兵退化成同目录的旁文件（§7.15）。
        for c in adoptable() {
            match c.format {
                Format::Json => assert!(c.comment_prefix().is_none(), "{}", c.id),
                _ => assert!(c.comment_prefix().is_some(), "{}", c.id),
            }
        }
    }

    #[test]
    fn cursor_is_manual_only_and_says_why() {
        // **不假装能接管。**显示成「已接管」会让用户以为所有流量都在
        // 我们这儿，而 Tab 补全根本不经过（§7.11）。
        let m = manual_only();
        let cursor = m.iter().find(|c| c.name == "Cursor").unwrap();
        assert!(cursor.caveat.contains("Tab 补全"), "{}", cursor.caveat);
        assert!(
            !adoptable().iter().any(|c| c.name == "Cursor"),
            "Cursor 不该在接管表里"
        );
    }
}
