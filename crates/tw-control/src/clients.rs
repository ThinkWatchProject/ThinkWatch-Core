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
    let (_, key, _) = prepare_key(s, client.id, key_name).await?;
    Ok(Gateway {
        base: gateway_base(s),
        key: Some(key),
    })
}

/// 为这个客户端准备一把钥匙：指名的 → 为它留着的 → 新建一把绑给它。
/// 返回（名字，明文，是不是这次新建的）。
///
/// 接管和手动配置走的是同一条：手动配 Cursor 时拿到的那把，和接管 Claude Code
/// 时生成的那把一样，都记着是为谁生成的 —— 流量、路由、并发上限才分得清是谁。
async fn prepare_key(
    s: &ControlState,
    client: &str,
    key_name: Option<&str>,
) -> Result<(String, String, bool), Fail> {
    let chosen = {
        let cfg = s.config();
        let picked = key_for(&cfg, client, key_name)?;
        // 指名的、或者已经为它留着的：认这一把。否则为它新建
        let mine = key_name.is_some() || cfg.client_key(client).is_some();
        if mine { Some(picked) } else { None }
    };
    if let Some(c) = chosen {
        // 指名一把还没绑过的，就此绑给它 —— 否则「这把是谁的」这件事只存在于
        // 用户此刻的记忆里
        if c.client.as_deref() != Some(client) {
            bind(s, &c.name, client).await?;
        }
        return Ok((c.name, c.key, false));
    }
    let name = free_name(&s.config(), client);
    let key = tw_config::generate_key();
    let item = tw_config::Client {
        name: name.clone(),
        key: key.clone(),
        client: Some(client.to_string()),
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
    Ok((name, key, true))
}

/// 为一个客户端准备它的专用密钥（`POST /clients/{id}/key`）。**手动配置时用**：
/// 接管不了的 Cursor、没检测到的 Claude Code，照着步骤填进去的应该是一把只属于
/// 它的钥匙，而不是默认那把。已经有为它留着的就直接给那把。
pub async fn client_key(
    State(s): State<ControlState>,
    Path(id): Path<String>,
) -> Result<Json<tw_api::ClientKey>, Fail> {
    let known = adoptable().iter().any(|c| c.id == id) || manual_only().iter().any(|m| m.id == id);
    if !known {
        return Err(fail(
            StatusCode::NOT_FOUND,
            msg!("control.client_unknown", client = id.clone() => "`{client}` is not a client we know."),
        ));
    }
    let (name, key, created) = prepare_key(&s, &id, None).await?;
    Ok(Json(tw_api::ClientKey { name, key, created }))
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
    // 只有请求能证明** —— 而且是带着为它生成的那把密钥的请求。按请求头里
    // 自报的客户端标识算的话，一个没接管的客户端冒用那个标识就能让它显示成
    // 「使用中」
    let seen: Vec<(String, i64)> = match &s.store {
        Some(st) => st
            .lock()
            .await
            .db()
            .last_seen_by_client()
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let cfg = s.config();
    let key_of = |id: &str| cfg.client_key(id).map(|c| c.name.clone());
    let seen_of = |key: &Option<String>| {
        key.as_ref()
            .and_then(|k| seen.iter().find(|(n, _)| n == k).map(|(_, at)| *at as u64))
    };
    let base = gateway_base(&s);
    // 手动配置时要写的字段按一把占位的密钥算：值本来就不回显，只要知道哪一项是密钥
    let gw = Gateway {
        base: base.clone(),
        key: Some(String::new()),
    };
    let defs = adoptable();
    let clients = detect::detect(&s.home)
        .into_iter()
        .map(|d| {
            let key = key_of(d.id);
            let manual = defs
                .iter()
                .find(|c| c.id == d.id)
                .map(|c| setup_of(c, &gw))
                .unwrap_or_else(|| tw_api::ManualSetup {
                    steps: Vec::new(),
                    fields: Vec::new(),
                    endpoint: base.clone(),
                });
            tw_api::DetectedClient {
                last_seen_ms: seen_of(&key),
                key,
                manual,
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
            }
        })
        .collect();
    let manual = manual_only()
        .into_iter()
        .map(|m| {
            let key = key_of(m.id);
            tw_api::ManualClient {
                id: m.id.to_string(),
                name: m.name.to_string(),
                last_seen_ms: seen_of(&key),
                key,
                setup: tw_api::ManualSetup {
                    steps: m.steps(),
                    fields: Vec::new(),
                    endpoint: m.endpoint(&gw),
                },
                caveat: m.caveat(),
            }
        })
        .collect();
    Ok(Json(tw_api::ClientsResponse {
        clients,
        manual,
        keys: cfg.clients.iter().map(|c| c.name.clone()).collect(),
        gateway_base: base,
    }))
}

/// 手动配置一个能接管的客户端：打开哪个文件、写哪几项、填哪个地址。
/// 写的那几项就是接管时写的那几项 —— 两条路写出来的配置一模一样。
fn setup_of(c: &tw_adopt::clients::Client, gw: &Gateway) -> tw_api::ManualSetup {
    tw_api::ManualSetup {
        steps: c.manual_steps(),
        fields: tw_adopt::clients::edits(c, gw)
            .iter()
            .map(|e| field("set", &e.path, Some(&e.value), e.secret))
            .collect(),
        endpoint: c.endpoint(gw),
    }
}

/// 一处字段改动在界面上的样子。**密钥不回显**，哪怕是打码的。
fn field(
    op: &str,
    path: &[String],
    value: Option<&tw_adopt::json::Val>,
    secret: bool,
) -> tw_api::FieldChange {
    tw_api::FieldChange {
        op: op.to_string(),
        path: path.join("."),
        value: if secret {
            None
        } else {
            value.map(|v| v.to_line())
        },
        secret,
    }
}

/// 接管这个客户端时哪几项是密钥。按一把占位的密钥算 —— 只看路径
fn secret_paths(c: &tw_adopt::clients::Client) -> Vec<Vec<String>> {
    let gw = Gateway {
        base: String::new(),
        key: Some(String::new()),
    };
    tw_adopt::clients::edits(c, &gw)
        .into_iter()
        .filter(|e| e.secret)
        .map(|e| e.path)
        .collect()
}

fn fields_of(p: &plan::Plan, secrets: &[Vec<String>]) -> Vec<tw_api::FieldChange> {
    p.targets
        .iter()
        .map(|t| match t {
            plan::Target::Set(path, v) => field("set", path, Some(v), secrets.contains(path)),
            plan::Target::Remove(path) => field("remove", path, None, secrets.contains(path)),
        })
        .collect()
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
        key: None,
        key_created: false,
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
    let mut v = view(&p, fields_of(&p, &secret_paths(&c)), gw.key.as_deref());
    // 落盘时写进去的是哪一把：和 `prepare_key` 同一个顺序 —— 指名的、为它留着的，
    // 都没有就是这次新建的那把。**要在确认之前说**：新建一把和沿用一把，
    // 对用户是两件事
    let cfg = s.config();
    let (name, created) = match (req.key_name.as_deref(), cfg.client_key(c.id)) {
        (Some(n), _) => (n.to_string(), false),
        (None, Some(k)) => (k.name.clone(), false),
        (None, None) => (free_name(&cfg, c.id), true),
    };
    v.key = Some(name);
    v.key_created = created;
    Ok(Json(v))
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
    let mut v = view(&p, fields_of(&p, &secret_paths(&c)), None);
    for k in &keys {
        v.before = v.before.as_deref().map(|t| mask(t, Some(k)));
        v.after = mask(&v.after, Some(k));
    }
    // 还原不删密钥：说清留下的是哪一把，下次接管直接用它
    v.key = s.config().client_key(c.id).map(|k| k.name.clone());
    Ok(Json(v))
}

fn bad(e: plan::PlanError) -> Fail {
    let code = match &e {
        plan::PlanError::NoRecord { .. } => StatusCode::NOT_FOUND,
        plan::PlanError::ForeignSidecar { .. } => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    fail(code, e.msg())
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
    tw_adopt::mcp::target(id).map_err(|e| fail(StatusCode::NOT_FOUND, e.msg()))
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
    fail(code, e.msg())
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
                why_not: t.why_not(),
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
            secret: false,
        }],
        key: None,
        key_created: false,
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
