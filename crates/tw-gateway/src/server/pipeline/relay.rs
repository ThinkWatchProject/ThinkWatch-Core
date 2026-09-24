//! 管线第 5 步的后半：把选中那一家的响应交给客户端。
//!
//! 流式：**不缓冲**。整块缓冲会把 SSE 变成一次性交付，客户端那边看起来
//! 就是「卡住很久然后一下全出来」。每一块依次经过：留档与嗅探 → 回显
//! 还原 → 格式转换 → 工具调用审查，然后才写给客户端。
//!
//! 这些步骤要不要做、怎么做，在响应头到手的那一刻就全定了（[`Plan`]）；
//! 流里每一块怎么处理在 [`Relay`] 上，`respond` 里的流只剩一个循环。

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;

use super::Inbound;
use super::hop::Served;
use crate::error::GatewayError;
use crate::forward;
use crate::server::{because, flagged};
use crate::state::{AppState, Runtime};
use tw_types::msg;

#[allow(clippy::too_many_arguments)]
pub(super) fn respond(
    state: &AppState,
    rt: &Runtime,
    req: &Inbound,
    generates: bool,
    served: Served<'_>,
    id: u64,
    live: crate::live::Pass,
    mut ending: crate::ending::Ending,
) -> Response {
    let Served {
        upstream,
        provider,
        ledger,
        session,
    } = served;
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    // 响应头到手就发一次。**这个事件单独存在是有意的**：流式请求从这里
    // 到结束可能还有好几分钟，UI 要能在这个点就把行画出来并标「进行中」，
    // 而不是等它结束才出现。
    let ttfb_ms = req.started.elapsed().as_millis() as u64;
    // `url-test` 的判据。**只记成功的那些** —— 一个 500 在
    // 十毫秒内返回，会让最坏的上游看起来最快
    if status.is_success() {
        state
            .latency
            .record(&provider.name, ttfb_ms.min(u32::MAX as u64) as u32);
    }
    state.bus.emit(tw_api::Event::RequestHeaders {
        id,
        status: status.as_u16(),
        ttfb_ms,
    });
    // 订阅额度。**零成本** —— 这些头本来就在响应里，读一下
    // 就有了。按量付费的账号没有它们，那时什么都不发。
    state.note_quota(id, &provider.name, upstream.headers());
    // 上游收不收我们的凭据、经过的代理通不通：**都是状态变化，各只报一次**
    state.note_auth(&provider.name, status.as_u16());
    state.note_proxy_ok(&provider.proxy);

    let mut out_headers = forward::response_headers(upstream.headers());
    // **哪一家服务的，写在头上。**错误契约要求上游的错误原样透传、不加
    // `[ThinkWatch]` 前缀 —— 那确实是它说的话。可上游的 401 说的是
    // 「invalid x-api-key」，而用户手里有两把 key（网关的和上游的），
    // 他会去查错的那一把。改 body 是越界，加一个头不是。
    if let Ok(v) = axum::http::HeaderValue::from_str(&provider.name) {
        out_headers.insert("x-thinkwatch-upstream", v);
    }

    // 流结束时才知道总字节数和真实耗时 —— 对一个跑了六分钟的任务，
    // 这两个数字在响应头那一刻都还不存在。
    //
    // **结局跟着流走，而不是只写在流的末尾。**客户端中途走掉时，末尾的
    // 代码一行都不会执行，而上游已经为这次请求计了费（见 `crate::ending`）。
    ending.responded(status.as_u16());
    let plan = Plan::new(
        req,
        generates,
        provider,
        status,
        &mut out_headers,
        session.as_ref(),
    );
    let mut relay = Relay::new(state, rt, req, plan, &ledger, session, provider, id);
    let chunks = upstream.bytes_stream();
    let dialect = req.dialect;
    let stream = async_stream::stream! {
        // **通行证跟着响应体走。**这个流被丢掉的时候它才还回去：正常
        // 发完是一种，客户端中途断开、hyper 丢掉响应体是另一种 —— 两种
        // 都算这个请求结束了。
        let _live = live;
        // 结局也一样：流被丢掉的时候，它替流报「客户端取消」。
        let mut ending = ending;
        let mut chunks = std::pin::pin!(chunks);
        let mut broke: Option<GatewayError> = None;
        while let Some(item) = chunks.next().await {
            match item {
                Ok(chunk) => {
                    // **旁路嗅探和留档，不缓冲**：字节照常流向客户端，同时
                    // 喂它一份。上游返回的 usage 是真相，而拿不到它就只能估。
                    //
                    // 看的是上游原话（带占位符的那一版）：usage 数字不受
                    // 影响，而请求详情里存的正是「我们发出去的和收回来的」，
                    // 把还原后的存进去会让那一页说谎。
                    ending.feed(&chunk);
                    let (out, cut) = relay.chunk(&chunk);
                    if !out.is_empty() {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(out));
                    }
                    if let Some(err) = cut {
                        broke = Some(err.in_dialect(dialect));
                        break;
                    }
                }
                Err(e) => {
                    broke = Some(forward::map_reqwest_error(e).in_dialect(dialect));
                    break;
                }
            }
        }
        let (tail, denied) = relay.finish(broke.is_some());
        if !tail.is_empty() {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(tail));
        }
        // 扣下整份 body 和流断在半路，对结局来说是同一件事
        if let Some(err) = denied {
            broke = Some(err.in_dialect(dialect));
        }
        // 响应体留档和结束事件都在 `ending` 里：三种结局要交出去的是同一份
        // 东西，分开写就会有一种漏掉
        match broke {
            None => ending.finished(status.as_u16()),
            Some(err) => {
                // 少了这个事件，UI 上那一行会永远停在「进行中」——
                // 而「一直转圈」比「明确失败」更让人怀疑是我们卡住了。
                //
                // **先报再发错误帧**：客户端恰好在最后这一帧上走掉的话，
                // 结局已经报过了，不会再被记成一次取消。
                //
                // `source` 用这个错误自己的：上游断了是 `upstream`，被
                // 防火墙切断是 `denied` —— 后者不是上游坏了，是策略拦的。
                // 码保持不变，只在句子前面点明它断在流里 —— 界面认的是码
                let mut why = err.detail.clone();
                why.text = format!("the response stream broke: {}", why.text);
                ending.failed(err.source.slug(), why);
                if let Some(frame) = relay.error_tail(&err) {
                    yield Ok(Bytes::from(frame));
                }
            }
        }
    };
    let mut resp = Response::new(Body::from_stream(stream));
    *resp.status_mut() = status;
    *resp.headers_mut() = out_headers;
    resp
}

