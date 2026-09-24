//! 扫一遍，把看到的和担心的分开说。
//!
//! 两个消费者共用这一份结果：
//!
//! | | 安全 | 清单 |
//! |---|---|---|
//! | 要的是 | 告警 | 此刻的真实状态 |
//! | 存哪 | 进 `data.db`，要有历史 | **只在内存** |
//!
//! 所以这里只负责「扫出来是什么」，**不写任何文件**，也不决定存不存。
//!
//! # 只报告，不自动删除
//!
//! 这一条写死。误报删掉用户的正常配置比漏报还糟 —— 它会摧毁
//! 信任，然后用户关掉整个功能，连真正有用的那些告警一起关掉。这个模块
//! 里没有任何一处会删东西，有测试盯着。

use std::path::PathBuf;

use tw_adopt::json::Val;
use tw_types::{Msg, msg};

use crate::sources::{self, Source};
use tw_guard::hidden;
use tw_guard::tools::rules::Rules;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// 高危：发系统通知，托盘挂角标
    High,
    Medium,
    Low,
}

impl Level {
    pub fn slug(&self) -> &'static str {
        match self {
            Level::High => "high",
            Level::Medium => "medium",
            Level::Low => "low",
        }
    }
}

/// 一条发现。
#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    /// 哪条规则命中的（`zero_width` / `curl-pipe-sh` / …）
    pub rule: String,
    pub kind: sources::Kind,
    pub client: String,
    pub path: PathBuf,
    /// 第几行，从 1 开始。**要能定位到行**
    pub line: usize,
    /// **带码。**这一屏要用界面自己的语言说出来；英文原句是给命令行
    /// 和不认识这个码的客户端的退路。参数里给的都是词表里的词
    /// （`kind` / `rule` / `what`），不是拼好的句子 —— 句子两边各写各的
    pub title: Msg,
    /// 为什么它值得看一眼
    pub detail: Msg,
    /// 命中的那一行。不可见字符已经换成可见记号
    pub excerpt: String,
}

/// 一个 MCP server 在某个客户端里的样子。
///
/// **有两种形态，而它们的风险不是一回事** —— 本机跑一个二进制，和把
/// 上下文发到别人的服务器上。本机这台机器上两种都有，是实扫出来的，
/// 不是设想的。
#[derive(Debug, Clone, PartialEq)]
pub struct McpServer {
    pub name: String,
    pub client: String,
    /// 要执行的程序。远端型的这里是空
    pub command: String,
    pub args: Vec<String>,
    /// 远端型：它的地址。**用到它的时候，相关上下文会发到这台服务器**
    pub url: Option<String>,
    /// 它会读哪些环境变量名。**只有名字，没有值** —— 值里常常就是密钥
    pub env_keys: Vec<String>,
    /// 配置里写着 `enabled = false`。
    ///
    /// 关掉的照样列出来（它还在配置里，一次编辑就能打开），但**告警要
    /// 降一级** —— 对一个当前跑不起来的东西喊高危，是狼来了。
    pub enabled: bool,
    pub source: PathBuf,
}

