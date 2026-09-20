//! 客户端接管的控制面。
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
use tw_types::msg;

use crate::{Fail, fail};

fn find(id: &str) -> Result<tw_adopt::clients::Client, Fail> {
    adoptable().into_iter().find(|c| c.id == id).ok_or_else(|| {
        fail(
            StatusCode::NOT_FOUND,
            msg!("control.client_unknown", client = id => "`{client}` is not a client we know."),
        )
    })
}

/// 客户端该连的地址。
///
/// **不是监听地址。**`0.0.0.0` 是「我在所有网卡上听」，把它填进客户端
/// 配置里，客户端会去连一个不存在的主机。
fn gateway_base(s: &ControlState) -> String {
    format!("http://127.0.0.1:{}", s.config().listen.gateway.port)
}

/// 这次接管该用哪把钥匙。
///
/// 顺序：**指名的 → 为这个客户端留着的 → 默认的**。
///
/// 中间那一条是「取消接管之后密钥不删」的另一半：再次接管时直接接着用同一把，
/// 用户不必重新配置，也不会在配置里攒下一堆同名的钥匙。
fn key_for(
    cfg: &tw_config::Config,
    client: &str,
    key_name: Option<&str>,
) -> Result<tw_config::Client, Fail> {
    if let Some(n) = key_name {
        return cfg
            .clients
            .iter()
            .find(|c| c.name == n)
            .cloned()
            .ok_or_else(|| {
                fail(
                    StatusCode::NOT_FOUND,
                    msg!(
                        "control.key_not_found", key = n =>
                        "config.yaml has no gateway key named `{key}`."
                    ),
                )
            });
    }
    if let Some(c) = cfg.client_key(client) {
        return Ok(c.clone());
    }
    cfg.default_client().cloned().ok_or_else(|| {
        fail(
            StatusCode::CONFLICT,
            msg!(
                "control.no_keys" =>
                "config.yaml has no gateway key yet. Create one before pointing a client at the \
                 gateway."
            ),
        )
    })
}

fn gateway_for(s: &ControlState, client: &str, key_name: Option<&str>) -> Result<Gateway, Fail> {
    let key = key_for(&s.config(), client, key_name)?;
    Ok(Gateway {
        base: gateway_base(s),
        key: Some(key.key.clone()),
    })
}

/// 接管之前先把钥匙准备好：**每个被接管的客户端有自己的一把**。
///
/// 共用一把的后果是连锁的：请求记录里分不出是谁发的，按密钥绑路由匹不到，
/// 每客户端并发上限形同虚设 —— 三样东西一起失效，而原因只是少了一把钥匙。
///
/// 已经有一把为它留着的（包括取消接管后留下的）就用那把，只补上绑定；
/// 一把也没有才新建。
async fn ensure_key(
    s: &ControlState,
    client: &tw_adopt::clients::Client,
    key_name: Option<&str>,
) -> Result<Gateway, Fail> {
    let chosen = {
        let cfg = s.config();
        let picked = key_for(&cfg, client.id, key_name)?;
        // 指名的、或者已经为它留着的：认这一把。否则为它新建
        let mine = key_name.is_some() || cfg.client_key(client.id).is_some();
        if mine { Some(picked) } else { None }
    };
    if let Some(c) = chosen {
        // 指名一把还没绑过的，就此绑给它 —— 否则「这把是谁的」这件事只存在于
        // 用户此刻的记忆里
        if c.client.as_deref() != Some(client.id) {
            bind(s, &c.name, client.id).await?;
        }
        return Ok(Gateway {
            base: gateway_base(s),
            key: Some(c.key),
        });
    }
    let name = free_name(&s.config(), client.id);
    let key = tw_config::generate_key();
    let item = tw_config::Client {
        name: name.clone(),
        key: key.clone(),
        client: Some(client.id.to_string()),
        ..Default::default()
    };
    s.cfg
        .transform(None, tw_config::history::Origin::Ui, |text, _| {
            Ok(tw_config::edit::upsert(
                text,
                crate::keys::CLIENTS,
                None,
                &crate::resources::mapping(&item)?,
            )?)
        })
        .await
        .map_err(|e| {
            fail(
                StatusCode::CONFLICT,
                msg!("control.key_create_failed", detail = e => "The gateway key could not be created: {detail}"),
            )
        })?;
    Ok(Gateway {
        base: gateway_base(s),
        key: Some(key),
    })
}

/// 把一把已有的钥匙记成某个客户端的。
async fn bind(s: &ControlState, key: &str, client: &str) -> Result<(), Fail> {
    let key = key.to_string();
    let client = client.to_string();
    s.cfg
        .transform(None, tw_config::history::Origin::Ui, |text, cfg| {
            let Some(i) = cfg.clients.iter().position(|c| c.name == key) else {
                return Ok(text.to_string());
            };
            Ok(tw_config::edit::set(
                text,
                &[
                    tw_yaml::Step::key("clients"),
                    tw_yaml::Step::Index(i),
                    tw_yaml::Step::key("client"),
                ],
                Some(&serde_yaml_ng::Value::String(client.clone())),
            )?)
        })
        .await
        .map_err(|e| {
            fail(
                StatusCode::CONFLICT,
                msg!("control.key_bind_failed", detail = e => "The key's owner could not be recorded: {detail}"),
            )
        })?;
    Ok(())
}