/// 响应头到手时就定下的处理方式。
#[derive(Clone, Copy)]
struct Plan {
    status: StatusCode,
    /// 上游回的是 SSE
    is_sse: bool,
    /// 客户端要整包、上游给的是流：**收齐之后写一个客户端格式的整包。**以前
    /// 这种情况把转换出来的流标成 `application/json` 发过去，客户端解析不了
    collect: bool,
    /// 流式成功的边收边转
    convert_stream: bool,
    /// 整包、以及上游返回的错误，整个到手再转 —— 上游的错误体要换成客户端
    /// 认得的错误格式，否则客户端连原因都解析不出来
    convert_whole: bool,
    /// 客户端收到的是不是 SSE。Gemini 客户端不带 `alt=sse` 时是一个 JSON 数组
    client_sse: bool,
    /// 客户端收到的是不是那个 JSON 数组流。**它也是边收边发的**：以前工具调用
    /// 审查只认 SSE，不带 `alt=sse` 的 Gemini 客户端收到的工具调用一个都没查过
    client_json_stream: bool,
}

impl Plan {
    /// 顺带把响应头上的 Content-Type 改成客户端将要收到的样子。
    fn new(
        req: &Inbound,
        generates: bool,
        provider: &tw_config::Provider,
        status: StatusCode,
        out_headers: &mut HeaderMap,
        session: Option<&tw_dialect::convert::Session>,
    ) -> Self {
        // **中途断掉不能只是让流消失。**首字节已经发出去了，状态码和响应头
        // 都改不了，而一个戛然而止的 SSE 流和一个正常结束的流在客户端看来
        // 长得一模一样 —— 用户会以为模型就答了这么多。唯一还能说话的地方
        // 是流本身，所以补一个 `event: error` 帧。
        //
        // **Codex 后端的流式响应没有 Content-Type**（实测）。发给它的请求一定是流式的，所以
        // 成功的响应就是 SSE；不补上的话，转换、回显还原和工具调用审查都会当成整包处理。
        if generates
            && provider.effective_protocol() == Some(tw_config::Protocol::Chatgpt)
            && status.is_success()
            && !out_headers.contains_key(axum::http::header::CONTENT_TYPE)
        {
            out_headers.insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/event-stream"),
            );
        }
        let is_sse = out_headers
            .get(axum::http::header::CONTENT_TYPE)
            .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
        let collect = session.is_some_and(|s| !s.stream) && is_sse && status.is_success();
        let convert_stream = session.is_some() && is_sse && status.is_success() && !collect;
        let convert_whole = session.is_some() && !convert_stream && !collect;
        if let Some(s) = session {
            // 客户端要流而上游给了整包时，整包会被写成客户端格式的流
            let writes_stream = convert_stream || (status.is_success() && s.stream);
            let ct = if writes_stream {
                s.content_type()
            } else {
                "application/json"
            };
            out_headers.insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static(ct),
            );
        }
        let client_sse = session.map_or(is_sse, |s| convert_stream && s.client_sse());
        let client_json_stream = status.is_success()
            && match session {
                Some(s) => convert_stream && !s.client_sse(),
                None => {
                    !is_sse
                        && req.api == Some(crate::client_api::ClientApi::Gemini)
                        && req
                            .uri
                            .path()
                            .trim_end_matches('/')
                            .ends_with(":streamGenerateContent")
                }
            };
        Self {
            status,
            is_sse,
            collect,
            convert_stream,
            convert_whole,
            client_sse,
            client_json_stream,
        }
    }

    /// 客户端要的是一整份 body（不是流）
    fn whole_body(&self) -> bool {
        !self.client_sse && !self.client_json_stream
    }
}

