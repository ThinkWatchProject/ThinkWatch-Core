//! 一家 Bedrock 上游能路由到哪些模型。
//!
//! Bedrock 列出的基础模型不是这份清单本身。它列的是基础模型 id，而现在的大多数
//! 模型 —— Claude 3.7 以后的都是 —— 根本不按基础 id 提供：要经推理配置（inference
//! profile）调用，比如 `us.anthropic.claude-sonnet-4-5-20250929-v1:0`，它把请求
//! 分摊到几个区域。所以清单是区域控制面（`bedrock.{区域}.amazonaws.com`）上几份
//! 列表的并集：
//!
//! - 能按需调用、输出文本的基础模型（`ListFoundationModels`）；
//! - AWS 预设的推理配置（`ListInferenceProfiles`，`SYSTEM_DEFINED`）；
//! - 要的话，还有账号自己建的应用推理配置（`APPLICATION`），按 ARN 调用。
//!
//! 这些列表都不说**这个账号**能不能调某个模型、走哪个接口 —— 那要调的时候才知道。
//!
//! 请求怎么认证和转发时一样：上游自己的请求头里有 Bedrock API Key 就原样带上，
//! 否则每个请求都签名（见 [`crate::carries_api_key`]）。

use std::collections::BTreeSet;
use std::time::SystemTime;

use serde_json::Value;

use crate::Credentials;

/// 一份推理配置列表最多读几页。一页最多 1000 个，一个区域也就几百个，
/// 只有永远翻不完的列表才会走到这里。
pub const MAX_PROFILE_PAGES: usize = 10;

/// 列哪几种推理配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profiles {
    /// 只列 AWS 预设的
    SystemDefined,
    /// 预设的，加上账号自己建的应用推理配置（它们按 ARN 调用，列出来的就是 ARN）
    WithApplication,
}

/// 怎么认证列模型的请求。
#[derive(Debug, Clone, Copy)]
pub struct Auth<'a> {
    /// 上游自己的请求头，每个请求都带上。里面有 `Authorization`（Bedrock API Key）
    /// 就不签名
    pub headers: &'a [(String, String)],
    /// 签名用的凭证。请求头里没有 API Key 时必须有
    pub credentials: Option<&'a Credentials>,
    /// 签名用的区域
    pub region: &'a str,
}

/// 清单为什么没列出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// AWS 回答了，拒绝了。`kind` 是异常名，比如 `AccessDeniedException`、
    /// `UnrecognizedClientException` —— 凭证不对和凭证对但没有列模型的权限，
    /// 靠它分开。`message` 是 AWS 的原话，**可能点名账号**，见 [`crate::error`]
    Status {
        status: u16,
        kind: Option<String>,
        message: String,
    },
    /// 没有可读的：请求签不了、发不出去，或者回来的不是一份列表
    Request(String),
}

/// `auth` 能在 `control_base` 上路由到的每一个模型 id，排好序。
///
/// `control_base` 是区域的控制面（[`crate::endpoint::control_base`]），或者一个
/// 替它回答的代理。每份列表都得成功：只有基础 id 的清单，给出的恰恰是大多数现在的
/// 模型**不能**用的那些 id，还不如没有。
pub async fn list_models(
    http: &reqwest::Client,
    control_base: &str,
    auth: &Auth<'_>,
    profiles: Profiles,
) -> Result<Vec<String>, Failure> {
    let application = async {
        match profiles {
            Profiles::SystemDefined => Ok(Vec::new()),
            Profiles::WithApplication => {
                inference_profiles(http, control_base, auth, ProfileType::Application).await
            }
        }
    };
    let (models, system, application) = futures::try_join!(
        foundation_models(http, control_base, auth),
        inference_profiles(http, control_base, auth, ProfileType::SystemDefined),
        application,
    )?;
    let ids: BTreeSet<String> = models
        .into_iter()
        .chain(system)
        .chain(application)
        .collect();
    Ok(ids.into_iter().collect())
}

/// 控制面认不认 `auth` 的凭证。只列一个推理配置：凭证被拒会在这里露出来，某一个
/// 模型被拒不会。
pub async fn accepts_credential(
    http: &reqwest::Client,
    control_base: &str,
    auth: &Auth<'_>,
) -> bool {
    let Ok(url) = listing_url(
        control_base,
        "inference-profiles",
        &[("type", "SYSTEM_DEFINED"), ("maxResults", "1")],
    ) else {
        return false;
    };
    get(http, url, auth).await.is_ok()
}

async fn foundation_models(
    http: &reqwest::Client,
    control_base: &str,
    auth: &Auth<'_>,
) -> Result<Vec<String>, Failure> {
    let url = listing_url(
        control_base,
        "foundation-models",
        &[
            ("byInferenceType", "ON_DEMAND"),
            ("byOutputModality", "TEXT"),
        ],
    )?;
    let listing = get(http, url, auth).await?;
    ids(&listing, "modelSummaries", "modelId")
}

#[derive(Debug, Clone, Copy)]
enum ProfileType {
    SystemDefined,
    Application,
}