/// 没被占用的密钥名。客户端 id 本身被占了就往后编号
fn free_name(cfg: &tw_config::Config, id: &str) -> String {
    if !cfg.clients.iter().any(|c| c.name == id) {
        return id.to_string();
    }
    (2..)
        .map(|n| format!("{id}-{n}"))
        .find(|n| !cfg.clients.iter().any(|c| &c.name == n))
        .unwrap_or_else(|| id.to_string())
}

pub async fn list(State(s): State<ControlState>) -> Result<Json<tw_api::ClientsResponse>, Fail> {
    // 观察窗口的依据：**我们改了一个文件，但那个文件有没有被读到，
    // 只有请求能证明**。
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
            takes_effect: d.takes_effect.slug().to_string(),
            warns_when_silent: d.takes_effect.warns_when_silent(),
            verified: d.verified.slug().to_string(),
            costs: d.costs,
        })
        .collect();
    Ok(Json(tw_api::ClientsResponse {
        clients,
        manual: manual_only()
            .into_iter()
            .map(|m| tw_api::ManualClient {
                name: m.name.to_string(),
                how: m.how(&Gateway {
                    base: gateway_base(&s),
                    key: None,
                }),
                caveat: m.caveat.to_string(),
            })
            .collect(),
        gateway_base: gateway_base(&s),
        keys: s.config().clients.iter().map(|c| c.name.clone()).collect(),
    }))
}

/// 密钥在界面上的样子。
///
/// **界面上永远不显示真正的密钥**，diff 里也不行 —— 用户会截图这一屏
/// 来问「这样对吗」。落盘写的仍然是真值，[`tw_api::PlanView`] 上那两个
/// 字段的文档里写清了这一点。
const MASK: &str = "«the gateway key from config.yaml»";

fn mask(text: &str, key: Option<&str>) -> String {
    match key {
        // 空 key 会把每个字符之间都插一遍，那不是脱敏是毁掉整份 diff
        Some(k) if !k.is_empty() => text.replace(k, MASK),
        _ => text.to_string(),
    }
}

fn view(p: &plan::Plan, fields: Vec<tw_api::FieldChange>, key: Option<&str>) -> tw_api::PlanView {
    tw_api::PlanView {
        client: p.client.clone(),
        path: p.path.display().to_string(),
        before: p.before.as_deref().map(|t| mask(t, key)),
        after: mask(&p.after, key),
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
    // **算一份改动不该写任何东西**，所以这里不建密钥：没有的话按默认那把算，
    // 而 diff 里的密钥本来就是打码的
    let gw = gateway_for(&s, c.id, req.key_name.as_deref())?;
    let p = plan::plan_adopt(&c, &s.home, &gw).map_err(bad)?;
    let fields = p
        .targets
        .iter()
        .map(|t| match t {
            plan::Target::Set(path, v) => tw_api::FieldChange {
                op: "set".into(),
                path: path.join("."),
                // **密钥不回显**，哪怕是打码的
                value: (!path.iter().any(|k| {
                    let k = k.to_ascii_lowercase();
                    k.contains("token") || k.contains("key")
                }))
                .then(|| v.to_line()),
            },
            plan::Target::Remove(path) => tw_api::FieldChange {
                op: "remove".into(),
                path: path.join("."),
                value: None,
            },
        })
        .collect();
    Ok(Json(view(&p, fields, gw.key.as_deref())))
}

/// 算一份还原改动。**不写任何东西。**
pub async fn plan_restore(
    State(s): State<ControlState>,
    Path(id): Path<String>,
) -> Result<Json<tw_api::PlanView>, Fail> {
    let c = find(&id)?;
    let p = plan::plan_restore(&c, &s.home).map_err(bad)?;
    // 还原的 diff 里，**要打码的是用户自己的原始密钥** —— 它正要被写
    // 回去，而它比我们那把更不该出现在截图里
    let keys: Vec<String> = s.config().clients.iter().map(|c| c.key.clone()).collect();
    let mut v = view(&p, Vec::new(), None);
    for k in &keys {
        v.before = v.before.as_deref().map(|t| mask(t, Some(k)));
        v.after = mask(&v.after, Some(k));
    }
    Ok(Json(v))
}

fn bad(e: plan::PlanError) -> Fail {
    let code = match &e {
        plan::PlanError::NoRecord { .. } => StatusCode::NOT_FOUND,
        plan::PlanError::ForeignSidecar { .. } => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    fail(code, msg!("control.adopt_failed", detail = e => "{detail}"))
}

/// 落盘。**用户在 diff 上点过确认之后才该到这里。**
pub async fn adopt(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::AdoptRequest>,
) -> Result<Json<tw_api::AdoptResponse>, Fail> {
    let c = find(&req.client)?;
    // 落盘这一步才建钥匙：**先有钥匙再写对方的配置** —— 反过来的话，中间那一刻
    // 对方配置里写着一把 config.yaml 里没有的钥匙
    let gw = ensure_key(&s, &c, req.key_name.as_deref()).await?;
    let p = plan::plan_adopt(&c, &s.home, &gw).map_err(bad)?;
    let a = plan::apply(&c, &p, &tw_adopt::foreign::backup_root()).map_err(bad)?;
    Ok(Json(tw_api::AdoptResponse {
        real: a.real.display().to_string(),
        backup: a.backup.display().to_string(),
        created: a.created,
        warnings: a.warnings,
        // **在接管完成那一屏说，不是等五分钟后再说**
        takes_effect: c.takes_effect.slug().to_string(),
    }))
}

/// 还原。
///
/// **走的是「把我们写的那几个字段改回去」，不是「拿全文备份覆盖」**
/// —— 后者会把用户这三个月里加的 MCP server、调的权限、写的 hook 全部
/// 抹掉。
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
        takes_effect: c.takes_effect.slug().to_string(),
    }))
}

