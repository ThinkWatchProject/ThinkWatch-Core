//! 列模型、查单个模型。

use axum::extract::{OriginalUri, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::error::GatewayError;
use crate::state::AppState;
use tw_types::msg;

/// 列模型、查单个模型时，客户端是哪一种。
///
/// 这两个请求没有体，路径也分不出 Anthropic 和 OpenAI（都是 `/v1/models`），
/// 只能看请求头：
///
/// - `/v1beta` 路径、或者把密钥放在 Google 位置上的是 Gemini
/// - 带 `anthropic-version`、或者把密钥放在 `x-api-key` 的是 Anthropic ——
///   两个信号**任一个**就算：Claude Code 用 `ANTHROPIC_AUTH_TOKEN` 时密钥走
///   Bearer，但版本头照带；手写的脚本常常只放 `x-api-key`
/// - 其余按 OpenAI 算
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListingShape {
    Anthropic,
    Openai,
    Gemini,
}

impl ListingShape {
    fn of(path: &str, headers: &HeaderMap, position: crate::auth::KeyPosition) -> Self {
        if path.starts_with("/v1beta") || position == crate::auth::KeyPosition::GoogleHeader {
            ListingShape::Gemini
        } else if headers.contains_key("anthropic-version")
            || position == crate::auth::KeyPosition::AnthropicHeader
        {
            ListingShape::Anthropic
        } else {
            ListingShape::Openai
        }
    }

    /// 这种客户端能用哪些协议的上游的模型。和请求那条路用同一张表：列出来的是
    /// 用来生成回答的模型，四种格式互相转换，所以谁都能用
    fn protocols(&self) -> Vec<&'static str> {
        use crate::client_api::{ClientApi, slugs};
        let api = match self {
            ListingShape::Anthropic => ClientApi::AnthropicMessages,
            ListingShape::Openai => ClientApi::OpenaiChat,
            ListingShape::Gemini => ClientApi::Gemini,
        };
        slugs(api.servable_by(true))
    }

    /// 这种客户端要的一个模型对象。**列表和单点查询都从这里出** —— 同一个模型在
    /// 两处必须是同一个对象。
    fn object(&self, id: &str, meta: &ModelMeta) -> serde_json::Value {
        match self {
            ListingShape::Gemini => gemini_model(id, meta),
            ListingShape::Anthropic => anthropic_model(id, meta),
            ListingShape::Openai => openai_model(id, meta),
        }
    }
}

/// 列表里一个模型带给客户端的元数据。
///
/// 这一家手写的（`model_specs`）优先，没写的来自价目表（见 [`tw_config::model_specs`]），
/// 和上游页模型一格的「上下文」（`ModelRow.context_window`）是同一个数。
/// **查不到就是 `None`，对应的字段整个不出现** —— 客户端读不到会用自己的默认值，
/// 一个编出来的数它却会照着截断对话。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ModelMeta {
    /// 一次最多输入多少 token，也就是上下文窗口
    pub max_input_tokens: Option<u64>,
    /// 一次最多输出多少 token。只有 Gemini 的模型对象有这一项
    pub max_output_tokens: Option<u64>,
}

/// 查一个模型的元数据。
///
/// `provider`：知道这个名称发给哪家上游时给上，先看这一家手写的，再按它选的价目表查，
/// 和上游页那一格是同一个查法（[`tw_config::Config::model_limits`]）；不给就只查默认
/// 价目表。自定义价目表只改单价，上下文窗口照样取自默认价目表。
pub(crate) fn model_meta(
    cfg: &tw_config::Config,
    book: &tw_pricing::PriceBook,
    provider: Option<&str>,
    model: &str,
) -> ModelMeta {
    let limits = match provider {
        Some(p) => cfg.model_limits(book, p, model),
        None => tw_config::ModelLimits::priced(book, model),
    };
    ModelMeta {
        max_input_tokens: limits.context_window(),
        max_output_tokens: limits.max_output_tokens(),
    }
}

