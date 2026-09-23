//! 本机上有哪些 AI 客户端。
//!
//! **两张表的成员不一样。**「能接管 API 端点」和「有 MCP 要管」是两件
//! 事：Claude Code 两张表都在，Claude Desktop 只在第二张（它是订阅制，
//! 接管不了，但它的 MCP 配置是危险度第二高的攻击面）。初稿把两张表混成
//! 一张，就漏掉了后者 —— 而漏掉它等于扫描留了个洞。
//!
//! 这个文件只管第一张表。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tw_types::Msg;

use crate::json::Val;

/// 配置文件是什么格式。**决定了怎么做字段级合并，以及哨兵往哪儿放。**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    /// 严格 JSON，**装不下注释** —— 哨兵退化成同目录的旁文件
    Json,
    Toml,
    Yaml,
}

/// 配置改完什么时候生效。
///
/// **这个差别真的会让用户困惑**，而它直接决定观察窗口的行为：
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
    /// 控制面发给界面的值。接管完成那一屏由界面按它说明什么时候生效 ——
    /// **在那一刻说，不是等五分钟后再说。**
    pub fn slug(&self) -> &'static str {
        match self {
            TakesEffect::Immediately => "immediately",
            TakesEffect::OnRestart => "on_restart",
        }
    }
    /// 接管提示和诊断结论里的那一句。
    pub fn note(&self) -> &'static str {
        match self {
            TakesEffect::Immediately => "The next request uses the new configuration.",
            TakesEffect::OnRestart => {
                "It takes effect once the terminal is reopened; until then the gateway sees nothing from this client."
            }
        }
    }
    /// 该不该设「还没收到请求」的超时提示。
    pub fn warns_when_silent(&self) -> bool {
        matches!(self, TakesEffect::Immediately)
    }
}

/// 我们对这一条了解到什么程度。**要显示在界面上。**
///
/// 表格自己就标了「前五个在这台机器上实测存在，后四个
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
    /// 控制面发给界面的值。
    pub fn slug(&self) -> &'static str {
        match self {
            Verified::Measured => "measured",
            Verified::FieldsOnly => "fields_only",
        }
    }
    /// 这一条我们了解到什么程度，说成一句话。
    ///
    /// **一整句，不是半截。**它会被接到另一句前面（接管说明里那条
    /// 「收到第一个请求之前别当成已生效」），半截话接上去读出来是
    /// 「…on this machine Do not take it…」—— 中间没有停顿，后半句还
    /// 小写开头。[`TakesEffect::note`] 早就是整句的，这里跟上。
    pub fn note(&self) -> &'static str {
        match self {
            Verified::Measured => "It was checked by actually running it on this machine.",
            Verified::FieldsOnly => {
                "The field names are verified; it has not been run on this machine."
            }
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
    /// 优先级比主配置更高、会盖住我们的那些文件（诊断链）。
    ///
    /// **检测阶段就要扫**：cc-switch 的 #6828 就是栽在
    /// `settings.local.json` 上 —— 我们写了 `settings.json`，而那边的
    /// 残留把它遮住了，用户看到的是「接管了但没生效」。
    pub shadowed_by: &'static [&'static str],
    /// 接管的代价。**接管确认对话框要把它们列出来，不能等用户自己发现**
    /// —— 这些不是我们的 bug，但用户会算到我们头上。
    ///
    /// 每条是「码，英文原句」。码给桌面版查中文，英文原句给命令行和
    /// 不认识这个码的客户端。
    pub costs: &'static [(&'static str, &'static str)],
    pub verified: Verified,
    /// 判断「这台机器上装了它吗」的痕迹，相对 `$HOME`。
    ///
    /// 不能只看配置文件在不在：`.aider.conf.yml` 这种，没接管过的用户
    /// 本来就没有；而 `~/.claude/` 这种，装了就一定有。
    pub marker: &'static [&'static str],
    /// 进程名里认得出它的片段。诊断「客户端没重启」要用。
    pub process: &'static [&'static str],
    /// 它会读的环境变量。扫 shell 配置时找这些名字。
    pub env_vars: &'static [&'static str],
    /// 密钥不在配置文件里、要在客户端自己的界面里填时，手动配置多出来的那一步
    /// （「码，英文原句」）。只有 Zed 是这样：它的密钥走自己的凭据存储。
    pub key_elsewhere: Option<(&'static str, &'static str)>,
    /// 它的配置文件优先级**高于**真实 shell 环境变量。
    ///
    /// Claude Code 是这样（`env` 块会盖住 shell 里的 export），所以对它
    /// 来说 `.zshrc` 里的残留不是问题；对 Codex 这类读环境变量的客户端
    /// 就是问题。**同一条发现，对不同客户端的结论相反** —— 不区分的话
    /// 就会给出一条错误的诊断。
    pub config_beats_env: bool,
}

