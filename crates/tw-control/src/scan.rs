//! 静态扫描的控制面。
//!
//! **每次请求现扫一遍，什么都不存。**没有「同步状态」
//! 这个概念，也就没有「同步失效了」「主清单过期了」这类问题 —— 你看到的
//! 永远是磁盘上此刻的真实情况。
//!
//! 代价是每次打开页面要读几十个文件。那是几毫秒，换掉一整类状态一致性
//! 问题。

use std::sync::Arc;

use axum::{Json, extract::State};

use crate::ControlState;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 把一条发现变成给界面看的样子。
pub fn finding_view(f: &tw_scan::report::Finding) -> tw_api::ScanFinding {
    tw_api::ScanFinding {
        level: f.level.slug().to_string(),
        rule: f.rule.clone(),
        kind: f.kind.slug().to_string(),
        client: f.client.clone(),
        path: f.path.display().to_string(),
        line: f.line,
        title: f.title.clone(),
        detail: f.detail.clone(),
        excerpt: f.excerpt.clone(),
    }
}

pub async fn scan(
    State(s): State<ControlState>,
    Json(p): Json<tw_api::ScanRequest>,
) -> Json<tw_api::ScanResponse> {
    // **只用内置规则。**安全页上的规则只作用于经过网关的请求：在那边停用
    // 一条误报，不该让这边悄悄少查一样东西
    let rules = tw_guard::tools::rules::scan_rules();

    let mut sources = tw_scan::sources::user_level(&s.home);
    for proj in &p.projects {
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
        projects: p.projects,
    })
}

/// 盯着配置面，**只在有新东西出现时**发事件。
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
    let dirs = tw_scan::watch::dirs_for(&tw_scan::sources::user_level(&home));
    tracing::debug!(
        dirs = dirs.len(),
        "watching the clients' configuration surface"
    );
    let (w, mut rx) = tw_scan::watch::watch(&dirs)?;

    let bus = state.bus().clone();
    tokio::spawn(async move {
        let mut seen = tw_scan::watch::Seen::default();
        // 先垫一次底：把此刻已经存在的那些记下来，它们不算「新出现」
        // 内置规则，和打开页面时扫的是同一套
        let rules = tw_guard::tools::rules::scan_rules();
        let scan_now = |home: &std::path::Path| {
            tw_scan::report::scan(&tw_scan::sources::user_level(home), &rules)
        };
        seen.diff(&scan_now(&home).findings);

        while rx.recv().await.is_some() {
            // **文件动了本身就是一条消息，和「可疑不可疑」无关。**接管
            // 状态读的就是这几个文件（`ANTHROPIC_BASE_URL` 指向哪儿），
            // 用户在编辑器里把它改回去一点都不可疑，但界面必须跟上。
            // 没有这条，客户端那一页只能每五秒重扫一次磁盘。
            let id = bus.next_id();
            bus.emit(tw_api::Event::ClientsChanged {
                id,
                at_ms: now_ms(),
            });

            // **每次重新枚举来源**：用户可能刚加了一个 skill，
            // 而那个文件在启动时还不存在
            let fresh = seen.diff(&scan_now(&home).findings);
            if fresh.is_empty() {
                continue;
            }
            tracing::info!(
                count = fresh.len(),
                "something new and suspicious appeared in the clients' configuration"
            );
            let id = bus.next_id();
            bus.emit(tw_api::Event::ScanAlert {
                id,
                alerts: fresh.iter().map(finding_view).collect(),
                at_ms: now_ms(),
            });
        }
    });
    Ok(Arc::new(w))
}
