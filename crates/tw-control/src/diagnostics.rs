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
pub async fn bundle(State(s): State<ControlState>) -> Result<String, crate::Fail> {
    // 生成在服务器上、写的是服务器的文件系统：远程拿不到它
    if crate::remote::is_remote() {
        return Err(crate::remote::refused(crate::remote::Refused::Diagnostics));
    }
    let mut out = String::new();
    let cfg = s.config();
    let _ = writeln!(out, "# ThinkWatch diagnostics bundle\n");
    let _ = writeln!(
        out,
        "> The keys, addresses and request bodies in this file are redacted. ThinkWatch is a \
         gateway and can see every API key you own, so read this through once more before you \
         send it anywhere.\n"
    );

    // ---- 版本和平台
    let _ = writeln!(out, "## Versions\n\n| | |\n|---|---|");
    line(&mut out, "core", env!("CARGO_PKG_VERSION"));
    line(&mut out, "Control-plane API", tw_api::CONTROL_API_VERSION);
    line(&mut out, "Config schema", cfg.version);
    {
        // **生效的那一份，不是内置快照的日期** —— 刷新过之后两者不同，而对账
        // 对不上时要问的正是「按哪天的价格算的」
        let book = s.gateway.pricing.load();
        let t = book.table();
        let source = match t.source {
            tw_pricing::TableSource::Builtin => "built in",
            tw_pricing::TableSource::Fetched => "fetched",
            tw_pricing::TableSource::Empty => "not loaded",
        };
        line(
            &mut out,
            "Default price sheet",
            format!("{} ({source}, {} models)", t.date, t.len()),
        );
        line(&mut out, "Custom price sheets", cfg.pricing.sheets.len());
    }
    line(&mut out, "Platform", std::env::consts::OS);
    line(&mut out, "Architecture", std::env::consts::ARCH);
    line(
        &mut out,
        "Uptime",
        format!("{} s", s.started.elapsed().as_secs()),
    );

    // ---- 监听
    let _ = writeln!(out, "\n## Listening\n\n| | |\n|---|---|");
    let listening = s.gateway.listening();
    line(
        &mut out,
        "Gateway",
        if listening.addrs.is_empty() {
            "not started (safe mode)".to_string()
        } else {
            listening
                .addrs
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
    if let Some(e) = &listening.error {
        line(&mut out, "Listen error", e);
    }
    line(&mut out, "Bind", format!("{:?}", cfg.listen.gateway.bind));
    line(&mut out, "Port", cfg.listen.gateway.port);
    line(
        &mut out,
        "Control plane",
        s.config_path()
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    );

    // ---- 上游
    let _ = writeln!(
        out,
        "\n## Upstreams ({})\n\n| Name | Endpoint | Protocol | Proxy | State | Models | Price sheet |\n|---|---|---|---|---|---|---|",
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
                .unwrap_or_else(|| "unrecognized".into()),
            p.proxy,
            if p.disabled {
                "disabled"
            } else if s.health().is_available(&p.name) {
                "ok"
            } else {
                "circuit open"
            },
            {
                // 「这家为什么收不到某个模型的请求」多半答在这一栏
                let l = s.gateway.models.listing(p);
                let served = s.gateway.catalog.load().count_for(&p.name);
                let scope = if p.models_only.is_some() {
                    ", scoped"
                } else {
                    ""
                };
                match l.source {
                    tw_gateway::models::Source::Discovered => {
                        format!("{served} (upstream offers {}{scope})", l.models.len())
                    }
                    tw_gateway::models::Source::Manual => {
                        format!("{served} (manual list of {}{scope})", l.models.len())
                    }
                    tw_gateway::models::Source::None => match &l.error {
                        Some(why) => format!("unknown: {why}"),
                        None => "unknown".to_string(),
                    },
                }
            },
            p.pricing.as_deref().unwrap_or("the default sheet"),
        );
    }

    // ---- 客户端条目（**只有名字和脱敏后的 key**）
    let _ = writeln!(
        out,
        "\n## Gateway keys ({})\n\n| Name | Key |\n|---|---|",
        cfg.clients.len()
    );
    for c in &cfg.clients {
        let _ = writeln!(out, "| {} | {} |", c.name, tw_secret::mask_secret(&c.key));
    }

    // ---- 各项防护：档位，以及和出厂不一样的那几处（只有规则名，不含正则 ——
    // 自定义规则的正则里可能写着公司内部的东西）
    let _ = writeln!(out, "\n## Security\n\n| | |\n|---|---|");
    let changes = |enable: &[String], disable: &[String], custom: Vec<&str>| {
        let mut parts = Vec::new();
        if !enable.is_empty() {
            parts.push(format!("on: {}", enable.join(" ")));
        }
        if !disable.is_empty() {
            parts.push(format!("off: {}", disable.join(" ")));
        }
        if !custom.is_empty() {
            parts.push(format!("custom: {}", custom.len()));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join("; "))
        }
    };
    let r = &cfg.security.redact;
    line(
        &mut out,
        "Outbound redaction",
        format!(
            "{}{}",
            r.mode.label(),
            changes(
                &r.enable,
                &r.disable,
                r.custom.iter().map(|c| c.name.as_str()).collect()
            )
        ),
    );
    let t = &cfg.security.inspect_tools;
    line(
        &mut out,
        "Tool-call inspection",
        format!(
            "{}{}",
            t.mode.label(),
            changes(
                &t.enable,
                &t.disable,
                t.custom.iter().map(|c| c.name.as_str()).collect()
            )
        ),
    );
    let h = &cfg.security.hidden_text;
    line(
        &mut out,
        "Hidden characters",
        format!("{}{}", h.mode.label(), changes(&[], &h.disable, Vec::new())),
    );
    let c = &cfg.security.content;
    line(
        &mut out,
        "Content filter",
        format!(
            "{}{}",
            c.mode.label(),
            changes(
                &c.enable,
                &c.disable,
                c.custom.iter().map(|c| c.name.as_str()).collect()
            )
        ),
    );
    let o = &cfg.security.output_limit;
    line(
        &mut out,
        "Output limit",
        format!("{} ({} characters)", o.mode.label(), o.max_chars),
    );

    // ---- 存储和最近的失败
    match &s.store {
        None => {
            let _ = writeln!(
                out,
                "\n## Request recording\n\nNot running, so nothing from this period was recorded."
            );
        }
        Some(store) => {
            let g = store.lock().await;
            let _ = writeln!(out, "\n## Request recording\n\n| | |\n|---|---|");
            line(&mut out, "Requests recorded", g.db().count().unwrap_or(0));
            line(
                &mut out,
                "Request bodies on disk",
                format!("{} bytes", g.blobs().total_bytes()),
            );

            let recent = g.db().recent(None, 200).unwrap_or_default();
            let failed: Vec<_> = recent
                .iter()
                .filter(|r| r.error.is_some())
                .take(20)
                .collect();
            let _ = writeln!(
                out,
                "\n### Recent failures ({} of the last {} requests)\n",
                failed.len(),
                recent.len()
            );
            if failed.is_empty() {
                let _ = writeln!(out, "None.");
            } else {
                let _ = writeln!(
                    out,
                    "| Time | Upstream | Status | Error |\n|---|---|---|---|"
                );
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
                        tw_secret::mask_body(
                            r.error.as_ref().map(|e| e.text.as_str()).unwrap_or("")
                        ),
                    );
                }
            }
        }
    }

    // ---- 配置原文（脱敏后）
    let _ = writeln!(out, "\n## config.yaml (redacted)\n\n```yaml");
    match std::fs::read_to_string(s.config_path()) {
        // **`mask_body` 一个人不够。**它认的是值的形状，而配置里有一
        // 类密钥没有形状：自建中转那把普通样子的 key、OAuth 的 refresh
        // token。实测它们原样穿过去了 —— 而这一段的上面就写着「已脱敏」。
        //
        // `mask_config_yaml` 按**字段名**打（schema 就是答案），里面照旧
        // 叠一层 `mask_body`：仔细的那一处不能被另一处抵消。
        //
        // 控制面的钥匙**整个换掉**，不是留头留尾：它是那扇门的全部凭据。
        Ok(text) => {
            let text = tw_config::control_key::mask(&text);
            let _ = writeln!(out, "{}", tw_secret::mask_config_yaml(&text));
        }
        Err(e) => {
            let _ = writeln!(out, "# could not be read: {e}");
        }
    }
    let _ = writeln!(out, "```");

    let _ = writeln!(
        out,
        "\n---\n\nThis file carries no request or response bodies. They are in the request detail view."
    );
    Ok(out)
}