/// 网关这一侧的地址和钥匙。
#[derive(Debug, Clone)]
pub struct Gateway {
    /// 形如 `http://127.0.0.1:8080`，**不带尾斜杠、不带 `/v1`**
    pub base: String,
    /// 给这个客户端的专属密钥。`None` = 网关不要求鉴权。
    ///
    /// 专属密钥的意义在于**客户端识别**，这样规则里才能写
    /// `when: { client: claude-code }`。
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
/// 还在增长的集合，所以 147 个 commit 之后整个撤回了。
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
/// Codex 的 `openai` / `ollama` / `lmstudio` 是保留 id，不能撞。
pub const PROVIDER_ID: &str = "thinkwatch";

/// 表一：能接管 API 端点的。
///
/// 字段名都对应上游当前文档，不是猜的。
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
                (
                    "adopt.cost.claude_code.remote_control",
                    "Remote Control and voice input do not work when the endpoint is not an official domain.",
                ),
                (
                    "adopt.cost.claude_code.mcp_tool_search",
                    "MCP tool search is off by default.",
                ),
                (
                    "adopt.cost.claude_code.welcome_screen",
                    "Claude Code may show its welcome screen once; closing it is enough.",
                ),
            ],
            verified: Verified::FieldsOnly,
            marker: &[".claude"],
            process: &["claude"],
            env_vars: &[
                "ANTHROPIC_BASE_URL",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_MODEL",
            ],
            // `env` 块会盖住 shell 里的 export
            key_elsewhere: None,
            config_beats_env: true,
        },
        // **不叫「Codex CLI」。**`~/.codex/config.toml` 是一份配置、两个
        // 前端：命令行的 codex，和 ChatGPT 桌面版内置的那一个
        // （`ChatGPT.app/Contents/Resources/codex`，同一个二进制）。桌面版
        // 启动 app-server 时不带任何 provider 覆盖，`model_provider` 完全
        // 由这个文件决定 —— 只装了桌面版的用户看到「Codex CLI」，会以为
        // 在说一个他没装的东西。
        Client {
            id: "codex",
            name: "Codex",
            config: ".codex/config.toml",
            format: Format::Toml,
            // **读环境变量的，必须关掉终端重开**
            takes_effect: TakesEffect::OnRestart,
            // 项目级的 .codex/config.toml 会忽略 model_provider，
            // 所以它不构成遮蔽 —— 但它确实存在，值得在诊断里提一句
            shadowed_by: &[],
            costs: &[
                // 接管这个文件顺带接管了桌面版，**这件事要在确认之前说**
                (
                    "adopt.cost.codex.chatgpt_desktop",
                    "The ChatGPT desktop app reads the same configuration file, so its local Codex sessions go through the gateway as well; the app has to be restarted for that to take effect.",
                ),
                (
                    "adopt.cost.codex.reopen_terminal",
                    "The terminal has to be reopened afterwards.",
                ),
            ],
            verified: Verified::Measured,
            marker: &[".codex"],
            process: &["codex"],
            env_vars: &["OPENAI_API_KEY", "OPENAI_BASE_URL", "CODEX_HOME"],
            key_elsewhere: None,
            config_beats_env: false,
        },
        Client {
            id: "opencode",
            name: "opencode",
            config: ".config/opencode/opencode.json",
            format: Format::Json,
            takes_effect: TakesEffect::OnRestart,
            shadowed_by: &[],
            costs: &[(
                "adopt.cost.opencode.restart",
                "opencode has to be restarted afterwards.",
            )],
            verified: Verified::FieldsOnly,
            marker: &[".config/opencode", ".local/share/opencode"],
            process: &["opencode"],
            env_vars: &["OPENAI_API_KEY", "OPENAI_BASE_URL"],
            key_elsewhere: None,
            config_beats_env: false,
        },
        Client {
            id: "zed",
            name: "Zed",
            config: crate::paths::zed_settings(),
            format: Format::Json,
            takes_effect: TakesEffect::Immediately,
            shadowed_by: &[],
            // **只算部分接管**：Zed 的密钥走它自己的凭据存储，不在
            // settings.json 里，我们写不进去。
            costs: &[(
                "adopt.cost.zed.key_store",
                "Zed keeps its key outside the configuration file, so it has to be filled in once in Zed's settings.",
            )],
            verified: Verified::FieldsOnly,
            marker: &[crate::paths::ZED_DIR],
            process: &["Zed"],
            env_vars: &[],
            key_elsewhere: Some((
                "adopt.manual.zed.key",
                "Then enter the key in Zed's settings, under the ThinkWatch provider.",
            )),
            config_beats_env: false,
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
                (
                    "adopt.cost.aider.lookup_order",
                    "Aider reads the home directory, then the Git project root, then the current directory, and each one overrides the last; only the home directory is changed here.",
                ),
                (
                    "adopt.cost.aider.restart",
                    "Aider has to be restarted afterwards.",
                ),
            ],
            verified: Verified::FieldsOnly,
            // 没接管过的用户本来就没有这个文件，所以它自己就是那个痕迹
            marker: &[".aider.conf.yml", ".aider.model.settings.yml"],
            process: &["aider"],
            env_vars: &["OPENAI_API_BASE", "OPENAI_API_KEY"],
            key_elsewhere: None,
            config_beats_env: false,
        },
    ]
}

