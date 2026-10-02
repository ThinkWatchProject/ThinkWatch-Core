//! 配置里的词和契约里的词：同一组词两边各有一个枚举，靠这里穷尽的 `match`
//! 对齐。多一个变体，这里编译不过。安全防护的档位、匹配方式两边是同一个类型
//! （`tw_guard::policy`），不用对齐。

use crate::{Billing, OnProxyFail, ProbeAction, Protocol, ProxyKind, Stage};

impl From<Billing> for tw_api::Billing {
    fn from(b: Billing) -> Self {
        match b {
            Billing::PerToken => Self::PerToken,
            Billing::Free => Self::Free,
        }
    }
}

impl From<tw_api::Billing> for Billing {
    fn from(b: tw_api::Billing) -> Self {
        match b {
            tw_api::Billing::PerToken => Self::PerToken,
            tw_api::Billing::Free => Self::Free,
        }
    }
}

impl From<Protocol> for tw_api::Protocol {
    fn from(p: Protocol) -> Self {
        match p {
            Protocol::Anthropic => Self::Anthropic,
            Protocol::OpenaiChat => Self::OpenaiChat,
            Protocol::OpenaiResponses => Self::OpenaiResponses,
            Protocol::Gemini => Self::Gemini,
            Protocol::Chatgpt => Self::Chatgpt,
            Protocol::Bedrock => Self::Bedrock,
        }
    }
}

impl From<tw_api::Protocol> for Protocol {
    fn from(p: tw_api::Protocol) -> Self {
        match p {
            tw_api::Protocol::Anthropic => Self::Anthropic,
            tw_api::Protocol::OpenaiChat => Self::OpenaiChat,
            tw_api::Protocol::OpenaiResponses => Self::OpenaiResponses,
            tw_api::Protocol::Gemini => Self::Gemini,
            tw_api::Protocol::Chatgpt => Self::Chatgpt,
            tw_api::Protocol::Bedrock => Self::Bedrock,
        }
    }
}

impl From<ProxyKind> for tw_api::ProxyKind {
    fn from(k: ProxyKind) -> Self {
        match k {
            ProxyKind::Socks5h => Self::Socks5h,
            ProxyKind::Socks5 => Self::Socks5,
            ProxyKind::Http => Self::Http,
            ProxyKind::Https => Self::Https,
        }
    }
}

impl From<tw_api::ProxyKind> for ProxyKind {
    fn from(k: tw_api::ProxyKind) -> Self {
        match k {
            tw_api::ProxyKind::Socks5h => Self::Socks5h,
            tw_api::ProxyKind::Socks5 => Self::Socks5,
            tw_api::ProxyKind::Http => Self::Http,
            tw_api::ProxyKind::Https => Self::Https,
        }
    }
}

impl From<OnProxyFail> for tw_api::OnProxyFail {
    fn from(o: OnProxyFail) -> Self {
        match o {
            OnProxyFail::Fail => Self::Fail,
            OnProxyFail::Direct => Self::Direct,
        }
    }
}

impl From<tw_api::OnProxyFail> for OnProxyFail {
    fn from(o: tw_api::OnProxyFail) -> Self {
        match o {
            tw_api::OnProxyFail::Fail => Self::Fail,
            tw_api::OnProxyFail::Direct => Self::Direct,
        }
    }
}

impl From<crate::history::Origin> for tw_api::ConfigOrigin {
    fn from(o: crate::history::Origin) -> Self {
        use crate::history::Origin;
        match o {
            Origin::Ui => Self::Ui,
            Origin::Cli => Self::Cli,
            Origin::External => Self::External,
            Origin::Rollback => Self::Rollback,
            Origin::Rotation => Self::Rotation,
            Origin::Defaults => Self::Defaults,
        }
    }
}

impl From<Stage> for tw_api::ConfigStage {
    fn from(s: Stage) -> Self {
        match s {
            Stage::Syntax => Self::Syntax,
            Stage::Schema => Self::Schema,
            Stage::Semantics => Self::Semantics,
        }
    }
}

impl From<ProbeAction> for tw_api::ProbeMode {
    fn from(a: ProbeAction) -> Self {
        match a {
            ProbeAction::Intercept => Self::Intercept,
            ProbeAction::Route => Self::Route,
            ProbeAction::Passthrough => Self::Passthrough,
        }
    }
}