impl ProfileType {
    /// 查询参数 `type`（接口文档里叫 `typeEquals`）的值
    fn query(self) -> &'static str {
        match self {
            ProfileType::SystemDefined => "SYSTEM_DEFINED",
            ProfileType::Application => "APPLICATION",
        }
    }

    /// 调用它时写进模型 id 的那个字段：预设的按 id（`us.anthropic.…`），
    /// 应用推理配置按 ARN
    fn id_field(self) -> &'static str {
        match self {
            ProfileType::SystemDefined => "inferenceProfileId",
            ProfileType::Application => "inferenceProfileArn",
        }
    }
}

async fn inference_profiles(
    http: &reqwest::Client,
    control_base: &str,
    auth: &Auth<'_>,
    kind: ProfileType,
) -> Result<Vec<String>, Failure> {
    let mut profiles = Vec::new();
    let mut next_token: Option<String> = None;
    for _ in 0..MAX_PROFILE_PAGES {
        let mut query = vec![("type", kind.query()), ("maxResults", "1000")];
        if let Some(token) = &next_token {
            query.push(("nextToken", token));
        }
        let url = listing_url(control_base, "inference-profiles", &query)?;
        let page = get(http, url, auth).await?;
        profiles.extend(ids(&page, "inferenceProfileSummaries", kind.id_field())?);
        next_token = page
            .get("nextToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        if next_token.is_none() {
            return Ok(profiles);
        }
    }
    Err(Failure::Request(format!(
        "the inference profile listing did not end after {MAX_PROFILE_PAGES} pages"
    )))
}

/// `{control_base}/{path}?{query}`，查询串做百分号编码：`nextToken` 里可能有 `+`、
/// `/`、`=`，而签名覆盖的是 AWS 解码之后的查询串。
fn listing_url(
    control_base: &str,
    path: &str,
    query: &[(&str, &str)],
) -> Result<reqwest::Url, Failure> {
    let base = control_base.trim_end_matches('/');
    reqwest::Url::parse_with_params(&format!("{base}/{path}"), query)
        .map_err(|e| Failure::Request(format!("{base}/{path}: {e}")))
}

/// GET 一份列表，按 JSON 读。
async fn get(http: &reqwest::Client, url: reqwest::Url, auth: &Auth<'_>) -> Result<Value, Failure> {
    let mut req = http.get(url.clone());
    for (k, v) in auth.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if !crate::carries_api_key(auth.headers.iter().map(|(k, _)| k.as_str())) {
        let credentials = auth.credentials.ok_or_else(|| {
            Failure::Request("no credential: neither a Bedrock API key nor AWS keys".into())
        })?;
        let signed = crate::sign::sign(
            credentials,
            auth.region,
            "GET",
            url.as_str(),
            None,
            SystemTime::now(),
        )
        .map_err(|e| Failure::Request(e.to_string()))?;
        for (k, v) in signed {
            req = req.header(k, v);
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| Failure::Request(e.to_string()))?;
    let status = resp.status();
    let kind_header = resp
        .headers()
        .get(crate::error::ERROR_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp
        .bytes()
        .await
        .map_err(|e| Failure::Request(e.to_string()))?;
    if status.is_success() {
        return Ok(serde_json::from_slice(&body).unwrap_or(Value::Null));
    }
    Err(Failure::Status {
        status: status.as_u16(),
        kind: crate::error::kind_of(kind_header.as_deref(), &body),
        message: crate::error::message_in(&body)
            .unwrap_or_else(|| status.canonical_reason().unwrap_or("error").to_string()),
    })
}

/// 一份列表里 `list` 数组每一项的 `id`。
///
/// 没有那个数组的「列表」不可信，报错，而不是当成空的读。
fn ids(listing: &Value, list: &str, id: &str) -> Result<Vec<String>, Failure> {
    let entries = listing
        .get(list)
        .and_then(Value::as_array)
        .ok_or_else(|| Failure::Request(format!("the listing has no {list}")))?;
    Ok(entries
        .iter()
        .filter_map(|entry| entry.get(id).and_then(Value::as_str))
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const TOKEN: &str = "page+2/of=2==";

    #[test]
    fn a_page_token_is_percent_encoded_into_the_query() {
        let url = listing_url(
            "https://bedrock.us-east-1.amazonaws.com/",
            "inference-profiles",
            &[("type", "SYSTEM_DEFINED"), ("nextToken", TOKEN)],
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://bedrock.us-east-1.amazonaws.com/inference-profiles\
             ?type=SYSTEM_DEFINED&nextToken=page%2B2%2Fof%3D2%3D%3D"
        );
    }

    #[test]
    fn a_listing_without_its_array_fails_rather_than_reading_as_empty() {
        assert_eq!(
            ids(
                &json!({"modelSummaries": [{"modelId": "a"}, {}]}),
                "modelSummaries",
                "modelId"
            ),
            Ok(vec!["a".to_string()])
        );
        assert!(ids(&json!({"message": "?"}), "modelSummaries", "modelId").is_err());
        assert!(ids(&Value::Null, "modelSummaries", "modelId").is_err());
    }
}