/// 流里每一块经过的那几道工序，和它们攒着的状态。
struct Relay {
    plan: Plan,
    session: Option<tw_dialect::convert::Session>,
    /// 回显还原。
    ///
    /// **SSE 和非流式走两套**：前者的占位符散落在几十帧里（模型按 token
    /// 吐字，一个 `<<TW_SECRET_1>>` 会被切成五到八段），后者整个躺在一份
    /// JSON 里。
    ///
    /// **没脱敏过就是个空壳**，`process` 直接把字节原样递出去 —— 绝大多数
    /// 请求走的是这条路，它不该为这个功能付任何延迟。
    restorer: tw_guard::redact::sse::Body,
    /// 流式转换器。**同格式时是 None，整段零成本。**
    ///
    /// 位置在还原**之后**：占位符是我们在出站时塞进去的，先换回真值再
    /// 转换，转换器看到的就和上游原话一样了。
    back: Option<tw_dialect::convert::StreamConverter>,
    /// 客户端要整包、上游给流时的收集器
    collector: Option<tw_dialect::convert::Collector>,
    /// 工具调用防火墙。
    ///
    /// **流式和非流式走两套，但两套都跑。**流式是边流边扫、命中就切，
    /// 立足点是「不完整的工具调用执行不了」；非流式没有这个立足点，
    /// 却有一个更强的条件 —— 整份 body 到手时一个字节都还没发出去，
    /// 所以整份看完再决定发不发。
    ///
    /// 以前非流式这一支根本不建审查器。代价有两条：观察档对非流式
    /// 客户端一条都不记（而界面上写的是「照常检测、照常记录」），
    /// 拦截档更是被整个绕过去。
    /// **对所有上游一样**：切不切只看档位和规则的处置，不看上游是不是官方的
    wall: Option<tw_guard::tools::wall::Wall>,
    inspect: tw_config::SecurityMode,
    /// 非流式要拦得住，body 就不能边收边发 —— 发出去了就收不回来。
    ///
    /// **这不是把流变成一次性交付**：客户端要的本来就是一整份 JSON，
    /// 它无论如何都得等完整 —— 它那边的 HTTP 栈同样要收齐才交给调用者。
    /// 整包转换（`convert_whole`）和整包收集（`collect`）本来就在攒，
    /// 只有剩下那条直通的路需要这一下。
    ///
    /// **不设大小上限。**设了就是一条绕过去的路：往响应里塞几 MB 无害
    /// 内容把体积顶过阈值，后面的工具调用就再也不会被看到了。而一次
    /// 非流式回答的体积由 `max_tokens` 封顶，十几万输出 token 也就几百
    /// KB —— 真正的风险不在这儿。整包转换那条路本来也是不封顶的。
    hold: bool,
    /// 整包那几条路攒着的 body
    whole: Vec<u8>,
    bus: tw_observe::EventBus,
    id: u64,
    provider: String,
}

