//! 模型目录：对着一个假的控制面，走真的请求、真的翻页、真的签名。

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use common::{Fake, SK, Seen, keys, signature_of};
use serde_json::{Value, json};
use tw_bedrock::catalog::{
    Auth, Failure, MAX_PROFILE_PAGES, Profiles, accepts_credential, list_models,
};

/// 每个查询值必须编码的字符都有
const TOKEN: &str = "page+2/of=2==";

fn api_key_headers() -> Vec<(String, String)> {
    vec![("Authorization".into(), "Bearer ABSK-test".into())]
}

fn with_api_key(headers: &[(String, String)]) -> Auth<'_> {
    Auth {
        headers,
        credentials: None,
        region: "us-east-1",
    }
}

type Answer = (StatusCode, Vec<(&'static str, String)>, Value);

fn ok(v: Value) -> Answer {
    (StatusCode::OK, vec![], v)
}

/// 一个正常的控制面：`models` 是基础模型；预设的推理配置分两页，`first` 在第一页，
/// `second` 在 [`TOKEN`] 后面；应用推理配置 `apps` 一页（按 ARN）。
fn control_plane(
    models: &'static [&'static str],
    first: &'static [&'static str],
    second: &'static [&'static str],
    apps: &'static [&'static str],
) -> impl Fn(&Seen) -> Answer + Send + Sync + 'static {
    move |req| {
        let profiles = |ids: &[&str], kind: &str| -> Vec<Value> {
            ids.iter()
                .map(|id| match kind {
                    "APPLICATION" => json!({
                        "inferenceProfileId": id.rsplit('/').next().unwrap(),
                        "inferenceProfileArn": id,
                        "type": kind,
                    }),
                    _ => json!({"inferenceProfileId": id, "type": kind}),
                })
                .collect()
        };
        match req.path.as_str() {
            "/foundation-models" => {
                assert_eq!(req.param("byInferenceType").as_deref(), Some("ON_DEMAND"));
                assert_eq!(req.param("byOutputModality").as_deref(), Some("TEXT"));
                ok(json!({
                    "modelSummaries": models.iter().map(|id| json!({"modelId": id})).collect::<Vec<_>>(),
                }))
            }
            "/inference-profiles" => match (req.param("type").as_deref(), req.param("nextToken")) {
                (Some("SYSTEM_DEFINED"), None) => ok(json!({
                    "inferenceProfileSummaries": profiles(first, "SYSTEM_DEFINED"),
                    "nextToken": TOKEN,
                })),
                (Some("SYSTEM_DEFINED"), Some(t)) if t == TOKEN => ok(json!({
                    "inferenceProfileSummaries": profiles(second, "SYSTEM_DEFINED"),
                })),
                (Some("APPLICATION"), None) => ok(json!({
                    "inferenceProfileSummaries": profiles(apps, "APPLICATION"),
                })),
                other => panic!("unexpected listing {other:?}"),
            },
            other => panic!("unexpected path {other}"),
        }
    }
}

#[tokio::test]
async fn the_catalog_is_foundation_models_and_every_page_of_inference_profiles() {
    let fake = Fake::start(control_plane(
        &["meta.llama3-8b-instruct-v1:0", "amazon.nova-lite-v1:0"],
        &["us.anthropic.claude-sonnet-4-5-20250929-v1:0"],
        &[
            "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "us.amazon.nova-lite-v1:0",
        ],
        &[],
    ))
    .await;
    let headers = api_key_headers();

    let models = list_models(
        &reqwest::Client::new(),
        &fake.uri(),
        &with_api_key(&headers),
        Profiles::SystemDefined,
    )
    .await
    .unwrap();

    assert_eq!(
        models,
        [
            "amazon.nova-lite-v1:0",
            "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "meta.llama3-8b-instruct-v1:0",
            "us.amazon.nova-lite-v1:0",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        ]
    );
    // 只要预设的，就不去问应用推理配置
    assert!(
        fake.seen()
            .iter()
            .all(|r| r.param("type").as_deref() != Some("APPLICATION"))
    );
}

#[tokio::test]
async fn application_inference_profiles_are_listed_by_the_arn_they_are_invoked_by() {
    const ARN: &str =
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/a1b2c3d4e5f6";
    let fake = Fake::start(control_plane(
        &["amazon.nova-lite-v1:0"],
        &["us.anthropic.claude-sonnet-4-5-20250929-v1:0"],
        &[],
        &[ARN],
    ))
    .await;
    let headers = api_key_headers();

    let models = list_models(
        &reqwest::Client::new(),
        &fake.uri(),
        &with_api_key(&headers),
        Profiles::WithApplication,
    )
    .await
    .unwrap();

    assert_eq!(
        models,
        [
            "amazon.nova-lite-v1:0",
            ARN,
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        ]
    );
}

#[tokio::test]
async fn an_api_key_goes_out_as_it_is_and_nothing_is_signed() {
    let fake = Fake::start(control_plane(&["amazon.nova-lite-v1:0"], &[], &[], &[])).await;
    let headers = api_key_headers();

    list_models(
        &reqwest::Client::new(),
        &fake.uri(),
        &with_api_key(&headers),
        Profiles::SystemDefined,
    )
    .await
    .unwrap();

    let seen = fake.seen();
    assert_eq!(seen.len(), 3, "one listing and two pages");
    for req in &seen {
        assert_eq!(
            req.all("authorization"),
            ["Bearer ABSK-test"],
            "{}",
            req.path
        );
        assert!(req.header("x-amz-date").is_none(), "{}", req.path);
    }
}

