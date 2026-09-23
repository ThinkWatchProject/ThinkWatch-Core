//! 监听设置：网关在哪儿听、放行哪些来源。
//!
//! # 为什么不走通用的配置补丁
//!
//! 监听是唯一一处「写进去了却可能生效不了」的设置：端口可能被别的程序
//! 占着，网卡可能此刻没有地址。补丁只管写文件，写完之后网关守着旧地址，
//! 配置文件却说着新地址 —— 两边从那一刻起各说各的，而界面上看到的是
//! 「保存成功」。这个接口在写之前先试着绑一下，**绑不上就不写**，并且
//! 说清为什么。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::put;
use serde_yaml_ng::Value;
use tw_config::edit;
use tw_config::history::Origin;
use tw_yaml::Step;

use crate::{ControlState, Fail, apply_fail, fail};
use tw_types::msg;

pub fn router() -> axum::Router<ControlState> {
    axum::Router::new().route("/listen", put(save))
}

async fn save(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::ListenSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    // `bind` 的写法和配置文件里一样，按同一套规则读 —— 界面和手写的
    // 配置不该有两种说法
    let bind: tw_config::Bind =
        serde_yaml_ng::from_value(Value::String(req.bind.trim().to_string())).map_err(|_| {
            fail(
                StatusCode::BAD_REQUEST,
                // 交进来的是一个字符串，**读不成 `Bind` 只有一种可能**：四种写法
                // 都不是。那句话是 `Bind` 自己的 serde 报错，这里照着说一遍、带上码
                msg!(
                    "control.listen.bind_invalid", bind = req.bind.trim() =>
                    "bind takes loopback, all, the name of an interface such as en0, or an \
                     address such as 192.168.1.5; it reads {bind}"
                ),
            )
        })?;
    if req.port == 0 {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            msg!("control.listen.bad_port" => "The port must be between 1 and 65535."),
        ));
    }
    let allow: Vec<String> = req
        .allow_from
        .iter()
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect();
    let listen = tw_config::GatewayListen {
        bind: bind.clone(),
        port: req.port,
        allow_from: allow.clone(),
    };
    // 网卡名要问系统：拼错了、此刻没有地址，都在这一步说
    let want = listen
        .addrs()
        .map_err(|e| fail(StatusCode::CONFLICT, tw_gateway::listen::unresolved(&e)))?;
    tw_gateway::listen::check(&s.gateway, &want)
        .await
        .map_err(|m| fail(StatusCode::CONFLICT, m))?;

    let version = s
        .cfg
        .transform(req.base_version.as_deref(), Origin::Ui, |text, _| {
            let at = |k: &'static str| [Step::key("listen"), Step::key("gateway"), Step::key(k)];
            // **默认值不写进文件。**出厂的配置里没有这一段，存一次「仅本机」
            // 不该让它凭空多出三行。放行网段是空的时候要写成 `[]`：不写是
            // 默认名单，不是空
            let bind_v =
                (bind != tw_config::Bind::Loopback).then(|| Value::String(bind.to_string()));
            let port_v = (req.port != tw_config::DEFAULT_GATEWAY_PORT)
                .then(|| Value::Number(req.port.into()));
            let allow_v = (allow != tw_config::default_allow_from())
                .then(|| Value::Sequence(allow.iter().cloned().map(Value::String).collect()));
            let out = edit::set(text, &at("bind"), bind_v.as_ref())?;
            let out = edit::set(&out, &at("port"), port_v.as_ref())?;
            Ok(edit::set(&out, &at("allow_from"), allow_v.as_ref())?)
        })
        .await
        .map_err(apply_fail)?;
    // 上一次没换成、这次原样再存的时候，配置没变，换入那一步不会去叫
    // 监听那一边。这里叫一次，让它照着当前配置再对一遍
    s.gateway.relisten();
    Ok(Json(tw_api::ConfigWritten { version }))
}