/// 列表里一个名称的元数据。
///
/// 别名用它第一个有上游提供的模型的（按列表顺序），按提供它的头一家查（见
/// [`tw_engine::Catalog::first_served`]）；真模型也按提供它的头一家（配置里的顺序）查
/// —— 那一家手写的规格才对得上。目录空着、谁都不提供时（这时单点查询不拦），别名按
/// 它列表里的头一个、真模型按它自己查默认价目表。
fn listed_meta(
    book: &tw_pricing::PriceBook,
    catalog: &tw_engine::Catalog,
    cfg: &tw_config::Config,
    name: &str,
) -> ModelMeta {
    if let Some((provider, model)) = catalog.first_served(name) {
        return model_meta(cfg, book, Some(provider), model);
    }
    if !cfg.aliases.contains(name)
        && let Some(provider) = catalog.providers_for(name).first()
    {
        return model_meta(cfg, book, Some(provider), name);
    }
    match cfg.aliases.find(name).and_then(|a| a.models.first()) {
        Some(model) => model_meta(cfg, book, None, model),
        None => model_meta(cfg, book, None, name),
    }
}

/// `GET /v1/models`。
///
/// 三种方言的响应结构不同，但**列表内容来自同一个函数** —— 差别只在
/// 外壳。
pub(super) async fn list_models(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let rt = state.runtime();
    if !rt.allow.allows(peer.ip()) {
        return Err(GatewayError::auth(msg!(
            "gw.auth.source_not_allowed", peer = peer.ip() =>
            "{peer} is not among the allowed source addresses."
        )));
    }
    let (client, position) = state.identify(&headers, query.as_deref())?;
    let allow = rt
        .config
        .clients
        .iter()
        .find(|c| c.name == client)
        .and_then(|c| c.allow.clone());
    let shape = ListingShape::of(uri.path(), &headers, position);
    let catalog = state.catalog.load();
    let models = catalog.resolve_allowed(Some(&shape.protocols()), allow.as_deref());

    let book = state.pricing.load();
    let objects: Vec<_> = models
        .iter()
        .map(|m| shape.object(m, &listed_meta(&book, &catalog, &rt.config, m)))
        .collect();

    let body = match shape {
        ListingShape::Gemini => serde_json::json!({ "models": objects }),
        ListingShape::Anthropic => serde_json::json!({
            "object": "list",
            "data": objects,
            "has_more": false,
            "first_id": models.first(),
            "last_id": models.last(),
        }),
        ListingShape::Openai => serde_json::json!({
            "object": "list",
            "data": objects,
        }),
    };
    Ok(axum::Json(body).into_response())
}

/// 模型的发布时间（Unix 秒）：网关不知道，填纪元零点。
///
/// Anthropic 的 `created_at` 和 OpenAI 的 `created` 说的都是模型本身什么时候发布，
/// Anthropic 的文档写明不知道时就填纪元时间。**不能填请求的时刻** —— 那样同一个
/// 模型每秒换一个发布时间，列表和单点查询跨过整秒就对不上。
const RELEASED_AT: i64 = 0;

/// OpenAI 格式的一个模型对象。
///
/// 知道上下文窗口时，同一个数写成三个字段：各家客户端读的不是同一个名字 ——
/// Grok Build 读 `context_window`，oh-my-pi 读 `context_length`，Hermes 三个依次试。
fn openai_model(id: &str, meta: &ModelMeta) -> serde_json::Value {
    let mut m = serde_json::json!({ "id": id, "object": "model", "created": RELEASED_AT });
    if let Some(n) = meta.max_input_tokens {
        m["context_window"] = n.into();
        m["context_length"] = n.into();
        m["max_input_tokens"] = n.into();
    }
    m
}

