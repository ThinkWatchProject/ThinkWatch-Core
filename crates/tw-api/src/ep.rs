//! 控制面的全部端点。**加端点从这里开始**：core 按这里注册，客户端按这里拼。
//!
//! 写法见 `endpoints!`（[`crate::endpoint`]）。类型名就是端点名，和同名的
//! 响应类型（比如 [`crate::Status`]）不在一个模块里，所以不冲突。

use crate as api;
use crate::endpoint::endpoints;

endpoints! {
    // ─────────────────────────────────────────────── 进程
    /// 版本、监听地址、在途请求数
    Status: GET "/status", () => api::Status;
    /// 请网关退出（202）
    Shutdown: POST "/shutdown", () => api::Msg;
    /// 本机的网卡，一张一行
    Interfaces: GET "/interfaces", () => Vec<api::NicView>;
    /// 实时事件流
    Events: GET "/events", () => api::Event, events;
    /// 此刻还没结束的请求，每个到目前为止的事件，和 core 的时钟
    InFlight: GET "/in-flight", () => api::InFlight;
    /// 此刻的实时读数：在跑的请求、最近一分钟的生成速率
    Live: GET "/live", () => api::LiveView;
    Overview: GET "/overview", () => api::Overview;
    /// 连通性分段测速
    L1: POST "/l1", api::L1Request => Vec<api::L1Result>;
    Storage: GET "/storage", () => api::StorageStatus;
    Quota: GET "/quota", () => Vec<api::ProviderQuota>;
    /// 诊断包（Markdown，已脱敏）
    Diagnostics: GET "/diagnostics", () => String, text;

    // ─────────────────────────────────────────────── 配置原文与历史
    GetConfig: GET "/config", () => api::ConfigText;
    PatchConfig: PATCH "/config", api::ConfigPatch => api::ConfigWritten;
    PutConfig: PUT "/config", api::ConfigWrite => api::ConfigWritten;
    ConfigHistory: GET "/config/history", () => Vec<api::ConfigVersion>;
    ConfigAt: GET "/config/at", api::ConfigAtQuery => api::ConfigAt;
    ConfigRollback: POST "/config/rollback", api::RollbackRequest => api::ConfigWritten;
    SaveListen: PUT "/listen", api::ListenSave => api::ConfigWritten;

    // ─────────────────────────────────────────────── 用量与记录
    Summary: GET "/summary", api::Window => api::Summary;
    CostBuckets: GET "/summary/buckets", api::BucketQuery => Vec<api::CostBucket>;
    CostBucketsBy: GET "/summary/buckets/by", api::BucketGroupQuery => Vec<api::CostBucketGroup>;
    CostBy: GET "/summary/by", api::GroupQuery => Vec<api::CostGroup>;
    /// 各条路由走了多少请求、各条规则命中了多少，记录从哪一刻起是全的
    RouteStats: GET "/summary/routes", api::Window => api::RouteStats;
    History: GET "/history", api::ListQuery => Vec<api::HistoryRow>;
    Latency: GET "/latency", api::Window => Vec<api::LatencyView>;
    LatencyByProvider: GET "/latency/provider", api::Window => Vec<api::LatencyView>;
    RequestDetail: GET "/request/{id}" [id], () => api::RequestDetail;
    /// 把一条记录变成回放用例（YAML）
    Fixture: GET "/request/{id}/fixture" [id], () => String, text;
    Sessions: GET "/sessions", api::ListQuery => Vec<api::SessionView>;
    SessionDetail: GET "/sessions/{id}" [id], () => api::SessionDetail;

    // ─────────────────────────────────────────────── 测速、回放、试路由
    SpeedQuote: POST "/speed/quote", api::SpeedRunRequest => api::SpeedQuote;
    SpeedRun: POST "/speed/run", api::SpeedRunRequest => Vec<api::SpeedResult>;
    ReplayQuote: POST "/replay/quote", api::ReplayRequest => api::ReplayQuote;
    ReplayRun: POST "/replay/run", api::ReplayRequest => api::ReplayResult;
    DryRun: POST "/dryrun", api::DryRunRequest => api::DryRunResult;

    // ─────────────────────────────────────────────── 客户端（接管本身在桌面端）
    ClientKey: POST "/clients/{id}/key" [id], () => api::ClientKey;

    // ─────────────────────────────────────────────── 密钥
    Keys: GET "/keys", () => Vec<api::ClientView>;
    CreateKey: POST "/keys", api::KeySave => api::ConfigWritten;
    UpdateKey: PUT "/keys/{name}" [name], api::KeySave => api::ConfigWritten;
    DeleteKey: DELETE "/keys/{name}" [name], api::BaseVersion => api::ConfigWritten;
    KeyValue: GET "/keys/{name}/value" [name], () => api::KeyValue;
    RotateKey: POST "/keys/{name}/rotate" [name], api::KeyRotate => api::KeyRotated;
    SetDefaultKey: PUT "/default_key", api::DefaultKeySave => api::ConfigWritten;

    // ─────────────────────────────────────────────── 上游与代理
    CreateProvider: POST "/providers", api::ProviderSave => api::ConfigWritten;
    TestProvider: POST "/providers/test", api::ProviderTest => api::ProviderTestResult;
    PreviewProvider: POST "/providers/preview", api::ProviderPreviewRequest => api::ProviderPreview;
    UpdateProvider: PUT "/providers/{name}" [name], api::ProviderSave => api::ConfigWritten;
    DeleteProvider: DELETE "/providers/{name}" [name], api::BaseVersion => api::ConfigWritten;
    ProviderModels: GET "/providers/{name}/models" [name], () => api::ProviderModelsView;
    RefreshProviderModels: POST "/providers/{name}/models/refresh" [name], () => api::ProviderModelsView;
    RefreshStaleModels: POST "/models/refresh", () => api::ModelsRefreshing;
    CreateProxy: POST "/proxies", api::ProxySave => api::ConfigWritten;
    TestProxy: POST "/proxies/test", api::ProxyTest => api::L1Result;
    UpdateProxy: PUT "/proxies/{name}" [name], api::ProxySave => api::ConfigWritten;
    DeleteProxy: DELETE "/proxies/{name}" [name], api::BaseVersion => api::ConfigWritten;

    // ─────────────────────────────────────────────── 路由
    CreateRoute: POST "/routes", api::RouteSave => api::ConfigWritten;
    UpdateRoute: PUT "/routes/{name}" [name], api::RouteSave => api::ConfigWritten;
    DeleteRoute: DELETE "/routes/{name}" [name], api::RouteDelete => api::ConfigWritten;
    SetDefaultRoute: PUT "/default_route", api::DefaultRouteSave => api::ConfigWritten;
    CreateGroup: POST "/groups", api::GroupSave => api::ConfigWritten;
    UpdateGroup: PUT "/groups/{name}" [name], api::GroupSave => api::ConfigWritten;
    DeleteGroup: DELETE "/groups/{name}" [name], api::BaseVersion => api::ConfigWritten;
    KnownModels: GET "/models", () => Vec<api::KnownModel>;

    // ─────────────────────────────────────────────── 价目表
    Pricing: GET "/pricing", () => api::PricingStatus;
    RefreshPricing: POST "/pricing/refresh", () => api::PricingRefreshed;
    SetPricingAutoUpdate: PUT "/pricing/auto_update", api::AutoUpdateSave => api::ConfigWritten;
    QueryPrice: POST "/pricing/query", api::PriceQuery => api::PriceQueryResult;
    CreatePriceSheet: POST "/pricing/sheets", api::PriceSheetSave => api::ConfigWritten;
    PriceSheet: GET "/pricing/sheets/{name}" [name], () => api::PriceSheetInput;
    UpdatePriceSheet: PUT "/pricing/sheets/{name}" [name], api::PriceSheetSave => api::ConfigWritten;
    DeletePriceSheet: DELETE "/pricing/sheets/{name}" [name], api::BaseVersion => api::ConfigWritten;

    // ─────────────────────────────────────────────── 安全
    Security: GET "/security", () => api::SecurityDetail;
    SecurityEvents: GET "/security/events", api::SecurityEventsQuery => api::SecurityEventsPage;
    SetSecurityMode: PUT "/security/{guard}/mode" [guard], api::ModeSave => api::ConfigWritten;
    ToggleBuiltinRule: PUT "/security/{guard}/builtin/{id}" [guard, id], api::RuleToggle => api::ConfigWritten;
    SetBuiltinRuleAction: PUT "/security/{guard}/builtin/{id}/action" [guard, id], api::ActionSave => api::ConfigWritten;
    /// 只有 `output_limit` 有上限
    SetSecurityLimit: PUT "/security/{guard}/limit" [guard], api::LimitSave => api::ConfigWritten;
    CreateCustomRule: POST "/security/{guard}/custom" [guard], api::CustomRuleSave => api::ConfigWritten;
    UpdateCustomRule: PUT "/security/{guard}/custom/{name}" [guard, name], api::CustomRuleSave => api::ConfigWritten;
    DeleteCustomRule: DELETE "/security/{guard}/custom/{name}" [guard, name], api::BaseVersion => api::ConfigWritten;
    TestSecurity: POST "/security/{guard}/test" [guard], api::SecurityTestRequest => api::SecurityTestResult;

    // ─────────────────────────────────────────────── 账号登录
    StartChatgptLogin: POST "/chatgpt/login", api::ChatgptLoginStart => api::ChatgptLogin;
    ChatgptLoginStatus: GET "/chatgpt/login/{id}" [id], () => api::ChatgptLoginStatus;
    CancelChatgptLogin: DELETE "/chatgpt/login/{id}" [id], () => api::ChatgptLoginStatus;
    ChatgptUsage: GET "/providers/{name}/chatgpt/usage" [name], () => api::ChatgptUsage;
    ChatgptResets: GET "/providers/{name}/chatgpt/resets" [name], () => api::ResetCredits;
    UseChatgptReset: POST "/providers/{name}/chatgpt/resets" [name], api::ResetCreditUse => api::ResetCreditUsed;
    StartZaiLogin: POST "/zai/login", api::ZaiLoginStart => api::ZaiLogin;
    ZaiLoginStatus: GET "/zai/login/{id}" [id], () => api::ZaiLoginStatus;
    CancelZaiLogin: DELETE "/zai/login/{id}" [id], () => api::ZaiLoginStatus;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::Method;
    use std::collections::HashSet;

    /// 模板里的 `{参数}` 和声明的参数一一对上，顺序也一样。
    #[test]
    fn every_template_parameter_is_declared_and_nothing_else() {
        for e in ALL {
            let in_template: Vec<&str> = e
                .path
                .split('/')
                .filter_map(|s| s.strip_prefix('{')?.strip_suffix('}'))
                .collect();
            assert_eq!(in_template, e.params, "{}: {}", e.name, e.path);
        }
    }

    /// 同一个路径上同一个方法只能有一个端点，名字也不能重。
    #[test]
    fn no_two_endpoints_collide() {
        let mut seen = HashSet::new();
        let mut names = HashSet::new();
        for e in ALL {
            assert!(
                seen.insert((e.method, e.path)),
                "{} {}",
                e.method.as_str(),
                e.path
            );
            assert!(names.insert(e.name), "{}", e.name);
        }
    }

    #[test]
    fn paths_are_filled_and_encoded() {
        assert_eq!(RequestDetail::path(42), "/request/42");
        assert_eq!(
            UpdateCustomRule::path("outbound", "my rule"),
            "/security/outbound/custom/my%20rule"
        );
        assert_eq!(Status::path(), "/status");
    }

    #[test]
    fn only_get_and_delete_carry_the_request_in_the_query() {
        assert!(Method::Get.query() && Method::Delete.query());
        assert!(!Method::Post.query() && !Method::Put.query() && !Method::Patch.query());
    }
}