/// 接管不了、只能给指引的。
///
/// **不假装能接管。**Cursor 没有可写的配置文件，而且即使手动改了，
/// Tab 补全和 inline edit 仍然走它自己的后端 —— 显示成「已接管」会让
/// 用户以为所有流量都在我们这儿。
pub struct ManualOnly {
    /// 为它生成专用密钥时用的标识：`cursor` / `continue` / `gemini-cli`
    pub id: &'static str,
    pub name: &'static str,
    /// 这个客户端的码前缀：每一步是 `<prefix>.<后缀>`，提醒也是 `<prefix>.<后缀>`（见 `caveat`）
    code: &'static str,
    /// 手动配置的几步，每步「码的后缀，英文原句」。
    ///
    /// **地址和密钥不写进句子。**以前是一整句带着地址的话，用户得从句子里
    /// 抠出一段 URL；现在两样东西各在界面上有自己的复制按钮，句子只说
    /// 「填到哪儿」。
    steps: &'static [(&'static str, &'static str)],
    /// 填带 `/v1` 的地址还是不带的
    v1: bool,
    /// 提醒，「码的后缀，英文原句」。**后缀也可以按平台分** —— 两个平台
    /// 说的不是一句话时，用各自的码，界面才能各翻各的
    caveat: (&'static str, &'static str),
}

impl ManualOnly {
    /// 手动配置的几步
    pub fn steps(&self) -> Vec<Msg> {
        self.steps
            .iter()
            .map(|(suffix, text)| Msg {
                code: format!("{}.{suffix}", self.code),
                args: BTreeMap::new(),
                text: text.to_string(),
            })
            .collect()
    }

    /// 要填的网关地址，这个客户端要的写法
    pub fn endpoint(&self, gw: &Gateway) -> String {
        if self.v1 {
            gw.v1()
        } else {
            gw.base.trim_end_matches('/').to_string()
        }
    }

    /// 接管不了的那一句提醒。**必须和步骤一起给** —— 只说怎么配、不说
    /// 配完还漏什么，等于说了假话
    pub fn caveat(&self) -> Msg {
        let (suffix, text) = self.caveat;
        Msg {
            code: format!("{}.{suffix}", self.code),
            args: BTreeMap::new(),
            text: text.to_string(),
        }
    }
}

