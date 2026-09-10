//! 静态扫描的控制面（DESIGN.md §5.3、§7.12）。
//!
//! **每次请求现扫一遍，什么都不存。**§7.12 说得很明确：没有「同步状态」
//! 这个概念，也就没有「同步失效了」「主清单过期了」这类问题 —— 你看到的
//! 永远是磁盘上此刻的真实情况。
//!
//! 代价是每次打开页面要读几十个文件。那是几毫秒，换掉一整类状态一致性
//! 问题。

use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;

use crate::ControlState;

/// 把一条发现变成给界面看的样子。
pub fn finding_view(f: &tw_scan::report::Finding) -> tw_api::ScanFinding {
    tw_api::ScanFinding {
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
    }
}

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
    // 规则住在 config.yaml 里（§3.1），所以「现在生效的是哪一套」和
    // 「现在生效的是哪一份配置」永远是同一个答案
    let cfg = s.config();
    let rules = match tw_scan::rules::build(&cfg.security.scan_rules) {
        Ok(r) => r,
        Err(e) => {
            return Json(tw_api::ScanResponse {
                findings: Vec::new(),
                mcp: Vec::new(),
                skills: Vec::new(),
                hooks: Vec::new(),
                conflicting: Vec::new(),
                unreadable: Vec::new(),
                scanned: 0,
                rules_origin: e.to_string(),
                rules_warning: Some(e.to_string()),
                projects: p.project,
            });
        }
    };

    let mut sources = tw_scan::sources::user_level(&s.home);
    for proj in &p.project {
        sources.extend(tw_scan::sources::in_project(std::path::Path::new(proj)));
    }
    let scanned = sources.len();
    let r = tw_scan::report::scan(&sources, &rules);

    Json(tw_api::ScanResponse {
        conflicting: tw_scan::report::conflicting(&r.mcp),
        findings: r.findings.iter().map(finding_view).collect(),
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
        rules_origin: rules.summary(),
        // **写坏的那几条要说出来** —— 一条静默失效的安全规则，比没有那条
        // 规则更糟，因为用户以为它在
        rules_warning: (!rules.warnings.is_empty()).then(|| rules.warnings.join("；")),
        projects: p.project,
    })
}

/// 盯着配置面，**只在有新东西出现时**发事件（§5.3）。
///
/// 三条纪律都在这个函数里：
///
/// 1. **首次扫描不算「新出现」**（[`tw_scan::watch::Seen`] 负责）——
///    否则用户第一次打开就会被一屏告警砸中，而那些东西可能放了半年。
/// 2. **范围就是 [`tw_scan::sources`] 划定的那一批目录**，不递归、不全盘。
/// 3. **只报告。**这条路径上没有任何一处会改用户的文件。
pub fn spawn_watcher(
    state: ControlState,
) -> Result<Arc<tw_scan::watch::Watch>, tw_scan::watch::WatchError> {
    let home = state.home.clone();
    let cfg = state.config();
    let dirs = tw_scan::watch::dirs_for(&tw_scan::sources::user_level(&home));
    tracing::debug!(dirs = dirs.len(), "开始盯客户端配置面");
    let (w, mut rx) = tw_scan::watch::watch(&dirs)?;

    let bus = state.bus().clone();
    tokio::spawn(async move {
        let mut seen = tw_scan::watch::Seen::default();
        // 先垫一次底：把此刻已经存在的那些记下来，它们不算「新出现」
        let scan_now = |home: &std::path::Path| {
            let rules = tw_scan::rules::build(&cfg.security.scan_rules).unwrap_or_else(|_| {
                tw_scan::rules::build(&tw_config::ScanRules::default())
                    .expect("内置规则必须能编译 —— 有测试盯着")
            });
            tw_scan::report::scan(&tw_scan::sources::user_level(home), &rules)
        };
        seen.diff(&scan_now(&home).findings);

        while rx.recv().await.is_some() {
            // **每次重新枚举来源**：用户可能刚加了一个 skill，
            // 而那个文件在启动时还不存在
            let fresh = seen.diff(&scan_now(&home).findings);
            if fresh.is_empty() {
                continue;
            }
            tracing::info!(count = fresh.len(), "配置面上出现了新的可疑内容");
            let id = bus.next_id();
            bus.emit(tw_api::Event::ScanAlert {
                id,
                alerts: fresh.iter().map(finding_view).collect(),
                at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            });
        }
    });
    Ok(Arc::new(w))
}
