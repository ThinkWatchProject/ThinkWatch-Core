//! 三项防护（出站脱敏、工具调用审查、内容过滤）在两个产品之间共用的那部分契约。
//!
//! **类型在 tw-guard 里定义一次**（规则视图 [`tw_guard::view`]、「测试…」
//! [`tw_guard::trial`]、档位和匹配方式 [`tw_guard::policy`]），企业版的管理接口返回同一份
//! JSON；这里重导出给桌面版的控制面。导出成 TypeScript 时名字和这里原有的一致
//! （`GuardMode`、`SecurityRuleView`、`SecurityTestRequest`……）。
//!
//! 眼下桌面版的端点还用着本 crate 根上的旧类型；控制面改用这一份之后，根上的旧类型
//! 删掉、换成从这里重导出。

pub use tw_guard::policy::{ContentMatch, Guard, Mode as GuardMode};
pub use tw_guard::trial::{
    TrialHit as SecurityTestHit, TrialRequest as SecurityTestRequest,
    TrialResult as SecurityTestResult,
};
pub use tw_guard::view::{
    CardNetwork, CardPrefix, GuardDetail, Matcher, RuleAction, RuleView as SecurityRuleView,
    SecurityDetail,
};
