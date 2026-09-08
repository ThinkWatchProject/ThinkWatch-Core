//! 控制面的**契约**。CLI、Tauri UI、第三方都依赖它（DESIGN.md §9.5）。
//!
//! 它刻意不含任何 IO —— 只有类型。这样它能被 Tauri 的前端（通过
//! ts-rs 之类的导出）、CLI、和将来的第三方同时依赖，而不会拖上一个
//! HTTP 栈。

use serde::{Deserialize, Serialize};

/// 控制面协议版本。UI 和 CLI 连上来时检查，不匹配就明确提示「请升级
/// 客户端」，而不是以奇怪的方式失败（§9.6）。
pub const CONTROL_API_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub api_version: u32,
    /// 二进制的 CalVer
    pub version: String,
    pub pid: u32,
    /// 数据面在监听哪儿。安全模式下是 None —— 那正是「只起控制面」的
    /// 可观测形态。
    pub gateway_addr: Option<String>,
    pub config_path: String,
    pub clients: usize,
    pub providers: usize,
    pub uptime_secs: u64,
}

/// 一次请求的观测事件。UI 的实时列表吃这个。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// 请求进来了
    RequestStarted {
        id: u64,
        client: String,
        provider: String,
        method: String,
        path: String,
        at_ms: u64,
    },
    /// 收到上游响应头。**这个事件单独存在是有意的**：流式请求从这里
    /// 到结束可能还有好几分钟，UI 要能在这个点就把行画出来并显示
    /// 「进行中」，而不是等它结束才出现。
    RequestHeaders { id: u64, status: u16, ttfb_ms: u64 },
    /// 结束了
    RequestFinished {
        id: u64,
        status: u16,
        bytes: u64,
        duration_ms: u64,
    },
    /// 失败了。`source` 和 HTTP 响应里的 `x-thinkwatch-error` 是同一个词表。
    RequestFailed {
        id: u64,
        source: String,
        message: String,
    },
}

impl Event {
    pub fn id(&self) -> u64 {
        match self {
            Event::RequestStarted { id, .. }
            | Event::RequestHeaders { id, .. }
            | Event::RequestFinished { id, .. }
            | Event::RequestFailed { id, .. } => *id,
        }
    }
}

/// 探一个上游能不能用。零成本，见 §4.6 的 L1/L2。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeRequest {
    pub base_url: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResponse {
    pub ok: bool,
    pub protocol: Option<String>,
    pub latency_ms: u64,
    pub models: Vec<String>,
    pub error: Option<String>,
}

/// 首次运行时写下第一个上游。
///
/// **只在还没有 provider 时可用**。之后改配置走 §3.8 的双向同步（M2），
/// 那是另一套机制：这里是从无到有整文件生成，那边是改一个字节而保住
/// 其余全部。混用会让「注释和格式原样保留」这条承诺失效。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupRequest {
    pub name: String,
    pub base_url: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupResponse {
    /// 写完之后，客户端该用哪把网关密钥
    pub gateway_key: String,
    pub gateway_addr: String,
    pub config_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_carry_a_discriminating_tag() {
        // 前端按 `kind` 分派。少了它，TypeScript 那边只能靠字段有无来猜。
        let e = Event::RequestFinished {
            id: 1,
            status: 200,
            bytes: 10,
            duration_ms: 5,
        };
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "request_finished");
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn every_event_exposes_its_request_id() {
        // UI 靠 id 把四个事件缝成一行。
        for e in [
            Event::RequestStarted {
                id: 7,
                client: "c".into(),
                provider: "p".into(),
                method: "POST".into(),
                path: "/v1/messages".into(),
                at_ms: 0,
            },
            Event::RequestHeaders {
                id: 7,
                status: 200,
                ttfb_ms: 1,
            },
            Event::RequestFinished {
                id: 7,
                status: 200,
                bytes: 1,
                duration_ms: 1,
            },
            Event::RequestFailed {
                id: 7,
                source: "upstream".into(),
                message: "x".into(),
            },
        ] {
            assert_eq!(e.id(), 7);
        }
    }

    #[test]
    fn status_round_trips() {
        let s = Status {
            api_version: CONTROL_API_VERSION,
            version: "2026.9.0".into(),
            pid: 1,
            gateway_addr: Some("127.0.0.1:8788".into()),
            config_path: "/x/config.yaml".into(),
            clients: 1,
            providers: 1,
            uptime_secs: 0,
        };
        let back: Status = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.gateway_addr.as_deref(), Some("127.0.0.1:8788"));
    }

    #[test]
    fn safe_mode_is_representable() {
        // 安全模式 = 控制面在、数据面不在。它必须是一个能被表达的状态，
        // 而不是「gateway_addr 是空字符串」这种约定。
        let json = r#"{"api_version":1,"version":"x","pid":1,"gateway_addr":null,
                       "config_path":"/x","clients":0,"providers":0,"uptime_secs":0}"#;
        let s: Status = serde_json::from_str(json).unwrap();
        assert!(s.gateway_addr.is_none());
    }
}
