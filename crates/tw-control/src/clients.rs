//! 客户端接管的控制面（DESIGN.md §7.11）。
//!
//! 这是全项目唯一会写用户其他软件配置的地方，所以端点的形状也是刻意的：
//! **`plan` 和 `adopt` 是两个端点**，中间必须夹一次人的确认。一个
//! 「一步接管」的端点会顺手到没有人记得展示 diff。

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use tw_adopt::clients::{Gateway, adoptable, manual_only};
use tw_adopt::{detect, plan};

use crate::ControlState;

type Fail = (StatusCode, String);

fn find(id: &str) -> Result<tw_adopt::clients::Client, Fail> {
    adoptable()
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("没有叫 `{id}` 的客户端")))
}

/// 客户端该连的地址。
///
/// **不是监听地址。**`0.0.0.0` 是「我在所有网卡上听」，把它填进客户端
/// 配置里，客户端会去连一个不存在的主机。
fn gateway_base(s: &ControlState) -> String {
    format!("http://127.0.0.1:{}", s.config().listen.gateway.port)
}

fn gateway_for(s: &ControlState, key_name: Option<&str>) -> Result<Gateway, Fail> {
    let cfg = s.config();
    // §0.6：为「一个 key 就够」的人设计 —— 没指定就用第一把。
    let key = match key_name {
        Some(n) => cfg.clients.iter().find(|c| c.name == n).ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("config.yaml 里没有叫 `{n}` 的网关密钥"),
            )
        })?,
        None => cfg.clients.first().ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                "config.yaml 里还没有任何网关密钥 —— 先在配置页加一个，再来接管客户端。"
                    .to_string(),
            )
        })?,
    };
    Ok(Gateway {
        base: gateway_base(s),
        key: Some(key.key.clone()),
    })
}

pub async fn list(State(s): State<ControlState>) -> Result<Json<tw_api::ClientsResponse>, Fail> {
    // 观察窗口的依据：**我们改了一个文件，但那个文件有没有被读到，
    // 只有请求能证明**（§7.11）。
    let seen: Vec<(String, i64)> = match &s.store {
        Some(st) => st.lock().await.db().last_seen_by_hint().unwrap_or_default(),
        None => Vec::new(),
    };
    let clients = detect::detect(&s.home)
        .into_iter()
        .map(|d| tw_api::DetectedClient {
            last_seen_ms: seen
                .iter()
                .find(|(h, _)| h == d.id)
                .map(|(_, at)| *at as u64),
            id: d.id.to_string(),
            name: d.name.to_string(),
            path: d.path.display().to_string(),
            real: d.real.display().to_string(),
            installed: d.installed,
            has_config: d.has_config,
            adopted_at_ms: d.adopted_at_ms,
            endpoint: d.endpoint,
            shadows: d.shadows.iter().map(|p| p.display().to_string()).collect(),
            takes_effect: match d.takes_effect {
                tw_adopt::clients::TakesEffect::Immediately => "immediately".into(),
                tw_adopt::clients::TakesEffect::OnRestart => "on_restart".into(),
            },
            takes_effect_note: d.takes_effect.note().to_string(),
            warns_when_silent: d.takes_effect.warns_when_silent(),
            verified: match d.verified {
                tw_adopt::clients::Verified::Measured => "measured".into(),
                tw_adopt::clients::Verified::FieldsOnly => "fields_only".into(),
            },
            verified_note: d.verified.note().to_string(),
            costs: d.costs,
        })
        .collect();
    Ok(Json(tw_api::ClientsResponse {
        clients,
        manual: manual_only()
            .into_iter()
            .map(|m| tw_api::ManualClient {
                name: m.name.to_string(),
                how: m.how.to_string(),
                caveat: m.caveat.to_string(),
            })
            .collect(),
        gateway_base: gateway_base(&s),
        keys: s.config().clients.iter().map(|c| c.name.clone()).collect(),
    }))
}

fn view(p: &plan::Plan, fields: Vec<String>) -> tw_api::PlanView {
    tw_api::PlanView {
        client: p.client.clone(),
        path: p.path.display().to_string(),
        before: p.before.clone(),
        after: p.after.clone(),
        notes: p.notes.clone(),
        shadows: p.shadows.iter().map(|x| x.display().to_string()).collect(),
        noop: p.is_noop(),
        carries_secret: p.carries_secret,
        fields,
    }
}

