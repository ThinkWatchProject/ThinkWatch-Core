//! 静态扫描的控制面（DESIGN.md §5.3、§7.12）。
//!
//! **每次请求现扫一遍，什么都不存。**§7.12 说得很明确：没有「同步状态」
//! 这个概念，也就没有「同步失效了」「主清单过期了」这类问题 —— 你看到的
//! 永远是磁盘上此刻的真实情况。
//!
//! 代价是每次打开页面要读几十个文件。那是几毫秒，换掉一整类状态一致性
//! 问题。

use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;

use crate::ControlState;

#[derive(Debug, Deserialize)]
pub struct Params {
    /// 额外扫哪些项目目录。**我们不去找项目，只看用户指的**（§5.3 的
    /// 范围约定）
    #[serde(default)]
    pub project: Vec<String>,
}

pub async fn scan(
    State(s): State<ControlState>,
    Query(p): Query<Params>,
) -> Json<tw_api::ScanResponse> {
    let dir = s
        .config_path()
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .to_path_buf();
    let (rules, warning) = tw_scan::rules::load(&dir);

    let mut sources = tw_scan::sources::user_level(&s.home);
    for proj in &p.project {
        sources.extend(tw_scan::sources::in_project(std::path::Path::new(proj)));
    }
    let scanned = sources.len();
    let r = tw_scan::report::scan(&sources, &rules);

    Json(tw_api::ScanResponse {
        conflicting: tw_scan::report::conflicting(&r.mcp),
        findings: r
            .findings
            .iter()
            .map(|f| tw_api::ScanFinding {
                level: f.level.slug().to_string(),
                rule: f.rule.clone(),
                kind: f.kind.slug().to_string(),
                kind_label: f.kind.label().to_string(),
                client: f.client.clone(),
                path: f.path.display().to_string(),
                line: f.line,
                title: f.title.clone(),
                detail: f.detail.clone(),
                excerpt: f.excerpt.clone(),
            })
            .collect(),
        mcp: r
            .mcp
            .iter()
            .map(|m| tw_api::McpView {
                third_party: m.is_third_party(),
                name: m.name.clone(),
                client: m.client.clone(),
                command: m.command.clone(),
                args: m.args.clone(),
                url: m.url.clone(),
                env_keys: m.env_keys.clone(),
                enabled: m.enabled,
                source: m.source.display().to_string(),
            })
            .collect(),
        skills: r
            .skills
            .iter()
            .map(|k| tw_api::SkillView {
                name: k.name.clone(),
                client: k.client.clone(),
                path: k.path.display().to_string(),
                allowed_tools: k.allowed_tools.clone(),
            })
            .collect(),
        hooks: r
            .hooks
            .iter()
            .map(|h| tw_api::HookView {
                client: h.client.clone(),
                event: h.event.clone(),
                command: h.command.clone(),
                source: h.source.display().to_string(),
            })
            .collect(),
        unreadable: r.unreadable,
        scanned,
        rules_origin: rules.origin,
        rules_warning: warning,
        projects: p.project,
    })
}