pub fn manual_only() -> Vec<ManualOnly> {
    vec![
        ManualOnly {
            id: "cursor",
            name: "Cursor",
            code: "adopt.manual.cursor",
            steps: &[
                ("open", "In Cursor, open Settings → Models."),
                (
                    "base",
                    "Turn on Override OpenAI Base URL and enter the gateway address.",
                ),
                (
                    "key",
                    "Enter the key as the OpenAI API Key, then click Verify.",
                ),
            ],
            v1: true,
            caveat: (
                "caveat",
                "Tab completion and inline edit still go to Cursor's own service rather than the gateway, so only part of Cursor is covered.",
            ),
        },
        ManualOnly {
            id: "continue",
            name: "Continue",
            code: "adopt.manual.continue",
            steps: &[
                #[cfg(not(windows))]
                ("open", "Open ~/.continue/config.yaml."),
                #[cfg(windows)]
                ("open_windows", r"Open %USERPROFILE%\.continue\config.yaml."),
                (
                    "entry",
                    "Add an entry to the models list with provider set to openai, apiBase set to the gateway address and apiKey set to the key.",
                ),
            ],
            v1: true,
            // **接管它要往一个 YAML 列表里插一个新条目**，那是结构性
            // 改写，不是替换一个标量。我们的 YAML 补丁只做后者
            // （见 crate::yaml 开头那段）。宁可少接管一个客户端，也不
            // 要写一段我们自己没把握的结构。
            caveat: (
                "caveat",
                "This needs a new entry in the models list, which is not written automatically; follow the steps above.",
            ),
        },
        ManualOnly {
            id: "gemini-cli",
            name: "Gemini CLI",
            code: "adopt.manual.gemini_cli",
            // **Windows 上没有 shell 配置文件可 export**：用户级环境变量用 setx
            // 写，写完只对之后打开的终端生效。两个平台各用各的码
            #[cfg(not(windows))]
            steps: &[
                (
                    "export",
                    "In the shell configuration, export GOOGLE_GEMINI_BASE_URL set to the gateway address and GEMINI_API_KEY set to the key.",
                ),
                ("reopen", "Then reopen the terminal."),
            ],
            #[cfg(windows)]
            steps: &[
                (
                    "setx",
                    "In a terminal, run setx GOOGLE_GEMINI_BASE_URL followed by the gateway address, and setx GEMINI_API_KEY followed by the key.",
                ),
                ("reopen", "Then reopen the terminal."),
            ],
            v1: false,
            // 它只认环境变量，没有可写的配置字段。改 .zshrc 超出了
            // 「只改 endpoint 和 key 字段」的边界 ——
            // **报告是我们的职责，修改是他的权利。**
            #[cfg(not(windows))]
            caveat: (
                "caveat",
                "Gemini CLI reads the endpoint only from the environment. ThinkWatch does not edit shell configuration files, so add it by hand.",
            ),
            #[cfg(windows)]
            caveat: (
                "caveat_windows",
                "Gemini CLI reads the endpoint only from the environment. ThinkWatch does not change environment variables, so add them by hand.",
            ),
        },
    ]
}