/// Anthropic 格式的一个模型对象。
///
/// **是 OpenAI 那个对象的超集**：Anthropic 的字段（`type`、`display_name`、
/// `created_at`）之外，`object`、`created` 和上下文窗口那几个字段照样在。只放
/// `x-api-key` 的客户端也被认成 Anthropic，其中有按 OpenAI 的形状读列表的，不能让
/// 它们读不出来。
///
/// Claude Desktop 读其中的 `max_input_tokens`，另外读 `supports_1m`：上下文窗口到
/// 1,000,000 时是 `true`。上下文窗口不知道时这两项都不给。
///
/// Claude 的模型再带上 `anthropic_family_tier`：Claude Desktop 按它把模型归档（见
/// [`family_tier`]），配置里写的 `sonnet` 这样的简称靠它解析。**只看模型名**，
/// 名字里看不出是 Claude 的一律不标 —— 把别家的模型标成 Claude 是在替客户端撒谎。
fn anthropic_model(id: &str, meta: &ModelMeta) -> serde_json::Value {
    let created_at = chrono::DateTime::from_timestamp(RELEASED_AT, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut m = openai_model(id, meta);
    m["type"] = "model".into();
    m["display_name"] = display_name(id).unwrap_or_else(|| id.to_string()).into();
    m["created_at"] = created_at.into();
    if let Some(n) = meta.max_input_tokens {
        m["supports_1m"] = (n >= 1_000_000).into();
    }
    if let Some(tier) = family_tier(id) {
        m["anthropic_family_tier"] = tier.into();
    }
    m
}

/// Gemini 格式的一个模型对象。上下文窗口和输出上限用 Gemini 自己的字段名。
fn gemini_model(id: &str, meta: &ModelMeta) -> serde_json::Value {
    let mut m = serde_json::json!({ "name": format!("models/{id}") });
    if let Some(n) = meta.max_input_tokens {
        m["inputTokenLimit"] = n.into();
    }
    if let Some(n) = meta.max_output_tokens {
        m["outputTokenLimit"] = n.into();
    }
    m
}

/// Claude 模型的名字：`claude-sonnet-4-5-20250929` → `Claude Sonnet 4.5`。
///
/// 只认最后一段（`/` 之后）以 `claude-` 开头、每一节都是字母或数字的；日期那一节
/// 去掉，相邻的数字用点连起来。**认不出就是 `None`**，调用方用模型 ID 本身 ——
/// 客户端看到和 ID 一样的名字时会自己想办法，一个猜错的名字它却会照着显示。
fn display_name(id: &str) -> Option<String> {
    let last = id.rsplit('/').next()?;
    let rest = last.strip_prefix("claude-")?;
    let mut words: Vec<String> = vec!["Claude".into()];
    let mut number = false;
    for part in rest.split('-') {
        if part.is_empty() {
            return None;
        }
        if part.len() == 8 && part.bytes().all(|b| b.is_ascii_digit()) {
            // 发布日期，不是名字的一部分
            continue;
        }
        if part.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            match words.last_mut() {
                Some(w) if number => {
                    w.push('.');
                    w.push_str(part);
                }
                _ => words.push(part.to_string()),
            }
            number = true;
        } else if part.bytes().all(|b| b.is_ascii_alphanumeric()) {
            let mut c = part.chars();
            let first = c.next()?.to_ascii_uppercase();
            words.push(std::iter::once(first).chain(c).collect());
            number = false;
        } else {
            return None;
        }
    }
    (words.len() > 1).then(|| words.join(" "))
}

/// 名字里看得出是哪一档的 Claude 模型：`opus`、`sonnet`、`haiku`、`fable` 或
/// `mythos`（Claude Desktop 认的就是这五档）。
fn family_tier(id: &str) -> Option<&'static str> {
    let lower = id.to_ascii_lowercase();
    if !lower.contains("claude") && !lower.contains("anthropic") {
        return None;
    }
    let mut tiers = ["opus", "sonnet", "haiku", "fable", "mythos"]
        .into_iter()
        .filter(|t| lower.contains(t));
    // 名字里同时出现两档的（一个路由别名）不猜
    match (tiers.next(), tiers.next()) {
        (Some(t), None) => Some(t),
        _ => None,
    }
}