/// 算一份接管改动。**不写任何东西。**
pub async fn plan_adopt(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::AdoptRequest>,
) -> Result<Json<tw_api::PlanView>, Fail> {
    let c = find(&req.client)?;
    let gw = gateway_for(&s, req.key_name.as_deref())?;
    let p = plan::plan_adopt(&c, &s.home, &gw).map_err(bad)?;
    let fields = p
        .targets
        .iter()
        .map(|t| match t {
            plan::Target::Set(path, v) => {
                // **密钥不回显**，哪怕是打码的
                let shown = if path.iter().any(|k| {
                    let k = k.to_ascii_lowercase();
                    k.contains("token") || k.contains("key")
                }) {
                    "（那把网关密钥）".to_string()
                } else {
                    v.to_line()
                };
                format!("{} = {shown}", path.join("."))
            }
            plan::Target::Remove(path) => format!("删掉 {}", path.join(".")),
        })
        .collect();
    Ok(Json(view(&p, fields)))
}

/// 算一份还原改动。**不写任何东西。**
pub async fn plan_restore(
    State(s): State<ControlState>,
    Path(id): Path<String>,
) -> Result<Json<tw_api::PlanView>, Fail> {
    let c = find(&id)?;
    let p = plan::plan_restore(&c, &s.home).map_err(bad)?;
    Ok(Json(view(&p, Vec::new())))
}

fn bad(e: plan::PlanError) -> Fail {
    let code = match &e {
        plan::PlanError::NoRecord { .. } => StatusCode::NOT_FOUND,
        plan::PlanError::ForeignSidecar { .. } => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    (code, e.to_string())
}

/// 落盘。**用户在 diff 上点过确认之后才该到这里。**
pub async fn adopt(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::AdoptRequest>,
) -> Result<Json<tw_api::AdoptResponse>, Fail> {
    let c = find(&req.client)?;
    let gw = gateway_for(&s, req.key_name.as_deref())?;
    let p = plan::plan_adopt(&c, &s.home, &gw).map_err(bad)?;
    let a = plan::apply(&c, &p, &tw_adopt::foreign::backup_root()).map_err(bad)?;
    Ok(Json(tw_api::AdoptResponse {
        real: a.real.display().to_string(),
        backup: a.backup.display().to_string(),
        created: a.created,
        warnings: a.warnings,
        // **在接管完成那一屏说，不是等五分钟后再说**（§7.11）
        takes_effect_note: c.takes_effect.note().to_string(),
    }))
}

/// 还原。
///
/// **走的是「把我们写的那几个字段改回去」，不是「拿全文备份覆盖」**
/// —— 后者会把用户这三个月里加的 MCP server、调的权限、写的 hook 全部
/// 抹掉（§7.15）。
pub async fn restore(
    State(s): State<ControlState>,
    Path(id): Path<String>,
) -> Result<Json<tw_api::AdoptResponse>, Fail> {
    let c = find(&id)?;
    let p = plan::plan_restore(&c, &s.home).map_err(bad)?;
    let a = plan::apply_restore(&c, &p, &tw_adopt::foreign::backup_root()).map_err(bad)?;
    Ok(Json(tw_api::AdoptResponse {
        real: a.real.display().to_string(),
        backup: a.backup.display().to_string(),
        created: false,
        warnings: p.notes,
        takes_effect_note: c.takes_effect.note().to_string(),
    }))
}

/// 「我明明配了，为什么没生效」—— 走一遍优先级链。
pub async fn why(
    State(s): State<ControlState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<tw_api::FindingView>>, Fail> {
    let c = find(&id)?;
    Ok(Json(
        detect::diagnose(&c, &s.home, None)
            .into_iter()
            .map(|f| tw_api::FindingView {
                level: match f.level {
                    detect::Level::Blocking => "blocking".into(),
                    detect::Level::Suspect => "suspect".into(),
                    detect::Level::Clear => "clear".into(),
                },
                title: f.title,
                detail: f.detail,
                fix: f.fix,
            })
            .collect(),
    ))
}
