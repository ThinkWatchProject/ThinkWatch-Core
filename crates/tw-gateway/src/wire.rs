//! 网关里的词和契约里的词：同一组词两边各有一个枚举，靠这里穷尽的 `match`
//! 对齐。多一个变体，这里编译不过。

use tw_api::{FailureSource, ModelListStatus, ModelSource, ProbeClass};

impl From<crate::error::Source> for FailureSource {
    fn from(s: crate::error::Source) -> Self {
        use crate::error::Source;
        match s {
            Source::Auth => Self::Auth,
            Source::Config => Self::Config,
            Source::Upstream => Self::Upstream,
            Source::Request => Self::Request,
            Source::RateLimited | Source::Busy => Self::RateLimited,
            Source::Denied => Self::Denied,
            Source::NotSupported | Source::TooLarge => Self::Request,
            Source::Timeout => Self::Upstream,
            Source::Slow => Self::Upstream,
            Source::Aborted => Self::Aborted,
        }
    }
}

impl From<crate::clientprobe::ProbeKind> for ProbeClass {
    fn from(k: crate::clientprobe::ProbeKind) -> Self {
        use crate::clientprobe::ProbeKind;
        match k {
            ProbeKind::HealthCheck => Self::HealthCheck,
            ProbeKind::Warmup => Self::Warmup,
            ProbeKind::Titling => Self::Titling,
            ProbeKind::TopicDetect => Self::TopicDetect,
            ProbeKind::Suggestion => Self::Suggestion,
        }
    }
}

impl From<crate::models::Source> for ModelSource {
    fn from(s: crate::models::Source) -> Self {
        use crate::models::Source;
        match s {
            Source::Discovered => Self::Discovered,
            Source::Manual => Self::Manual,
            Source::None => Self::None,
        }
    }
}

impl From<crate::models::Status> for ModelListStatus {
    fn from(s: crate::models::Status) -> Self {
        use crate::models::Status;
        match s {
            Status::Pending => Self::Pending,
            Status::Listed => Self::Listed,
            Status::NoList => Self::NoList,
            Status::Failed => Self::Failed,
        }
    }
}

impl From<crate::models::Skip> for tw_api::ServeSkip {
    fn from(s: crate::models::Skip) -> Self {
        use crate::models::Skip;
        match s {
            Skip::Disabled => Self::Disabled,
            Skip::OutOfScope => Self::OutOfScope,
            Skip::NotOffered => Self::NotOffered,
            Skip::NotAllowed => Self::NotAllowed,
        }
    }
}

/// 请求体格式在契约里的词。
pub fn dialect(d: tw_dialect::ir::Dialect) -> tw_api::Dialect {
    use tw_dialect::ir::Dialect;
    match d {
        Dialect::Anthropic => tw_api::Dialect::Anthropic,
        Dialect::Chat => tw_api::Dialect::OpenaiChat,
        Dialect::Responses => tw_api::Dialect::OpenaiResponses,
        Dialect::Gemini => tw_api::Dialect::Gemini,
        Dialect::Bedrock => tw_api::Dialect::Bedrock,
    }
}

/// 契约里的格式在方言库里的样子。
pub fn from_dialect(d: tw_api::Dialect) -> tw_dialect::ir::Dialect {
    use tw_dialect::ir::Dialect;
    match d {
        tw_api::Dialect::Anthropic => Dialect::Anthropic,
        tw_api::Dialect::OpenaiChat => Dialect::Chat,
        tw_api::Dialect::OpenaiResponses => Dialect::Responses,
        tw_api::Dialect::Gemini => Dialect::Gemini,
        tw_api::Dialect::Bedrock => Dialect::Bedrock,
    }
}

/// 出站脱敏的类别在契约里的词。
pub fn secret_kind(k: tw_guard::redact::rules::Kind) -> tw_api::SecretKind {
    use tw_guard::redact::rules::Kind;
    match k {
        Kind::Saved => tw_api::SecretKind::Saved,
        Kind::AiServices => tw_api::SecretKind::AiServices,
        Kind::DevPlatforms => tw_api::SecretKind::DevPlatforms,
        Kind::Cloud => tw_api::SecretKind::Cloud,
        Kind::Business => tw_api::SecretKind::Business,
        Kind::PrivateKeys => tw_api::SecretKind::PrivateKeys,
        Kind::Jwt => tw_api::SecretKind::Jwt,
        Kind::ConnStrings => tw_api::SecretKind::ConnStrings,
        Kind::InContext => tw_api::SecretKind::InContext,
        Kind::Crypto => tw_api::SecretKind::Crypto,
        Kind::IdDocuments => tw_api::SecretKind::IdDocuments,
        Kind::ForeignIds => tw_api::SecretKind::ForeignIds,
        Kind::Financial => tw_api::SecretKind::Financial,
        Kind::Contact => tw_api::SecretKind::Contact,
        Kind::Location => tw_api::SecretKind::Location,
        Kind::Vehicle => tw_api::SecretKind::Vehicle,
        Kind::Device => tw_api::SecretKind::Device,
        Kind::LabeledPersonal => tw_api::SecretKind::LabeledPersonal,
        Kind::Internal => tw_api::SecretKind::Internal,
        Kind::Custom => tw_api::SecretKind::Custom,
    }
}
