//! 为某个客户端发一把专用的网关密钥（`POST /clients/{id}/key`）。
//!
//! 接管、还原、MCP、扫描都不在这里：它们改的是桌面端所在那台机器上别的软件的
//! 配置，由桌面端自己做。桌面端接管一个客户端、或者手动配置时，要的只是一把
//! 属于这个客户端的钥匙 —— 而钥匙写在 config.yaml 里，只有 core 发得出来。
//!
//! **客户端认不认得由桌面端判断**（客户端清单在那边）。这里只保证 id 是一个
//! 能当密钥名、能写进配置的词。

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use tw_types::msg;

use crate::ControlState;
use crate::{Fail, fail};

/// 一个像客户端 id 的词：小写字母、数字、`-`，字母或数字开头。桌面端的客户端
/// id 都是这个样子（`claude-code`、`gemini-cli`）；别的样子一律不认，免得一个
/// 随手写的名字进了 config.yaml
fn looks_like_client(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// 为一个客户端准备它的专用密钥。**为它留着的就给那把**（取消接管之后密钥不删：
/// 再次接管时接着用同一把，用户不必重新配置，也不会在配置里攒下一堆同名的钥匙）；
/// 一把也没有才新建一把绑给它。
///
/// 共用一把的后果是连锁的：请求记录里分不出是谁发的，按密钥绑路由匹不到，
/// 每客户端并发上限形同虚设 —— 三样东西一起失效，而原因只是少了一把钥匙。
pub async fn client_key(
    State(s): State<ControlState>,
    Path(id): Path<String>,
) -> Result<Json<tw_api::ClientKey>, Fail> {
    if !looks_like_client(&id) {
        return Err(fail(
            StatusCode::NOT_FOUND,
            msg!("control.client_unknown", client = id.clone() => "`{client}` is not a client we know."),
        ));
    }
    if let Some(c) = s.config().client_key(&id) {
        return Ok(Json(tw_api::ClientKey {
            name: c.name.clone(),
            key: c.key.clone(),
            created: false,
        }));
    }
    // 一把都还没有时不替用户建第一把：默认那把是首次启动生成的，没有它说明配置
    // 是手写的，那时该由人决定
    if s.config().default_client().is_none() {
        return Err(fail(
            StatusCode::CONFLICT,
            msg!(
                "control.no_keys" =>
                "config.yaml has no gateway key yet. Create one before pointing a client at the \
                 gateway."
            ),
        ));
    }
    let name = free_name(&s.config(), &id);
    let key = tw_config::generate_key();
    let item = tw_config::Client {
        name: name.clone(),
        key: key.clone(),
        client: Some(id.clone()),
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
    Ok(Json(tw_api::ClientKey {
        name,
        key,
        created: true,
    }))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_words_that_look_like_a_client_id_become_key_owners() {
        for ok in ["claude-code", "gemini-cli", "zed", "a1"] {
            assert!(looks_like_client(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            "Claude Code",
            "../etc",
            "a b",
            "名字",
            &"x".repeat(65),
        ] {
            assert!(!looks_like_client(bad), "{bad}");
        }
    }
}