impl McpServer {
    /// 判断「同名不同配置」用的指纹（矩阵上要标记号）。
    pub fn shape(&self) -> String {
        match &self.url {
            Some(u) => format!("remote {u}"),
            None => format!("{} {}", self.command, self.args.join(" ")),
        }
    }
    /// 远端型，而且不在本机。
    pub fn is_third_party(&self) -> bool {
        self.url.as_deref().is_some_and(|u| {
            !u.contains("://localhost") && !u.contains("://127.0.0.1") && !u.contains("://[::1]")
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillEntry {
    pub name: String,
    pub client: String,
    pub path: PathBuf,
    /// frontmatter 里声明的工具权限
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HookEntry {
    pub client: String,
    /// `PostToolUse` 之类
    pub event: String,
    pub command: String,
    pub source: PathBuf,
}

#[derive(Debug, Default)]
pub struct Report {
    pub mcp: Vec<McpServer>,
    pub skills: Vec<SkillEntry>,
    pub hooks: Vec<HookEntry>,
    pub findings: Vec<Finding>,
    /// 读不动的文件。**说出来** —— 一个悄悄跳过了半数文件的扫描比不扫
    /// 更糟，因为它会给人一种「查过了」的错觉
    pub unreadable: Vec<String>,
}

fn read(p: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

/// 把 JSON / TOML 读成同一种值。两种格式各有各的解析器，但清单和扫描
/// 只关心结构。
fn parse_any(src: &Source, text: &str) -> Option<Val> {
    match src.path.extension().and_then(|e| e.to_str()) {
        Some("toml") => tw_adopt::toml::value(text).ok(),
        Some("json") => tw_adopt::json::value(text).ok(),
        _ => None,
    }
}

fn obj<'a>(v: &'a Val, key: &str) -> Option<&'a Vec<(String, Val)>> {
    let Val::Obj(ms) = v else { return None };
    match &ms.iter().find(|(k, _)| k == key)?.1 {
        Val::Obj(inner) => Some(inner),
        _ => None,
    }
}

fn s(v: &Val, key: &str) -> Option<String> {
    let Val::Obj(ms) = v else { return None };
    ms.iter()
        .find(|(k, _)| k == key)?
        .1
        .as_str()
        .map(|x| x.to_string())
}

fn strings(v: &Val, key: &str) -> Vec<String> {
    let Val::Obj(ms) = v else { return Vec::new() };
    match ms.iter().find(|(k, _)| k == key).map(|(_, v)| v) {
        Some(Val::Arr(es)) => es.iter().map(|e| e.to_line()).collect(),
        _ => Vec::new(),
    }
}

/// 各客户端把 MCP 段放在不同的键下面。**这是唯一的差别** —— 里面的
/// 形状（`command` / `args` / `env`）反而是一致的。
const MCP_KEYS: &[&str] = &["mcpServers", "mcp_servers", "mcp", "context_servers"];

fn mcp_from(src: &Source, v: &Val) -> Vec<McpServer> {
    let mut out = Vec::new();
    for key in MCP_KEYS {
        let Some(servers) = obj(v, key) else { continue };
        for (name, cfg) in servers {
            out.push(McpServer {
                name: name.clone(),
                client: src.client.to_string(),
                command: s(cfg, "command").unwrap_or_default(),
                args: strings(cfg, "args"),
                url: s(cfg, "url"),
                // **只取键名，不取值。**值里常常就是密钥本身
                env_keys: match obj(cfg, "env") {
                    Some(e) => e.iter().map(|(k, _)| k.clone()).collect(),
                    None => Vec::new(),
                },
                // 没写就是开着 —— 各家的默认都是这样
                enabled: !matches!(
                    cfg,
                    Val::Obj(ms) if ms.iter().any(|(k, v)| k == "enabled" && *v == Val::Bool(false))
                ),
                source: src.path.clone(),
            });
        }
    }
    out.sort_by(|a, b| (&a.name, &a.client).cmp(&(&b.name, &b.client)));
    out
}

/// hooks 段里所有的 `command`。
///
/// **递归着找，不照着某一版的嵌套形状写死。**上游改了层级我们顶多多报
/// 一条，而写死的话会一条都报不出来 —— 对一个安全功能来说，这两种失败
/// 的代价差得很远。
fn commands_under(v: &Val, out: &mut Vec<String>) {
    match v {
        Val::Obj(ms) => {
            for (k, x) in ms {
                if k == "command"
                    && let Some(c) = x.as_str()
                {
                    out.push(c.to_string());
                } else {
                    commands_under(x, out);
                }
            }
        }
        Val::Arr(es) => es.iter().for_each(|e| commands_under(e, out)),
        _ => {}
    }
}

fn hooks_from(src: &Source, v: &Val) -> Vec<HookEntry> {
    let Some(hooks) = obj(v, "hooks") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (event, body) in hooks {
        let mut cmds = Vec::new();
        commands_under(body, &mut cmds);
        for c in cmds {
            out.push(HookEntry {
                client: src.client.to_string(),
                event: event.clone(),
                command: c,
                source: src.path.clone(),
            });
        }
    }
    out
}

/// `---` 之间的 frontmatter。
fn frontmatter(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

fn allowed_tools(text: &str) -> Vec<String> {
    let Some(fm) = frontmatter(text) else {
        return Vec::new();
    };
    let Ok(v) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(fm) else {
        return Vec::new();
    };
    let get = |k: &str| v.get(k).cloned();
    let raw = get("allowed-tools")
        .or_else(|| get("allowed_tools"))
        .or_else(|| get("tools"));
    match raw {
        Some(serde_yaml_ng::Value::Sequence(xs)) => xs
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect(),
        Some(serde_yaml_ng::Value::String(s)) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn line_of(text: &str, needle: &str) -> (usize, String) {
    for (i, l) in text.lines().enumerate() {
        if l.contains(needle) {
            return (i + 1, l.trim().to_string());
        }
    }
    (0, needle.to_string())
}

/// 扫一批文件。**不写任何东西。**
pub fn scan(sources: &[Source], rules: &Rules) -> Report {
    let mut r = Report::default();

    for src in sources {
        let Some(text) = read(&src.path) else {
            // 悄悄跳过比不扫更糟：它会给人一种「查过了」的错觉
            r.unreadable.push(src.path.display().to_string());
            continue;
        };

        // 一、藏起来的东西。**每一份都扫**，配置文件也不例外
        for h in hidden::scan(&text) {
            // 标签字符和双向控制符在任何文本里都没有正当用途
            let level = if h.kind.smuggles() {
                Level::High
            } else {
                Level::Medium
            };
            r.findings.push(Finding {
                level,
                rule: h.kind.slug().to_string(),
                kind: src.kind,
                client: src.client.to_string(),
                path: src.path.clone(),
                line: h.line,
                title: msg!(
                    "scan.hidden",
                    kind = src.kind.slug(),
                    what = h.kind.slug()
                    => "{} contains {}",
                    src.kind.label(),
                    h.kind
                        .why()
                        .split(':')
                        .next()
                        .unwrap_or("hidden characters")
                ),
                // 两句之间要有一个空格 —— 中文句号自己带停顿，英文句点不带，
                // 直接接上会读成「…by the model.The content of…」
                detail: msg!(
                    "scan.hidden.detail",
                    kind = src.kind.slug(),
                    what = h.kind.slug()
                    => "{} {}",
                    h.kind.why(),
                    src.kind.why()
                ),
                excerpt: h.line_text,
            });
        }

        // 二、结构化的那几类：MCP、hooks、skill
        let parsed = parse_any(src, &text);
        if let Some(v) = &parsed {
            if src.kind == sources::Kind::Mcp {
                for m in mcp_from(src, v) {
                    // **远端型不是「运行一个二进制」，而是「把上下文发
                    // 出去」。**级别定成 low：它多半是用户自己有意加的，
                    // 喊高危就是狼来了；但他有权知道有这么一条出境路径。
                    if m.is_third_party() && m.enabled {
                        let (line, excerpt) = line_of(&text, m.url.as_deref().unwrap_or(""));
                        r.findings.push(Finding {
                            level: Level::Low,
                            rule: "remote-mcp".into(),
                            kind: src.kind,
                            client: src.client.to_string(),
                            path: src.path.clone(),
                            line,
                            title: msg!("scan.mcp.remote", name = m.name.clone() => "MCP server `{name}` is remote"),
                            detail: msg!(
                                "scan.mcp.remote.detail",
                                url = m.url.clone().unwrap_or_default()
                                => "That server is at {url}, and using it sends the surrounding context there."
                            ),
                            excerpt,
                        });
                    }
                    r.mcp.push(m);
                }
                r.mcp
                    .sort_by(|a, b| (&a.name, &a.client).cmp(&(&b.name, &b.client)));
            }
            if src.kind == sources::Kind::Hooks {
                r.hooks.extend(hooks_from(src, v));
            }
        }
        if src.kind == sources::Kind::Skill {
            let name = src
                .path
                .parent()
                .and_then(|p| p.file_name())
                .map(|x| x.to_string_lossy().to_string())
                .unwrap_or_default();
            let tools = allowed_tools(&text);
            // **`*` 意味着这个 skill 能用任何工具。**它可能完全正当，
            // 但用户有权知道自己装了这么一个东西
            if tools.iter().any(|t| t == "*") {
                let (line, excerpt) = line_of(&text, "*");
                r.findings.push(Finding {
                    level: Level::Medium,
                    rule: "over-broad-tools".into(),
                    kind: src.kind,
                    client: src.client.to_string(),
                    path: src.path.clone(),
                    line,
                    title: msg!("scan.skill.all_tools", name = name.clone() => "skill `{name}` declares allowed-tools: [\"*\"]"),
                    detail: msg!("scan.skill.all_tools.detail" => "That skill may use any tool. It may well need to; it is worth confirming that it does."),
                    excerpt,
                });
            }
            r.skills.push(SkillEntry {
                name,
                client: src.client.to_string(),
                path: src.path.clone(),
                allowed_tools: tools,
            });
        }

        // 三、规则集。**指令类文件扫全文**，配置类只扫命令字段 ——
        // 拿注入规则去扫一份 JSON 会把里面正常的英文说明全报一遍
        let targets: Vec<(String, bool)> = match src.kind {
            sources::Kind::Skill
            | sources::Kind::Command
            | sources::Kind::Agent
            | sources::Kind::Instructions => vec![(text.clone(), false)],
            sources::Kind::Hooks => hooks_from(src, parsed.as_ref().unwrap_or(&Val::Null))
                .into_iter()
                .map(|h| (h.command, true))
                .collect(),
            sources::Kind::Mcp => mcp_from(src, parsed.as_ref().unwrap_or(&Val::Null))
                .into_iter()
                // 关掉的那些跑不起来，规则不扫它们；它们仍然在清单里
                .filter(|m| m.enabled)
                .map(|m| (format!("{} {}", m.command, m.args.join(" ")), true))
                .collect(),
        };
        for (hay, executes) in targets {
            for rule in &rules.rules {
                let Some(m) = rule.re.find(&hay) else {
                    continue;
                };
                let (line, excerpt) = line_of(&text, m.as_str());
                // `msg!` 会把 `rule` 遮住，所以句子要用到的几段先取出来
                let why = rule.why.clone();
                let name = rule.name.clone();
                let kind_why = src.kind.why();
                r.findings.push(Finding {
                    // **hook 和 MCP 里的危险命令是最高级**：它们不需要
                    // 模型参与就会被执行
                    level: if executes && rule.group == "dangerous" {
                        Level::High
                    } else {
                        Level::Medium
                    },
                    rule: rule.id.clone(),
                    kind: src.kind,
                    client: src.client.to_string(),
                    path: src.path.clone(),
                    line,
                    title: msg!(
                        "scan.rule",
                        kind = src.kind.slug(),
                        rule = rule.id.clone()
                        => "{} matched rule “{}”",
                        src.kind.label(),
                        name
                    ),
                    detail: msg!(
                        "scan.rule.detail",
                        kind = src.kind.slug(),
                        rule = rule.id.clone()
                        => "{}. {}",
                        why,
                        kind_why
                    ),
                    excerpt,
                });
            }
        }
    }

    // 高的排前面，同级按危险度排。**界面直接按这个顺序画**
    r.findings.sort_by_key(|a| (a.level, a.kind));
    r
}

/// 同名但配置不同的 MCP server（矩阵上要标记号）。
pub fn conflicting(mcp: &[McpServer]) -> Vec<String> {
    let mut by_name: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    for m in mcp {
        by_name.entry(&m.name).or_default().push(m.shape());
    }
    by_name
        .into_iter()
        .filter(|(_, shapes)| {
            let first = &shapes[0];
            shapes.iter().any(|s| s != first)
        })
        .map(|(n, _)| n.to_string())
        .collect()
}