/// 接管这个客户端要写哪些字段。
pub fn edits(client: &Client, gw: &Gateway) -> Vec<Edit> {
    match client.id {
        // 统一写 `ANTHROPIC_AUTH_TOKEN` 而不是 `ANTHROPIC_API_KEY`：
        // 后者在交互模式下要用户去 /config 点一次确认，**被拒绝是静默
        // 忽略的** —— 接管会看起来「没生效」却查不出原因。
        "claude-code" => {
            let mut v = vec![
                e(&["env", "ANTHROPIC_BASE_URL"], Val::s(&gw.base)),
                // 不设它，Claude Code 根本不会来问我们的 /v1/models
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
        //
        // **这里曾经有一条「Codex 不从网关取模型列表」的接管代价，删了。**
        // 0.139.0 确实一个 GET 都没有，0.153.4 开始会先来一个
        // `GET /v1/models?client_version=…` —— 但它要的是 Codex 自己的目录
        // 格式（顶层 `models`），网关答的是 OpenAI 的 `data` 形状，它解码
        // 失败，只记一行日志。**而这不影响任何人用**：codex 的
        // `models-manager/models.json` 是 `include_str!` 编进二进制的，远端
        // 目录只是补充；模型名在内置表里就没有任何提示，不在就按兜底的
        // 272k 上下文跑。openai/codex 今天的 main 上还加了闸 —— 设了自定义
        // base_url 又没设 `model_catalog_url` 的 provider 直接跳过这次请求。
        // 一条不拦人、还在自己消失的事，不值得占接管对话框的一行。
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
    /// 手动配置的几步：打开哪个文件、写下面那几项（就是接管时写的那几项，
    /// 见 [`edits`]），密钥要另外填的再加一步。
    ///
    /// **没检测到它的时候也要给。**配置文件不在默认位置、或者装在别的
    /// 用户目录下时，检测不到不等于用不了 —— 照着做一样能接上。
    pub fn manual_steps(&self) -> Vec<Msg> {
        let file = crate::paths::shown(self.config);
        let mut out = vec![Msg {
            code: "adopt.manual.file".into(),
            args: BTreeMap::from([("file".to_string(), file.clone())]),
            text: format!("Open {file} and set the fields below."),
        }];
        if let Some((code, text)) = self.key_elsewhere {
            out.push(Msg {
                code: code.into(),
                args: BTreeMap::new(),
                text: text.into(),
            });
        }
        out
    }

    /// 要填的网关地址：接管时写进它配置的那一个（有的带 `/v1`，有的不带）
    pub fn endpoint(&self, gw: &Gateway) -> String {
        edits(self, gw)
            .into_iter()
            .find_map(|e| match &e.value {
                Val::Str(s) if !e.secret && (*s == gw.base || *s == gw.v1()) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| gw.base.clone())
    }

    pub fn config_path(&self, home: &std::path::Path) -> PathBuf {
        crate::paths::under(home, self.config)
    }
    pub fn shadow_paths(&self, home: &std::path::Path) -> Vec<PathBuf> {
        self.shadowed_by
            .iter()
            .map(|p| crate::paths::under(home, p))
            .collect()
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
        // 被误报几次之后真正该看的那次也不会看了。
        for c in adoptable() {
            if c.takes_effect == TakesEffect::OnRestart {
                assert!(
                    !c.takes_effect.warns_when_silent(),
                    "{} 需要重开却设了超时提示",
                    c.id
                );
                assert!(
                    c.takes_effect.note().contains("terminal is reopened"),
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
        // 这些不是我们的 bug，但用户会算到我们头上。
        let cc = adoptable()
            .into_iter()
            .find(|c| c.id == "claude-code")
            .unwrap();
        assert!(!cc.costs.is_empty());
        assert!(
            cc.costs.iter().any(|(_, t)| t.contains("Remote Control")),
            "{:?}",
            cc.costs
        );
    }

    #[test]
    fn json_clients_have_no_comment_prefix_so_they_use_the_sidecar() {
        // 严格 JSON 装不下注释 —— 哨兵退化成同目录的旁文件。
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
        // 我们这儿，而 Tab 补全根本不经过。
        let m = manual_only();
        let cursor = m.iter().find(|c| c.name == "Cursor").unwrap();
        assert!(
            cursor.caveat.1.contains("Tab completion"),
            "{}",
            cursor.caveat.1
        );
        // 地址是真实的地址，而不是一个让用户自己去找的说法；**不在句子里**，
        // 界面单独给它一个复制按钮
        let gw = Gateway {
            base: "http://127.0.0.1:8788".into(),
            key: None,
        };
        assert_eq!(cursor.endpoint(&gw), "http://127.0.0.1:8788/v1");
        let gemini = m.iter().find(|c| c.id == "gemini-cli").unwrap();
        assert_eq!(
            gemini.endpoint(&gw),
            "http://127.0.0.1:8788",
            "Gemini CLI 不要 /v1"
        );
        for c in &m {
            for step in c.steps() {
                assert!(!step.text.contains("127.0.0.1"), "{}：{}", c.name, step);
                assert!(!step.text.contains('{'), "{}：{}", c.name, step);
            }
        }
        assert!(
            !adoptable().iter().any(|c| c.name == "Cursor"),
            "Cursor 不该在接管表里"
        );
    }

    /// **每一句给人看的话都要带码。**
    ///
    /// 漏一个不会报错、不会崩，只会让中文界面上那一行悄悄变成英文 ——
    /// 而这正是 v0.10.0 干过的事：错误全上了码，接管的代价和提醒没有，
    /// 于是客户端页整页是英文。
    #[test]
    fn every_sentence_here_carries_a_code() {
        for c in adoptable() {
            for (code, text) in c.costs {
                assert!(!code.is_empty(), "{}：「{text}」没有码", c.id);
                assert!(!text.is_empty(), "{}：{code} 没有英文原句", c.id);
            }
        }
        let gw = Gateway {
            base: "http://127.0.0.1:8787".into(),
            key: None,
        };
        let _ = &gw;
        for m in manual_only() {
            assert!(!m.steps().is_empty(), "{}：没有步骤", m.name);
            for step in m.steps() {
                assert!(!step.code.is_empty(), "{}：步骤没有码", m.name);
            }
            assert!(!m.caveat().code.is_empty(), "{}：提醒没有码", m.name);
        }
        for c in adoptable() {
            for step in c.manual_steps() {
                assert!(!step.code.is_empty(), "{}：手动配置的步骤没有码", c.id);
            }
        }
    }

    /// 没检测到的客户端也要能照着手动配上：打开哪个文件、写哪几项、填哪个地址。
    #[test]
    fn every_adoptable_client_says_how_to_do_it_by_hand() {
        let gw = Gateway {
            base: "http://127.0.0.1:18790".into(),
            key: Some("tw-k".into()),
        };
        let by = |id: &str| adoptable().into_iter().find(|c| c.id == id).unwrap();
        // 各要各的写法：Claude Code 不带 /v1，其余带
        assert_eq!(by("claude-code").endpoint(&gw), "http://127.0.0.1:18790");
        for id in ["codex", "opencode", "zed", "aider"] {
            assert_eq!(by(id).endpoint(&gw), "http://127.0.0.1:18790/v1", "{id}");
        }
        for c in adoptable() {
            let steps = c.manual_steps();
            assert_eq!(steps[0].arg("file"), format!("~/{}", c.config), "{}", c.id);
            assert!(!edits(&c, &gw).is_empty(), "{}：没有要写的字段", c.id);
        }
        // Zed 的密钥不在配置文件里，多一步在它自己的设置里填
        assert_eq!(by("zed").manual_steps().len(), 2);
        assert_eq!(by("claude-code").manual_steps().len(), 1);
    }

    /// **`note()` 给的是整句，不是半截。**这两个 `note()` 都会被接到
    /// 别的句子前后去，半截话接上去就是一句读不通的话 —— 而它不会报错，
    /// 只会让用户读到 `…on this machine Do not take it…`。
    #[test]
    fn a_note_is_a_whole_sentence() {
        let notes = [
            Verified::Measured.note(),
            Verified::FieldsOnly.note(),
            TakesEffect::Immediately.note(),
            TakesEffect::OnRestart.note(),
        ];
        for n in notes {
            let first = n.chars().next().expect("note 不为空");
            assert!(
                first.is_uppercase(),
                "「{n}」小写开头，接在别的句子后面读不通"
            );
            assert!(n.ends_with('.'), "「{n}」没有句号，后面再接一句就连成一片");
        }
    }

    /// 码重了等于两句不同的话共用一条译文 —— 改其中一句，另一句会跟着
    /// 变，而没有任何东西会说出来。
    #[test]
    fn no_two_sentences_share_a_code() {
        let mut seen = std::collections::BTreeMap::new();
        for c in adoptable() {
            for (code, text) in c.costs {
                if let Some(other) = seen.insert(*code, *text) {
                    assert_eq!(other, *text, "{code} 被两句话共用了");
                }
            }
        }
    }
}