#[tokio::test]
async fn access_keys_sign_every_listing_request_as_aws_checks_it() {
    let fake = Fake::start(control_plane(
        &["amazon.nova-lite-v1:0"],
        &["us.amazon.nova-lite-v1:0"],
        &[],
        &[],
    ))
    .await;
    let headers = vec![("x-custom".to_string(), "1".to_string())];
    let credentials = keys();
    let auth = Auth {
        headers: &headers,
        credentials: Some(&credentials),
        region: "us-east-1",
    };

    list_models(
        &reqwest::Client::new(),
        &fake.uri(),
        &auth,
        Profiles::SystemDefined,
    )
    .await
    .unwrap();

    let seen = fake.seen();
    assert_eq!(seen.len(), 3, "one listing and two pages");
    assert!(
        seen.iter()
            .any(|r| r.param("nextToken").as_deref() == Some(TOKEN)),
        "the second page must have been asked for"
    );
    for req in &seen {
        assert_eq!(req.header("x-custom"), Some("1"));
        let (claimed, expected) = signature_of(req, SK, "us-east-1");
        assert_eq!(claimed, expected, "{} ?{}", req.path, req.query);
    }
}

#[tokio::test]
async fn without_a_key_or_credentials_nothing_is_sent() {
    let fake = Fake::start(control_plane(&[], &[], &[], &[])).await;
    let failure = list_models(
        &reqwest::Client::new(),
        &fake.uri(),
        &Auth {
            headers: &[],
            credentials: None,
            region: "us-east-1",
        },
        Profiles::SystemDefined,
    )
    .await
    .unwrap_err();
    assert!(matches!(failure, Failure::Request(_)), "{failure:?}");
    assert!(fake.seen().is_empty());
}

#[tokio::test]
async fn a_refused_listing_fails_the_whole_catalog_and_names_the_exception() {
    // 只有基础 id 的清单比没有还糟：大多数现在的模型不按它们提供
    let fake = Fake::start(|req: &Seen| match req.path.as_str() {
        "/foundation-models" => ok(json!({"modelSummaries": [{"modelId": "anthropic.claude-sonnet-4-5-20250929-v1:0"}]})),
        _ => (
            StatusCode::FORBIDDEN,
            vec![(
                "x-amzn-errortype",
                "AccessDeniedException:http://internal.amazon.com/coral/com.amazon.bedrock/".into(),
            )],
            json!({"message": "User: arn:aws:iam::123456789012:user/x is not authorized to perform: bedrock:ListInferenceProfiles"}),
        ),
    })
    .await;
    let headers = api_key_headers();

    let failure = list_models(
        &reqwest::Client::new(),
        &fake.uri(),
        &with_api_key(&headers),
        Profiles::SystemDefined,
    )
    .await
    .unwrap_err();

    assert_eq!(
        failure,
        Failure::Status {
            status: 403,
            kind: Some("AccessDeniedException".into()),
            message: "User: arn:aws:iam::123456789012:user/x is not authorized to perform: bedrock:ListInferenceProfiles".into(),
        }
    );
}

#[tokio::test]
async fn a_credential_is_good_when_the_control_plane_serves_it() {
    let fake = Fake::start(|req: &Seen| {
        assert_eq!(req.path, "/inference-profiles");
        assert_eq!(req.param("maxResults").as_deref(), Some("1"));
        ok(json!({"inferenceProfileSummaries": []}))
    })
    .await;
    let headers = api_key_headers();
    assert!(
        accepts_credential(
            &reqwest::Client::new(),
            &fake.uri(),
            &with_api_key(&headers)
        )
        .await
    );
}

#[tokio::test]
async fn a_credential_the_control_plane_refuses_is_not_good() {
    let fake = Fake::start(|_: &Seen| {
        (
            StatusCode::FORBIDDEN,
            vec![],
            json!({"Message": "Authentication failed: Please make sure your API Key is valid."}),
        )
    })
    .await;
    let headers = api_key_headers();
    assert!(
        !accepts_credential(
            &reqwest::Client::new(),
            &fake.uri(),
            &with_api_key(&headers)
        )
        .await
    );
}

#[tokio::test]
async fn a_listing_that_never_ends_is_given_up_on() {
    let pages = Arc::new(AtomicUsize::new(0));
    let counted = pages.clone();
    let fake = Fake::start(move |req: &Seen| match req.path.as_str() {
        "/foundation-models" => {
            ok(json!({"modelSummaries": [{"modelId": "amazon.nova-lite-v1:0"}]}))
        }
        _ => {
            counted.fetch_add(1, Ordering::SeqCst);
            ok(json!({"inferenceProfileSummaries": [], "nextToken": "again"}))
        }
    })
    .await;
    let headers = api_key_headers();

    let failure = list_models(
        &reqwest::Client::new(),
        &fake.uri(),
        &with_api_key(&headers),
        Profiles::SystemDefined,
    )
    .await
    .unwrap_err();

    assert!(matches!(failure, Failure::Request(_)), "{failure:?}");
    assert_eq!(pages.load(Ordering::SeqCst), MAX_PROFILE_PAGES);
}
