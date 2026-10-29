//! 逐条扫描的老写法，**只给对照测试用**：两段式扫描（`engine`）改的只是「在哪些 token 上跑
//! 判据」，判据本身两边共用。对照测试（`tests`）拿同一份语料比两种扫法的结果，一处不同都不行。
//!
//! 这里的代码是两段式扫描之前 `rules::scan_unresolved` 和 `custom_hits` 的原样；判据改了不用
//! 动这里，改了「哪些 token 要看」才要。

use super::*;

/// 老写法的 [`super::scan`]
pub(crate) fn scan(text: &str, set: &RuleSet) -> Vec<Hit> {
    resolve(scan_unresolved(text, set), set)
}

pub(crate) fn scan_unresolved(text: &str, set: &RuleSet) -> Vec<Hit> {
    let mut out = Vec::new();
    if set.is_empty() {
        return out;
    }
    // **个人号码只在 JSON 的字符串里找**（见下面），私钥块不跨出它所在的那个字符串。不是
    // JSON 的文本（直接拿来扫的一段话）不分里外
    let json = json_like(text);
    // 保存的凭据最先找：同一处另一条规则也认得出时（一把保存的 `sk-ant-…`、连接串里的代理
    // 口令），报成哪一项保存的凭据说得最清楚（重叠的合成一处时，最先记下的说了算）
    if let Some(saved) = &set.saved
        && set.is_on("thinkwatch-saved-secret")
    {
        saved.find(text, &mut out);
    }
    if set.is_on("private-key") {
        private_keys(text, json, &mut out);
    }
    if set.is_on("conn-string-password") {
        conn_strings(text, &mut out);
    }
    if set.is_on("email") {
        emails(text, &mut out);
    }
    // 每个 token 都要问的几条开没开，扫之前问一次：在每个 token 上查一遍表，光哈希就占了
    // 整遍扫描的两成
    let want_openai = set.is_on("openai-api-key");
    let want_ip = set.is_on("internal-ip");
    let want_domain = set.is_on("internal-domain");
    let want_jwt = set.is_on("jwt");
    let want_id = set.is_on("cn-resident-id");
    let want_card = set.is_on("bank-card");
    let want_phone = set.is_on("cn-mobile-phone");
    // 一次扫描问一次时钟。只开卡号那条时也要问：认出是身份证号的不当卡号换
    let today = if want_id || want_card { today_ymd() } else { 0 };
    // **个人号码只在 JSON 的字符串里找。**字符串外面的数字是 JSON 的数值：工具
    // 调用的参数（`"input":{"order_id":6222…}`）、`seed`。凭据规则在那儿天然认
    // 不出东西，号码规则却认得出 —— 而把一个数值换成占位符，整个请求体就不是
    // JSON 了
    let mut spaced = Spaced::default();
    // 扩充的两组规则各有自己的一份「开没开、扫到哪了」，见各自的模块
    let mut secrets = super::super::secrets::Want::new(set);
    let mut personal = super::super::personal::Want::new(set);
    secrets.text(text, json, &mut out);
    personal.text(text, json, None, &mut out);
    for_each_token(text, |tok, span, quoted| {
        secrets.token(text, tok, span.clone(), &mut out);
        personal.token(text, tok, span.clone(), quoted, json, &mut out);
        if want_phone && (quoted || !json) {
            mobile_phones(tok, span.start, &mut out);
        }
        if (want_id || want_card) && (quoted || !json) {
            // 句末的句号不是号码的一部分：`…卡号是 6222 0212 3456 7894.`
            let body = tok.trim_end_matches('.');
            if want_card {
                spaced.feed(text, span.start, body, body.len() < tok.len(), &mut out);
            }
            personal_token(body, span.start, want_id, want_card, today, &mut out);
        } else if want_card {
            // 字符串外面的东西把在接的那一串截断
            spaced.finish(text, &mut out);
        }
        if let Some(id) = classify_token(tok, set, want_openai) {
            out.push(builtin_hit(id, span));
            return;
        }
        if want_jwt && looks_like_jwt(tok) {
            out.push(builtin_hit("jwt", span));
            return;
        }
        if let Some(id) = internal_token(tok, want_ip, want_domain) {
            out.push(builtin_hit(id, span));
        }
    });
    spaced.finish(text, &mut out);
    secrets.finish(text, &mut out);
    personal.finish(text, &mut out);
    custom_hits(text, set, json, &mut out);
    out
}

pub(super) fn custom_hits(text: &str, set: &RuleSet, json: bool, out: &mut Vec<Hit>) {
    if set.custom.is_empty() {
        return;
    }
    // 是 JSON 的话（请求体、编成 JSON 字符串的一段正文）**一个字符串一个字符串地找**：
    // 用户的正则（`\S+`）在整段原文上会从 `{`、`:` 这些结构上的字开始，一路吃过引号 ——
    // 换掉结构上的字，请求体就不是 JSON 了；而它吃掉的那一段里真正该换的字也就没换
    let spans = if json {
        json_string_spans(text)
    } else {
        std::iter::once(0..text.len()).collect()
    };
    for c in &set.custom {
        for span in &spans {
            let Some(piece) = text.get(span.clone()) else {
                continue;
            };
            for m in c.re.find_iter(piece) {
                let m = span.start + m.start()..span.start + m.end();
                if let Some(bytes) = json_safe(text, m) {
                    out.push(Hit {
                        bytes,
                        rule: Rule::Custom(c.name.clone()),
                        label: c.label.clone(),
                    });
                }
            }
        }
    }
}

/// 逐字节找的老写法：和 [`super::json_string_spans`] 比
pub(super) fn json_string_spans(text: &str) -> Vec<Range<usize>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'"' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < b.len() && b[j] != b'"' {
            j += if b[j] == b'\\' { 2 } else { 1 };
        }
        let end = j.min(b.len());
        out.push(start..end);
        i = end + 1;
    }
    out
}

mod tests;