// ---------------------------------------------------------------- MCP 矩阵

fn mcp_target(id: &str) -> Result<tw_adopt::mcp::Target, Fail> {
    tw_adopt::mcp::target(id).map_err(|e| {
        fail(
            StatusCode::NOT_FOUND,
            msg!("control.mcp_target_unknown", detail = e => "{detail}"),
        )
    })
}

fn mcp_err(e: tw_adopt::mcp::McpError) -> Fail {
    let code = match &e {
        tw_adopt::mcp::McpError::UnknownClient(_) | tw_adopt::mcp::McpError::NotThere { .. } => {
            StatusCode::NOT_FOUND
        }
        // 「我们没验证过那个格式」不是用户做错了什么，但也确实做不了
        tw_adopt::mcp::McpError::NotCopyable { .. } => StatusCode::NOT_IMPLEMENTED,
        _ => StatusCode::BAD_REQUEST,
    };
    fail(code, msg!("control.mcp_failed", detail = e => "{detail}"))
}

/// 能写和不能写的分别是哪些。
pub async fn mcp_targets(State(_s): State<ControlState>) -> Json<Vec<tw_api::McpTargetView>> {
    Json(
        tw_adopt::mcp::targets()
            .into_iter()
            .map(|t| tw_api::McpTargetView {
                client: t.client.to_string(),
                name: t.name.to_string(),
                path: t.config.to_string(),
                copyable: t.copyable,
                why_not: t.why_not.to_string(),
            })
            .collect(),
    )
}

fn mcp_plan(s: &ControlState, req: &tw_api::McpOpRequest) -> Result<tw_adopt::mcp::Plan, Fail> {
    let to = mcp_target(&req.to)?;
    match req.op.as_str() {
        "remove" => tw_adopt::mcp::plan_remove(&to, &s.home, &req.name).map_err(mcp_err),
        "copy" => {
            let from = mcp_target(req.from.as_deref().unwrap_or_default())?;
            let v = tw_adopt::mcp::read_server(&from, &s.home, &req.name).map_err(mcp_err)?;
            tw_adopt::mcp::plan_copy(&to, &s.home, &req.name, &v).map_err(mcp_err)
        }
        other => Err(fail(
            StatusCode::BAD_REQUEST,
            msg!("control.unsupported_action", action = other => "`{action}` is not an action we support."),
        )),
    }
}

/// 算一份改动。**不写任何东西** —— 和接管一样，中间夹一次人的确认。
pub async fn mcp_plan_op(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::McpOpRequest>,
) -> Result<Json<tw_api::PlanView>, Fail> {
    let p = mcp_plan(&s, &req)?;
    Ok(Json(tw_api::PlanView {
        client: p.client,
        path: p.path.display().to_string(),
        before: p.before,
        after: p.after,
        notes: Vec::new(),
        shadows: Vec::new(),
        noop: p.noop,
        // MCP 的 env 里可能有密钥，而我们正把它抄进另一个文件
        carries_secret: true,
        fields: vec![tw_api::FieldChange {
            op: if p.remove { "remove" } else { "set" }.to_string(),
            path: p.field.join("."),
            // 值是一整段 server 配置，里面可能有密钥，diff 里已经能看到打过码的样子
            value: None,
        }],
    }))
}

pub async fn mcp_apply(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::McpOpRequest>,
) -> Result<Json<tw_api::AdoptResponse>, Fail> {
    let to = mcp_target(&req.to)?;
    let p = mcp_plan(&s, &req)?;
    let a = tw_adopt::mcp::apply(&to, &p, &tw_adopt::foreign::backup_root()).map_err(mcp_err)?;
    Ok(Json(tw_api::AdoptResponse {
        real: a.real.display().to_string(),
        backup: a.backup.display().to_string(),
        created: a.created,
        warnings: a.warnings,
        // 客户端只在启动时读 MCP 配置
        takes_effect: tw_adopt::clients::TakesEffect::OnRestart.slug().to_string(),
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