impl Relay {
    #[allow(clippy::too_many_arguments)]
    fn new(
        state: &AppState,
        rt: &Runtime,
        req: &Inbound,
        plan: Plan,
        ledger: &tw_guard::redact::replace::Ledger,
        session: Option<tw_dialect::convert::Session>,
        provider: &tw_config::Provider,
        id: u64,
    ) -> Self {
        // 还原看的是**上游的原话**（转换之前），所以按上游的格式认帧
        let upstream_dialect = session
            .as_ref()
            .map(|s| s.upstream)
            .or_else(|| req.api.map(|a| a.dialect()))
            .unwrap_or(tw_dialect::ir::Dialect::Anthropic);
        let restorer = tw_guard::redact::sse::Body::new(ledger, plan.is_sse, upstream_dialect);
        let back = if plan.convert_stream {
            session.as_ref().map(|s| s.stream())
        } else {
            None
        };
        let collector = if plan.collect {
            session.as_ref().map(|s| s.collector())
        } else {
            None
        };
        let inspect = rt.config.security.inspect_tools.mode;
        let wall = if !inspect.detects() {
            None
        } else if plan.client_sse {
            Some(tw_guard::tools::wall::Wall::new(rt.tools.clone()))
        } else if plan.client_json_stream {
            Some(tw_guard::tools::wall::Wall::json_array(rt.tools.clone()))
        } else {
            Some(tw_guard::tools::wall::Wall::json_body(rt.tools.clone()))
        };
        let hold = wall.is_some() && plan.whole_body() && !plan.convert_whole && !plan.collect;
        Self {
            plan,
            session,
            restorer,
            back,
            collector,
            wall,
            inspect,
            hold,
            whole: Vec::new(),
            bus: state.bus.clone(),
            id,
            provider: provider.name.clone(),
        }
    }

    /// 处理上游的一块：返回现在该写给客户端的字节，以及防火墙切断时的那个错误。
    fn chunk(&mut self, chunk: &[u8]) -> (Vec<u8>, Option<GatewayError>) {
        let out = self.restorer.process(chunk);
        // 翻译在还原之后、审查之前：**审查看的必须是客户端
        // 将要拿到的那一版**，而那一版是翻译过的
        let out = match self.back.as_mut() {
            Some(c) => c.process(&out),
            None => out,
        };
        // 整包那一条整个攒起来，最后转一次。**这不是缓冲流** ——
        // 整包响应本来就是一整个 body，客户端无论如何都要等它完整
        // （说的是别把 SSE 变成一次性交付，这里没有 SSE）
        if self.plan.convert_whole || self.hold {
            self.whole.extend_from_slice(&out);
            return (Vec::new(), None);
        }
        if let Some(c) = self.collector.as_mut() {
            c.process(&out);
            return (Vec::new(), None);
        }
        // **审查的是客户端将要看到的那一版**（还原之后的），
        // 因为那才是它真正会去执行的东西
        let Some(w) = self.wall.as_mut() else {
            return (out, None);
        };
        for v in w.feed(&out) {
            // 规则是切断 + 拦截档 = 切断
            let blocked = v.cut && self.inspect.acts();
            self.bus.emit(flagged(self.id, &self.provider, &v, blocked));
            if blocked {
                tracing::warn!(
                    provider = %self.provider, tool = %v.tool, rule = %v.rule,
                    "cut the response stream: the upstream returned a dangerous tool call"
                );
                let err = GatewayError::denied(msg!(
                    "gw.toolcall.cut",
                    upstream = self.provider.clone(), tool = v.tool.clone(),
                    rule = v.rule.clone(), name = v.name.clone(), why = v.why.clone() =>
                    "The {tool} call returned by upstream `{upstream}` \
                     matched rule “{name}”{}, so the response was cut off.",
                    because(&v.why)
                ));
                // **命中那一帧之前的内容照常发。**模型在动手之前
                // 通常先说了几句正常的话，一起吞掉的话用户看到的
                // 是「什么都没发生然后报错了」。而从那一帧起一个
                // 字节都不发 —— 「尽力阻断」的要点是客户端拼不出
                // 完整的工具调用
                let safe = v.safe_prefix.min(out.len());
                return (out[..safe].to_vec(), Some(err));
            }
        }
        (out, None)
    }

