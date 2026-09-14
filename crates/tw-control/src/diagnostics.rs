//! 诊断包（M6+，脱敏纪律）。
//!
//! 用户遇到问题时要能一次性交出「我这儿是什么情况」。手工问一轮
//! （版本？配置？哪家上游？日志？）要来回好几趟，而每一趟都可能问漏。
//!
//! # 这个功能的全部风险都在一句话里
//!
//! > **我们是一个看得见所有 API key 的网关。**
//!
//! 持久化的日志会被用户直接贴进 issue，而这件事对我们
//! 的风险比一般应用高一个量级。所以这个包有三条硬规矩：
//!
//! 1. **一律脱敏**，走和请求详情同一个函数 —— 两处标准不同的话，仔细
//!    的那一处等于白做（cc-switch 那个「同一个程序，两个界面，
//!    两套标准」的例子）。
//! 2. **写成人能读的 Markdown，不是 JSON dump。**用户在交出去之前会看
//!    一眼；看不懂的东西他不会看，也就没法发现里面有什么不该有的。
//! 3. **不含请求体和响应体。**它们最有用，也最危险 —— 需要的话请求详情
//!    页里单独看，那一页是他自己打开的，不会被顺手贴进 issue。

use std::fmt::Write as _;

use axum::extract::State;

use crate::ControlState;

fn line(out: &mut String, k: &str, v: impl std::fmt::Display) {
    let _ = writeln!(out, "| {k} | {v} |");
}

/// 攒一份诊断包。**只读，不写任何文件。**
pub async fn bundle(State(s): State<ControlState>) -> String {
    let mut out = String::new();
    let cfg = s.config();
    let _ = writeln!(out, "# ThinkWatch 诊断包\n");
    let _ = writeln!(
        out,
        "> 这份内容里的密钥、地址、请求正文都已经打码。交出去之前请自己扫一眼 —— \
         **我们是一个看得见所有 API key 的网关**，这一步值得多花十秒。\n"
    );

    // ---- 版本和平台
    let _ = writeln!(out, "## 版本\n\n| | |\n|---|---|");
    line(&mut out, "core", env!("CARGO_PKG_VERSION"));
    line(&mut out, "控制面 API", tw_api::CONTROL_API_VERSION);
    line(&mut out, "配置 schema", cfg.version);
    line(&mut out, "价目表快照", tw_pricing::SNAPSHOT_DATE);
    line(&mut out, "平台", std::env::consts::OS);
    line(&mut out, "架构", std::env::consts::ARCH);
    line(
        &mut out,
        "已运行",
        format!("{} 秒", s.started.elapsed().as_secs()),
    );

    // ---- 监听
    let _ = writeln!(out, "\n## 监听\n\n| | |\n|---|---|");
    line(
        &mut out,
        "网关",
        s.gateway_addr.as_deref().unwrap_or("没起（安全模式）"),
    );
    line(&mut out, "绑定", format!("{:?}", cfg.listen.gateway.bind));
    line(&mut out, "端口", cfg.listen.gateway.port);
    line(
        &mut out,
        "控制面",
        s.config_path()
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    );

    // ---- 上游
    let _ = writeln!(
        out,
        "\n## 上游（{} 个）\n\n| 名字 | 地址 | 协议 | 代理 | 熔断 | 脱敏 | 信任 |\n|---|---|---|---|---|---|---|",
        cfg.providers.len()
    );
    for p in &cfg.providers {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} |",
            p.name,
            // **地址也要脱敏** —— 中转站的 base_url 里常常带着 key
            tw_secret::redact_url(&p.base_url),
            p.effective_protocol()
                .map(|x| format!("{x:?}"))
                .unwrap_or_else(|| "猜不出".into()),
            p.proxy,
            if s.health().is_available(&p.name) {
                "正常"
            } else {
                "**熔断中**"
            },
            {
                let k = p.effective_redact();
                if k.is_empty() {
                    "不脱".to_string()
                } else {
                    k.iter().map(|x| x.slug()).collect::<Vec<_>>().join(" ")
                }
            },
            p.effective_trust().label(),
        );
    }

    // ---- 客户端条目（**只有名字和脱敏后的 key**）
    let _ = writeln!(
        out,
        "\n## 网关密钥（{} 把）\n\n| 名字 | key |\n|---|---|",
        cfg.clients.len()
    );
    for c in &cfg.clients {
        let _ = writeln!(out, "| {} | {} |", c.name, tw_secret::mask_secret(&c.key));
    }

    // ---- 安全三态
    let _ = writeln!(out, "\n## 安全\n\n| | |\n|---|---|");
    line(&mut out, "出站脱敏", cfg.security.redact.label());
    line(&mut out, "入站审查", cfg.security.inspect_tools.label());
    line(&mut out, "配置扫描", cfg.security.scan_configs.label());

    // ---- 存储和最近的失败
    match &s.store {
        None => {
            let _ = writeln!(out, "\n## 观测\n\n**没有启动。**这段时间的请求没有被记录。");
        }
        Some(store) => {
            let g = store.lock().await;
            let _ = writeln!(out, "\n## 观测\n\n| | |\n|---|---|");
            line(&mut out, "磁盘状态", format!("{:?}", g.level()));
            line(&mut out, "请求条数", g.db().count().unwrap_or(0));
            line(
                &mut out,
                "留档大小",
                format!("{} 字节", g.blobs().total_bytes()),
            );

            let recent = g.db().recent(200).unwrap_or_default();
            let failed: Vec<_> = recent
                .iter()
                .filter(|r| r.error.is_some())
                .take(20)
                .collect();
            let _ = writeln!(
                out,
                "\n### 最近的失败（{} 条，取自最近 {} 条请求）\n",
                failed.len(),
                recent.len()
            );
            if failed.is_empty() {
                let _ = writeln!(out, "没有。");
            } else {
                let _ = writeln!(out, "| 时间 | 上游 | 状态 | 错误 |\n|---|---|---|---|");
                for r in failed {
                    let _ = writeln!(
                        out,
                        "| {} | {} | {} | {} |",
                        r.at_ms,
                        r.provider,
                        r.status
                            .map(|x| x.to_string())
                            .unwrap_or_else(|| "—".into()),
                        // **错误信息也要脱敏** —— 上游的 401 正文里可能
                        // 回显了我们发过去的 key
                        tw_secret::mask_body(r.error.as_deref().unwrap_or("")),
                    );
                }
            }
        }
    }

    // ---- 配置原文（脱敏后）
    let _ = writeln!(out, "\n## config.yaml（已脱敏）\n\n```yaml");
    match std::fs::read_to_string(s.config_path()) {
        // **`mask_body` 一个人不够。**它认的是值的形状，而配置里有一
        // 类密钥没有形状：自建中转那把普通样子的 key、OAuth 的 refresh
        // token。实测它们原样穿过去了 —— 而这一段的上面就写着「已脱敏」。
        //
        // `mask_config_yaml` 按**字段名**打（schema 就是答案），里面照旧
        // 叠一层 `mask_body`：仔细的那一处不能被另一处抵消。
        Ok(text) => {
            let _ = writeln!(out, "{}", tw_secret::mask_config_yaml(&text));
        }
        Err(e) => {
            let _ = writeln!(out, "# 读不了：{e}");
        }
    }
    let _ = writeln!(out, "```");

    let _ = writeln!(
        out,
        "\n---\n\n**不含请求体和响应体。**它们最有用也最危险 —— 需要的话在请求详情页里单独看。"
    );
    out
}
