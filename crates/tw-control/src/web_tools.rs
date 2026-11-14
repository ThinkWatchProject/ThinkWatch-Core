use axum::{Json, Router, extract::State, http::StatusCode};
use tw_api::ep;

use crate::{ControlState, Fail, apply_fail, contract::RouterExt, fail};

pub fn router() -> Router<ControlState> {
    Router::new()
        .at(ep::WebTools, view)
        .at(ep::SaveWebTools, save)
        .at(ep::TestWebTool, test)
}

async fn view(State(s): State<ControlState>) -> Json<tw_api::WebToolsView> {
    Json(tw_api::WebToolsView {
        config: s.config().web_tools.clone(),
        config_version: s.cfg.current().map(|c| c.version()).unwrap_or_default(),
    })
}

async fn save(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::WebToolsSave>,
) -> Result<Json<tw_api::ConfigWritten>, Fail> {
    let value = serde_yaml_ng::to_value(&req.config).expect("web config serializes");
    let version = s
        .cfg
        .transform(
            Some(&req.base_version),
            tw_config::history::Origin::Ui,
            |text, _| {
                Ok(tw_config::edit::set(
                    text,
                    &[tw_yaml::Step::key("web_tools")],
                    Some(&value),
                )?)
            },
        )
        .await
        .map_err(apply_fail)?;
    Ok(Json(tw_api::ConfigWritten { version }))
}

async fn test(
    State(s): State<ControlState>,
    Json(req): Json<tw_api::WebToolRequest>,
) -> Result<Json<tw_api::WebToolResult>, Fail> {
    let rt = s.gateway.runtime();
    tw_gateway::web_tools::execute(&rt, "control-plane", &req)
        .await
        .map(Json)
        .map_err(|m| fail(StatusCode::BAD_GATEWAY, m))
}