    /// 流走完了（或者断了）：吐出扣住的尾巴，**在结束事件之前** —— 否则最后
    /// 几个字节会掉在流的外面。整包的那几条路在这里转换、收齐、审查。
    ///
    /// 返回要写给客户端的尾巴，和非流式审查扣下整份 body 时的那个错误。
    fn finish(&mut self, broke: bool) -> (Vec<u8>, Option<GatewayError>) {
        let status = self.plan.status;
        let tail = self.restorer.flush();
        let tail = match (&self.session, self.back.as_mut()) {
            (Some(s), None) if self.plan.convert_whole => {
                if broke {
                    // 半截的整包转不出任何有意义的东西，由下面的错误收尾
                    Vec::new()
                } else {
                    self.whole.extend_from_slice(&tail);
                    if !status.is_success() {
                        s.error(status.as_u16(), &self.whole)
                    } else if s.stream {
                        s.stream_from_whole(&self.whole)
                            .unwrap_or_else(|| self.whole.clone())
                    } else {
                        // 转不动就原样交给客户端 —— 那是上游的原话，比我们编的任何
                        // 东西都有用
                        s.response(&self.whole)
                            .unwrap_or_else(|| self.whole.clone())
                    }
                }
            }
            (Some(s), None) if self.plan.collect => match self.collector.take() {
                Some(mut c) if !broke => {
                    c.process(&tail);
                    match c.finish() {
                        Ok(body) => body,
                        // 上游在流里报了错：按客户端的格式说出来
                        Err(why) => tw_dialect::convert::error_body(s.client, 502, &why),
                    }
                }
                // 半截的流收不出完整的回答，由下面的错误收尾
                _ => Vec::new(),
            },
            // 攒着等整份看完的那条直通路：尾巴接上，整份交给下面
            _ if self.hold => {
                if broke {
                    // 半截的整包交不出去，由下面的错误收尾
                    Vec::new()
                } else {
                    self.whole.extend_from_slice(&tail);
                    std::mem::take(&mut self.whole)
                }
            }
            (_, Some(c)) => {
                let mut t = c.process(&tail);
                // **收尾必须补上**：客户端等着结束帧（Anthropic 的 message_stop、
                // Chat 的 [DONE]），少了会一直等。中途断了的由下面按错误收尾
                if !broke {
                    t.extend(c.finish());
                }
                t
            }
            _ => tail,
        };
        /*
          非流式：**整份到手了才看得见工具调用，而它一个字节都还没发出去。**

          流式那条路只能「尽力阻断」—— 首字节早发了，能做的是从命中的
          那一帧起不再发。这里不一样：要么整份发出去，要么一份都不发，
          所以拦得干净。代价是状态码已经随响应头走了，改不动 —— body
          里换成错误体，和 `sse_frame` 在流上扮演的是同一个角色。
        */
        if self.plan.whole_body()
            && !broke
            && status.is_success()
            && let Some(w) = self.wall.as_mut()
        {
            for v in w.whole(&tail) {
                let blocked = v.cut && self.inspect.acts();
                self.bus.emit(flagged(self.id, &self.provider, &v, blocked));
                if blocked {
                    tracing::warn!(
                        provider = %self.provider, tool = %v.tool, rule = %v.rule,
                        "withheld the response: the upstream returned a dangerous tool call"
                    );
                    let err = GatewayError::denied(msg!(
                        "gw.toolcall.blocked",
                        upstream = self.provider.clone(), tool = v.tool.clone(),
                        rule = v.rule.clone(), name = v.name.clone(), why = v.why.clone() =>
                        "The {tool} call returned by upstream `{upstream}` matched rule \
                         “{name}”{}, so the response was withheld.",
                        because(&v.why)
                    ));
                    return (Vec::new(), Some(err));
                }
            }
        }
        (tail, None)
    }

    /// 流断了之后还能对客户端说的最后一句：按它收到的格式收尾。
    fn error_tail(&mut self, err: &GatewayError) -> Option<Vec<u8>> {
        if let Some(c) = self.back.as_mut() {
            // 转换过的流按客户端的格式收尾
            Some(c.fail(&format!("[ThinkWatch] {}", err.message())))
        } else if let (true, Some(s)) = (self.plan.collect, self.session.as_ref()) {
            // 要收齐的整包一个字节都还没发：按客户端的格式回一个错误体
            Some(tw_dialect::convert::error_body(
                s.client,
                502,
                &format!("[ThinkWatch] {}", err.message()),
            ))
        } else if self.plan.is_sse && self.session.is_none() {
            Some(err.sse_frame().into_bytes())
        } else if self.hold {
            // 整份攒着的那条路：body 还没发，整个换成错误体
            Some(err.body_bytes())
        } else {
            None
        }
    }
}
