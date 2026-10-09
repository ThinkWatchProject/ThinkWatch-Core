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
    /// 配置读不进来时，一键修复会改哪几处
    ConfigRepairPlan: GET "/config/repair", () => api::ConfigRepair;
    /// 照那几处修好写回：取值改回默认值、删掉不认识的字段
    RepairConfig: POST "/config/repair", api::ConfigRepairRequest => api::ConfigWritten;
    SaveListen: PUT "/listen", api::ListenSave => api::ConfigWritten;

    // ─────────────────────────────────────────────── 用量与记录
    Summary: GET "/summary", api::Window => api::Summary;
    CostBuckets: GET "/summary/buckets", api::BucketQuery => Vec<api::CostBucket>;
    CostBucketsBy: GET "/summary/buckets/by", api::BucketGroupQuery => Vec<api::CostBucketGroup>;
    CostBy: GET "/summary/by", api::GroupQuery => Vec<api::CostGroup>;
    /// 各条路由走了多少请求、各条规则命中了多少，记录从哪一刻起是全的
    RouteStats: GET "/summary/routes", api::Window => api::RouteStats;
    History: GET "/history", api::ListQuery => Vec<api::HistoryRow>;
    /// 在整份记录里找，一页一页往回翻；也可以按正文找
    HistorySearch: POST "/history/search", api::HistorySearchQuery => api::HistorySearchPage;
    Latency: GET "/latency", api::Window => Vec<api::LatencyView>;
    LatencyByProvider: GET "/latency/provider", api::Window => Vec<api::LatencyView>;
    /// 生成速度的中位数，按模型、按上游
    TokenRate: GET "/token-rate", api::Window => Vec<api::TokenRateView>;
    TokenRateByProvider: GET "/token-rate/provider", api::Window => Vec<api::TokenRateView>;
    /// 上游体检：每家上游的失败、回答里的模型名、输入用量和本地估算之比、缓存读、
    /// 延迟与速度，各带样本数和别家的参照。不给时间窗是最近 7 天
    UpstreamHealth: GET "/upstreams/health", api::Window => api::UpstreamHealth;
    RequestDetail: GET "/request/{id}" [id], () => api::RequestDetail;
    /// 把一条记录变成回放用例（YAML）
    Fixture: GET "/request/{id}/fixture" [id], () => String, text;
    Sessions: GET "/sessions", api::ListQuery => Vec<api::SessionView>;
    SessionDetail: GET "/sessions/{id}" [id], () => api::SessionDetail;
    /// 一次会话读成一段对话：每一轮新说的话、回答、工具调用和结果（已脱敏）。可以只要
    /// 从某一轮起的那些
    SessionTranscript: GET "/sessions/{id}/transcript" [id], api::TranscriptQuery => api::Transcript;

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
    /// 检测和预览一家还没保存的上游。**不在 `/providers/` 底下**，代理的检测也一样：写死
    /// 的一段会盖住 `/providers/{name}`，叫 `test` 的上游就改不了、删不掉了
    TestProvider: POST "/provider-test", api::ProviderTest => api::ProviderTestResult;
    PreviewProvider: POST "/provider-preview", api::ProviderPreviewRequest => api::ProviderPreview;
    UpdateProvider: PUT "/providers/{name}" [name], api::ProviderSave => api::ConfigWritten;
    DeleteProvider: DELETE "/providers/{name}" [name], api::BaseVersion => api::ConfigWritten;
    ProviderModels: GET "/providers/{name}/models" [name], () => api::ProviderModelsView;
    /// 手写一家上游的一个模型的上下文窗口、输出上限，优先于价目表；两项都空就删掉。
    /// **不在 `/providers/` 底下**：写死的一段会盖住 `/providers/{name}`
    SetModelSpec: PUT "/provider-model-spec", api::ModelSpecSave => api::ConfigWritten;
    /// 换掉一家上游手动添加的模型（整份清单，空的就是清掉）：上游能服务、却没列进清单的
    /// 模型，和列出的一样算这家提供。**不在 `/providers/` 底下**，同上
    SetManualModels: PUT "/provider-manual-models", api::ManualModelsSave => api::ConfigWritten;
    RefreshProviderModels: POST "/providers/{name}/models/refresh" [name], () => api::ProviderModelsView;
    RefreshStaleModels: POST "/models/refresh", () => api::ModelsRefreshing;
    CreateProxy: POST "/proxies", api::ProxySave => api::ConfigWritten;
    TestProxy: POST "/proxy-test", api::ProxyTest => api::L1Result;
    UpdateProxy: PUT "/proxies/{name}" [name], api::ProxySave => api::ConfigWritten;
    DeleteProxy: DELETE "/proxies/{name}" [name], api::BaseVersion => api::ConfigWritten;

    // ─────────────────────────────────────────────── 模型别名
    /// 全部别名，按书写顺序：各家发出的名称、同名被挡住的上游、24 小时用量；以及同一个
    /// 模型在各家叫不同名称的建议（只认 Claude）
    Aliases: GET "/aliases", () => api::AliasesView;
    CreateAlias: POST "/aliases", api::AliasSave => api::ConfigWritten;
    /// 预览一个还没保存的别名：挡着保存的问题、发往各家的名称、别家上的同一个模型。
    /// **不在 `/aliases/` 底下**：`/aliases/preview` 会盖住 `/aliases/{name}`，一个叫
    /// `preview` 的别名就改不了、删不掉了
    PreviewAlias: POST "/alias-preview", api::AliasPreviewRequest => api::AliasPreview;
    /// 保存，可以改名：引用旧名的密钥（`allow` 里的整项）和规则（`when.model`、
    /// `set.model`）在同一个版本里跟着改。旧名还在改名后的列表里时，`allow` 和
    /// `when.model` 不改（见 [`api::AliasWritten::renamed_in`]）
    UpdateAlias: PUT "/aliases/{name}" [name], api::AliasSave => api::AliasWritten;
    /// 删掉。**引用它的密钥和规则不拦**：删之前先看 `AliasUsage`
    DeleteAlias: DELETE "/aliases/{name}" [name], api::BaseVersion => api::ConfigWritten;
    /// 谁在用它：24 小时的请求、密钥、规则
    AliasUsage: GET "/aliases/{name}/usage" [name], () => api::AliasUsage;

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
    CreateCustomRule: POST "/security/{guard}/custom" [guard], api::CustomRuleSave => api::ConfigWritten;
    UpdateCustomRule: PUT "/security/{guard}/custom/{name}" [guard, name], api::CustomRuleSave => api::ConfigWritten;
    DeleteCustomRule: DELETE "/security/{guard}/custom/{name}" [guard, name], api::BaseVersion => api::ConfigWritten;
    TestSecurity: POST "/security/{guard}/test" [guard], api::SecurityTestRequest => api::SecurityTestResult;

    // ─────────────────────────────────────────────── 脚本插件
    //
    // 插件的配置（出错时怎么办、范围、设置的值）**在插件文件自己的 manifest 里**，界面改它们
    // 就是改源码（`PluginRewrite` 改写、`SavePlugin` 保存）。
    //
    // **要点头的只有一种插件**：权限有 `reply_tool_calls` 的（改得了客户端要执行的工具调用）。
    // 装它、打开它、改它的代码、批准它磁盘上改过的文件，网页调得到的那条路一律拒绝（403，
    // `control.plugin.needs_confirmation`），要走带 `confirmed` 的那一条：那几条**桌面端不放进
    // 网页的 `call` 白名单**，它的 Rust 先自己再编一遍源码（或者读一遍插件现在的样子），在系统
    // 的确认框里把名字、权限和要改的地方摆给人看，点了头才发。网页里注入的脚本调不到它们。
    // 每个端点说明里写着桌面端能不能把它给网页。
    //
    // 不针对某一个插件的那几个（试编、改写、点过头的装、排顺序）**不在 `/plugins/` 底下**：
    // 插件的 id 是用户起的，写死的一段会盖住 `/plugins/{id}`。
    /// 全部插件，按运行的顺序：状态、计数。**网页可以调**
    Plugins: GET "/plugins", () => Vec<api::PluginView>;
    /// 编一份源码看看它是什么插件，**什么都不留下**。**网页可以调**
    PluginInspect: POST "/plugin-inspect", api::PluginSource => api::PluginInspection;
    /// 改写一份源码里的数据（出错时怎么办、范围、设置的值），交回改写之后的源码：只换
    /// manifest 那一段，别的字节一个不动。**没有副作用**，不读不写任何文件和配置。
    /// 界面的设置表单靠它更新代码视图（反过来，代码 → 表单用 `PluginInspect`）。
    /// **网页可以调**
    PluginRewrite: POST "/plugin-rewrite", api::PluginRewriteRequest => api::PluginSource;
    /// 装一个：写插件文件和它的底稿，配置里加一条。改得了工具调用的插件在这里拒绝（403，
    /// `control.plugin.needs_confirmation`），要走 `CreatePluginConfirmed`。**网页可以调**
    CreatePlugin: POST "/plugins", api::PluginCreate => api::ConfigWritten;
    /// 同一件事，在系统的确认框里点过头了：改得了工具调用的插件也装得上。**网页不能调，
    /// 桌面端也不许把它放进网页的白名单**
    CreatePluginConfirmed: POST "/plugin-confirmed", api::PluginCreate => api::ConfigWritten;
    /// 排顺序，也就是运行的顺序。**网页可以调**
    ReorderPlugins: PUT "/plugin-order", api::PluginOrder => api::ConfigWritten;
    /// 保存：源码和开关。core 拿源码和批准的那一份比，分成「只改了数据」和「改了代码」
    /// （见 `PluginSave`）；写文件、底稿和配置里的哈希是一件事，中间没有「文件变了」
    /// 的那一刻。改得了工具调用的插件，改代码、打开它在这里拒绝（403，
    /// `control.plugin.needs_confirmation`），要走 `SavePluginConfirmed`；只改数据、停用照常。
    /// **网页可以调**
    SavePlugin: PUT "/plugins/{id}" [id], api::PluginSave => api::ConfigWritten;
    /// 同一件事，在系统的确认框里点过头了。**网页不能调，桌面端也不许把它放进网页的白名单**：
    /// 网页里注入的脚本调得到它，就能自己打开一个改工具调用的插件、改它的代码。桌面端的 Rust
    /// 先弹系统的确认框（插件的名字、它能做什么、这次改了什么），点了头再发
    SavePluginConfirmed: PUT "/plugins/{id}/confirmed" [id], api::PluginSave => api::ConfigWritten;
    /// 删掉：配置里那一条、插件文件和底稿。**网页可以调**
    DeletePlugin: DELETE "/plugins/{id}" [id], api::BaseVersion => api::ConfigWritten;
    /// 批准过的那一份和磁盘上现在那一份。**网页可以调**
    PluginSourceDiff: GET "/plugins/{id}/source" [id], () => api::PluginSourceView;
    /// 批准磁盘上改过的那个文件。改得了工具调用的插件（新旧两份里有一份能）在这里拒绝（403，
    /// `control.plugin.needs_confirmation`），要走 `ApprovePluginFileConfirmed`。**网页可以调**
    ApprovePluginFile: POST "/plugins/{id}/approve" [id], api::PluginApprove => api::ConfigWritten;
    /// 同一件事，在系统的确认框里点过头了。**网页不能调，桌面端也不许把它放进网页的白名单**
    ApprovePluginFileConfirmed: POST "/plugins/{id}/approve/confirmed" [id], api::PluginApprove => api::ConfigWritten;
    /// 拿一条记下的请求试跑。**不连上游**。**网页可以调**
    TrialPlugin: POST "/plugins/{id}/trial" [id], api::PluginTrial => api::PluginTrialResult;
    /// 最近的日志，老的在前。**网页可以调**
    PluginLogs: GET "/plugins/{id}/logs" [id], () => Vec<api::PluginLogEntry>;

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

    /// 名字是用户起的，什么词都可能：一个端点的某一段是 `{参数}` 的话，前面几段相同的
    /// 别的端点在这一段上不能写死一个词，不管方法是什么。写死了，axum 先认写死的那个，
    /// 叫这个词的上游（代理、插件……）就只剩那一个端点的方法，改和删都是 405 ——
    /// `PUT /plugins/order` 更是进了排顺序。
    ///
    /// 取值由 core 定死的参数（`{guard}`）除外，只要写死的词不是其中一个。
    #[test]
    fn no_fixed_segment_shadows_a_parameter() {
        // 这个词能不能是这个参数的一个取值
        let could_be = |param: &str, word: &str| match param {
            "{guard}" => api::Guard::from_slug(word).is_some(),
            _ => true,
        };
        let is_param = |s: &str| s.starts_with('{');
        let segments = |p: &'static str| p.split('/').skip(1).collect::<Vec<_>>();
        let mut shadowed = std::collections::BTreeSet::new();
        for a in ALL {
            let sa = segments(a.path);
            for (i, param) in sa.iter().enumerate().filter(|(_, s)| is_param(s)) {
                for b in ALL {
                    let sb = segments(b.path);
                    let Some(word) = sb.get(i).filter(|s| !is_param(s)) else {
                        continue;
                    };
                    let same_prefix = sa[..i]
                        .iter()
                        .zip(&sb[..i])
                        .all(|(x, y)| x == y || (is_param(x) && is_param(y)));
                    if same_prefix && could_be(param, word) {
                        shadowed.insert(format!(
                            "{} {}: `{word}` where /{} takes any name",
                            b.method.as_str(),
                            b.path,
                            sa[..=i].join("/")
                        ));
                    }
                }
            }
        }
        assert!(shadowed.is_empty(), "{shadowed:#?}");
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

    /// 别名的名字是用户起的，什么词都可能：`/aliases/` 下面的第二段只能是 `{name}`。
    /// 写死一个词（`/aliases/preview`）的话，axum 先认写死的那个，叫这个词的别名就只剩
    /// 那一个方法，改和删都是 405
    #[test]
    fn no_fixed_path_under_aliases_shadows_an_alias_name() {
        for e in ALL {
            let segments: Vec<&str> = e.path.split('/').skip(1).collect();
            if segments.first() == Some(&"aliases") && segments.len() > 1 {
                assert_eq!(segments[1], "{name}", "{}: {}", e.name, e.path);
            }
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