/// `GET /v1/models/:model`。
///
/// **不许可就当它不存在（404），不是 403。**回 403 等于告诉对方
/// 「这个模型在，只是你不能用」—— 而列表里根本没列它，两处说法不一致
/// 本身就是一条信息。
pub(super) async fn get_model(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(model): axum::extract::Path<String>,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, GatewayError> {
    let rt = state.runtime();
    if !rt.allow.allows(peer.ip()) {
        return Err(GatewayError::auth(msg!(
            "gw.auth.source_not_allowed", peer = peer.ip() =>
            "{peer} is not among the allowed source addresses."
        )));
    }
    let (client, position) = state.identify(&headers, query.as_deref())?;
    let allow = rt
        .config
        .clients
        .iter()
        .find(|c| c.name == client)
        .and_then(|c| c.allow.clone());
    let catalog = state.catalog.load();
    // 目录空着时不拦 —— 那说明探测还没回来或者上游都不给列表，这时候
    // 拦等于把整个网关关掉（和请求那条路同一个判断）
    let shape = ListingShape::of(uri.path(), &headers, position);
    if !catalog.is_empty() && !catalog.admits(&model, Some(&shape.protocols()), allow.as_deref()) {
        return Err(GatewayError::new(
            crate::error::Source::Request,
            msg!(
                "gw.model.unknown", model = model.clone() =>
                "There is no model {model}. GET /v1/models lists the models that are available."
            ),
        ));
    }
    let meta = listed_meta(&state.pricing.load(), &catalog, &rt.config, &model);
    Ok(axum::Json(shape.object(&model, &meta)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_model_ids_get_their_names() {
        for (id, name) in [
            ("claude-sonnet-4-5", "Claude Sonnet 4.5"),
            ("claude-sonnet-4-5-20250929", "Claude Sonnet 4.5"),
            ("claude-opus-4-1-20250805", "Claude Opus 4.1"),
            ("claude-3-5-haiku-20241022", "Claude 3.5 Haiku"),
            ("claude-fable-5-1", "Claude Fable 5.1"),
            ("anthropic/claude-sonnet-4.5", "Claude Sonnet 4.5"),
        ] {
            assert_eq!(display_name(id).as_deref(), Some(name), "{id}");
        }
    }

    #[test]
    fn a_name_that_cannot_be_read_is_not_guessed() {
        for id in [
            "deepseek-chat",
            "gpt-5",
            "claude-",
            "claude--x",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        ] {
            assert_eq!(display_name(id), None, "{id}");
        }
    }

    #[test]
    fn only_claude_models_are_given_a_family_tier() {
        assert_eq!(family_tier("claude-opus-4-1"), Some("opus"));
        assert_eq!(family_tier("claude-3-5-haiku-20241022"), Some("haiku"));
        assert_eq!(
            family_tier("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            Some("sonnet")
        );
        // 名字里有 sonnet，但看不出是 Claude
        assert_eq!(family_tier("my-sonnet-alias"), None);
        assert_eq!(family_tier("claude-opus-or-sonnet"), None);
    }

    #[test]
    fn fable_and_mythos_are_tiers_too() {
        assert_eq!(family_tier("claude-fable-5-1"), Some("fable"));
        assert_eq!(
            family_tier("global.anthropic.claude-fable-5"),
            Some("fable")
        );
        assert_eq!(family_tier("claude-mythos-preview"), Some("mythos"));
        assert_eq!(
            family_tier("anthropic.claude-mythos-preview"),
            Some("mythos")
        );
        assert_eq!(family_tier("claude-fable-or-opus"), None);
        assert_eq!(family_tier("fable-mini"), None);
    }

    const KNOWN: ModelMeta = ModelMeta {
        max_input_tokens: Some(200_000),
        max_output_tokens: Some(64_000),
    };

    #[test]
    fn an_openai_model_carries_its_context_window_under_each_name_clients_read() {
        let m = openai_model("claude-sonnet-4-5", &KNOWN);
        assert_eq!(m["id"], "claude-sonnet-4-5");
        assert_eq!(m["context_window"], 200_000);
        assert_eq!(m["context_length"], 200_000);
        assert_eq!(m["max_input_tokens"], 200_000);
        // 输出上限 OpenAI 的模型对象里没有人读
        assert!(m.get("max_output_tokens").is_none(), "{m}");
    }

    #[test]
    fn an_anthropic_model_says_whether_it_takes_a_million_tokens() {
        let m = anthropic_model("claude-sonnet-4-5", &KNOWN);
        assert_eq!(m["max_input_tokens"], 200_000);
        assert_eq!(m["supports_1m"], false);
        assert_eq!(m["anthropic_family_tier"], "sonnet");
        // 仍是 OpenAI 那个对象的超集
        let openai = openai_model("claude-sonnet-4-5", &KNOWN);
        for (k, v) in openai.as_object().unwrap() {
            assert_eq!(&m[k], v, "{k}");
        }

        for n in [1_000_000, 1_048_576] {
            let big = ModelMeta {
                max_input_tokens: Some(n),
                ..KNOWN
            };
            let m = anthropic_model("claude-fable-5", &big);
            assert_eq!(m["supports_1m"], true, "{n}");
            assert_eq!(m["max_input_tokens"], n);
            assert_eq!(m["anthropic_family_tier"], "fable");
        }
    }

    #[test]
    fn a_gemini_model_uses_geminis_field_names() {
        let m = gemini_model("gemini-2.5-pro", &KNOWN);
        assert_eq!(m["name"], "models/gemini-2.5-pro");
        assert_eq!(m["inputTokenLimit"], 200_000);
        assert_eq!(m["outputTokenLimit"], 64_000);
    }

    #[test]
    fn an_unknown_context_window_leaves_the_fields_out() {
        let unknown = ModelMeta::default();
        let fields = [
            "context_window",
            "context_length",
            "max_input_tokens",
            "supports_1m",
            "inputTokenLimit",
            "outputTokenLimit",
        ];
        for m in [
            openai_model("my-model", &unknown),
            anthropic_model("my-model", &unknown),
            gemini_model("my-model", &unknown),
        ] {
            for f in fields {
                assert!(m.get(f).is_none(), "{f}: {m}");
            }
        }
    }

    #[test]
    fn metadata_comes_from_the_price_table() {
        let book = tw_pricing::PriceBook::builtin().unwrap();
        let cfg = tw_config::Config::default();
        let sonnet = model_meta(&cfg, &book, None, "claude-sonnet-4-5-20250929");
        assert_eq!(sonnet.max_input_tokens, Some(200_000));
        assert_eq!(sonnet.max_output_tokens, Some(64_000));
        // 给了上游就按它选的价目表查；没选价目表的上游和默认价目表一样
        assert_eq!(
            model_meta(&cfg, &book, Some("up"), "claude-sonnet-4-5"),
            sonnet
        );
        // Bedrock 的名字也查得到
        assert_eq!(
            model_meta(&cfg, &book, Some("bedrock"), "us.anthropic.claude-fable-5")
                .max_input_tokens,
            Some(1_000_000)
        );
        assert_eq!(
            model_meta(&cfg, &book, None, "no-such-model-anywhere"),
            ModelMeta::default()
        );
    }

    #[test]
    fn a_spec_written_for_the_upstream_wins_over_the_price_table() {
        let book = tw_pricing::PriceBook::builtin().unwrap();
        let cfg = tw_config::Config {
            providers: vec![tw_config::Provider {
                name: "up".into(),
                base_url: "https://relay.example.com".into(),
                model_specs: [(
                    "claude-sonnet-4-5".to_string(),
                    tw_config::ModelSpec {
                        context_window: Some(1_000_000),
                        max_output_tokens: None,
                    },
                )]
                .into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let m = model_meta(&cfg, &book, Some("up"), "claude-sonnet-4-5");
        assert_eq!(m.max_input_tokens, Some(1_000_000));
        assert_eq!(m.max_output_tokens, Some(64_000), "没写的照样取价目表");
        // 不知道是哪一家时没有手写的可看
        assert_eq!(
            model_meta(&cfg, &book, None, "claude-sonnet-4-5").max_input_tokens,
            Some(200_000)
        );
    }
}
