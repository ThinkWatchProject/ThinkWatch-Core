//! 扩充的个人信息规则：证件、境外证件、金融账户、联系方式、住址与位置、车辆、设备、按字段名认的。
//!
//! 和 [`super::rules`] 同一套纪律：**宁可漏，不可吵**。能在结构上核对的（校验位、日期、查表）
//! 才认；形状太普通、又没有校验位的号码（护照号、通行证、日本个人番号、IMEI）还要前 20 个
//! 字里写明了是什么（[`near`]）。**号码只在 JSON 字符串里找**：字符串外面的是 JSON 的数值，
//! 换成占位符请求体就坏了。
//!
//! 分几个文件：
//!
//! - [`ids`]：证件号码（15 位旧身份证号、护照机读区、护照号、港澳台身份证、通行证）和境外
//!   证件号码；
//! - [`contact`]：IBAN、手机号的几种写法、固定电话、国际电话；
//! - [`place`]：详细地址、精确坐标、车牌号；
//! - [`device`]：车辆识别代号、IMEI、MAC 地址、公网 IP（和内网地址的 IPv6 那一半）；
//! - [`labeled`]：按字段名认的（姓名、住址、出生日期、账号、健康）；
//! - [`data`]：查的那几张表（行政区划、长途区号、IBAN 各国长度、国际电话国家码）。

mod contact;
mod data;
mod device;
mod ids;
mod labeled;
mod place;

use std::ops::Range;

use super::common;
use super::rules::{Hit, RuleSet, builtin_hit};

pub(super) use data::nationality;
pub(super) use labeled::{
    ACCOUNT_FIELDS, ADDRESS_FIELDS, BIRTH_FIELDS, HEALTH_FIELDS, NAME_FIELDS,
};

/// 扫之前问一次哪几条开着：每个 token 上查一遍表太贵（见 `rules::scan_unresolved`）
#[derive(Default, Clone, Copy)]
struct On {
    id15: bool,
    mrz: bool,
    cn_passport: bool,
    hmt: bool,
    permit: bool,
    ssn: bool,
    nino: bool,
    sg: bool,
    kr: bool,
    jp: bool,
    aadhaar: bool,
    cpf: bool,
    iban: bool,
    phone: bool,
    landline: bool,
    intl: bool,
    address: bool,
    geo: bool,
    plate: bool,
    vin: bool,
    imei: bool,
    mac: bool,
    public_ip: bool,
    internal_ip: bool,
    labeled: [bool; labeled::RULES.len()],
}

impl On {
    fn of(set: &RuleSet) -> Self {
        let mut labeled = [false; labeled::RULES.len()];
        for (on, r) in labeled.iter_mut().zip(labeled::RULES) {
            *on = set.is_on(r.id);
        }
        On {
            id15: set.is_on("cn-resident-id-15"),
            mrz: set.is_on("passport-mrz"),
            cn_passport: set.is_on("cn-passport"),
            hmt: set.is_on("hk-macau-taiwan-id"),
            permit: set.is_on("mainland-travel-permit"),
            ssn: set.is_on("us-ssn"),
            nino: set.is_on("uk-nino"),
            sg: set.is_on("sg-nric"),
            kr: set.is_on("kr-rrn"),
            jp: set.is_on("jp-my-number"),
            aadhaar: set.is_on("in-aadhaar"),
            cpf: set.is_on("br-cpf"),
            iban: set.is_on("iban"),
            phone: set.is_on("cn-mobile-phone"),
            landline: set.is_on("cn-landline"),
            intl: set.is_on("intl-phone"),
            address: set.is_on("cn-address"),
            geo: set.is_on("geo-coordinates"),
            plate: set.is_on("cn-license-plate"),
            vin: set.is_on("vin"),
            imei: set.is_on("imei"),
            mac: set.is_on("mac-address"),
            public_ip: set.is_on("public-ip"),
            internal_ip: set.is_on("internal-ip"),
            labeled,
        }
    }

    /// 分组写、跨几个 token 的号码（`138 1234 5678`、`2345 6789 0123`、`AB 12 34 56 C`）
    fn spaced(&self) -> bool {
        self.phone || self.aadhaar || self.jp || self.nino
    }

    /// 一个 token 自己就认得出的
    fn single(&self) -> bool {
        self.id15
            || self.cn_passport
            || self.hmt
            || self.permit
            || self.ssn
            || self.nino
            || self.sg
            || self.kr
            || self.jp
            || self.aadhaar
            || self.cpf
            || self.phone
            || self.landline
            || self.vin
            || self.imei
            || self.mac
            || self.public_ip
    }
}

/// 这一组规则在一次扫描里的状态：扫之前问一次哪几条开着，扫的过程中攒着跨 token 的东西。
pub(super) struct Want {
    on: On,
    /// 有没有一条要看 token 的开着。出厂只开 IBAN 这一条
    tokens: bool,
    /// IBAN 以外还有要看 token 的开着
    rest: bool,
    single: bool,
    today: u32,
    iban: contact::IbanRun,
    groups: contact::Groups,
    landline: contact::LandlineParen,
    mac: device::MacRun,
    geo: place::GeoChain,
    plate: place::PlatePending,
}

impl Want {
    pub(super) fn new(set: &RuleSet) -> Self {
        let on = On::of(set);
        let single = on.single();
        Want {
            tokens: single || on.iban || on.spaced() || on.mac || on.geo || on.plate,
            rest: single || on.spaced() || on.mac || on.geo || on.plate,
            single,
            today: if on.id15 || on.kr {
                super::rules::today_ymd()
            } else {
                0
            },
            on,
            iban: contact::IbanRun::default(),
            groups: contact::Groups::default(),
            landline: contact::LandlineParen::default(),
            mac: device::MacRun::default(),
            geo: place::GeoChain::default(),
            plate: place::PlatePending::default(),
        }
    }

    /// 整段文本过一遍的判据（跨 token 的、按位置找的）
    pub(super) fn text(&mut self, text: &str, json: bool, out: &mut Vec<Hit>) {
        let on = &self.on;
        if on.mrz {
            ids::mrz(text, out);
        }
        if on.address {
            place::addresses(text, out);
        }
        if on.intl {
            contact::intl_phones(text, out);
        }
        if on.public_ip || on.internal_ip {
            device::ipv6(text, on.public_ip, on.internal_ip, out);
        }
        if on.labeled.iter().any(|&b| b) {
            labeled::scan(text, json, &on.labeled, out);
        }
    }

    /// 每个 token 一次。`quoted`：在 JSON 字符串里面（`json` 为真时才有意义）
    pub(super) fn token(
        &mut self,
        text: &str,
        tok: &str,
        span: Range<usize>,
        quoted: bool,
        json: bool,
        out: &mut Vec<Hit>,
    ) {
        if !self.tokens {
            return;
        }
        if json && !quoted {
            // 字符串外面的东西把在接的几串都截断
            self.iban.reset();
            if self.rest {
                self.finish(text, out);
            }
            return;
        }
        // 出厂只开着 IBAN 这一条要看 token：在接着一串的，或者头两个字节是大写字母的才往下看
        let b = tok.as_bytes();
        if self.on.iban
            && (self.iban.active()
                || (b.len() >= 4 && b[0].is_ascii_uppercase() && b[1].is_ascii_uppercase()))
        {
            self.iban
                .feed(text, tok.trim_end_matches('.'), span.start, out);
        }
        if !self.rest {
            return;
        }
        // 句末的句号不是号码的一部分
        let body = tok.trim_end_matches('.');
        if body.is_empty() {
            return;
        }
        let at = span.start;
        let on = &self.on;
        if on.spaced() {
            self.groups.feed(text, body, at, on, out);
        }
        if on.landline {
            self.landline.feed(text, body, at, out);
        }
        if on.mac {
            self.mac.feed(text, body, at, out);
        }
        if on.geo {
            self.geo.feed(text, body, at, out);
        }
        if on.plate {
            self.plate.feed(text, body, at, out);
        }
        if self.single {
            single(text, body, at, json, on, self.today, out);
        }
    }

    /// 扫完了：攒着没交出去的交出去
    pub(super) fn finish(&mut self, text: &str, out: &mut Vec<Hit>) {
        self.iban.reset();
        if !self.rest {
            return;
        }
        self.groups.finish(text, &self.on, out);
        self.landline = contact::LandlineParen::default();
        self.mac.finish(text, out);
        self.geo.finish(text, out);
        self.plate = place::PlatePending::default();
    }
}

/// 一个 token 自己就认得出的那些。`body` 去掉了句末句号，从 `at` 开始。
fn single(text: &str, body: &str, at: usize, json: bool, on: &On, today: u32, out: &mut Vec<Hit>) {
    let b = body.as_bytes();
    let span = at..at + b.len();
    let push = |out: &mut Vec<Hit>, id: &'static str| out.push(builtin_hit(id, span.clone()));
    if b[0].is_ascii_digit() {
        let digits = b.iter().all(u8::is_ascii_digit);
        if digits {
            match b.len() {
                15 if on.id15 && ids::resident_id_15(b, today) => push(out, "cn-resident-id-15"),
                15 if on.imei && super::rules::luhn(b) && near(text, at, json, IMEI_WORDS) => {
                    push(out, "imei")
                }
                _ => {}
            }
            if on.jp && b.len() == 12 && ids::my_number(b) && near(text, at, json, ids::JP_WORDS) {
                push(out, "jp-my-number");
            }
            if on.aadhaar && b.len() == 12 && ids::aadhaar(b) {
                push(out, "in-aadhaar");
            }
            if on.permit && ids::taiwan_permit(b) && near(text, at, json, ids::TAIWAN_PERMIT_WORDS)
            {
                push(out, "mainland-travel-permit");
            }
            if on.hmt && ids::macau_id(text, at, b) && near(text, at, json, ids::MACAU_WORDS) {
                // 括号里的校验位一起换
                out.push(builtin_hit("hk-macau-taiwan-id", at..at + b.len() + 3));
            }
        } else {
            if on.ssn && ids::us_ssn(b) {
                push(out, "us-ssn");
            }
            if on.kr && ids::kr_rrn(b, today) {
                push(out, "kr-rrn");
            }
            if on.cpf && ids::br_cpf(b) {
                push(out, "br-cpf");
            }
            if (on.jp || on.aadhaar) && b.len() == 14 {
                if let Some(d) = contact::grouped_4_4_4(b) {
                    if on.jp && ids::my_number(&d) && near(text, at, json, ids::JP_WORDS) {
                        push(out, "jp-my-number");
                    }
                    if on.aadhaar && ids::aadhaar(&d) {
                        push(out, "in-aadhaar");
                    }
                }
            }
            if on.public_ip && device::public_ip_token(text, at, body, json) {
                push(out, "public-ip");
            }
        }
        if on.phone {
            contact::mobile_written(text, body, at, out);
        }
        if on.landline {
            contact::landline_dashed(body, at, out);
        }
    } else if b[0].is_ascii_uppercase() {
        if on.cn_passport && ids::cn_passport(b) && near(text, at, json, ids::PASSPORT_WORDS) {
            push(out, "cn-passport");
        }
        if on.hmt {
            if let Some(end) = ids::hk_id(text, at, b) {
                out.push(builtin_hit("hk-macau-taiwan-id", at..end));
            } else if ids::taiwan_id(b) {
                push(out, "hk-macau-taiwan-id");
            }
        }
        if on.permit && ids::hm_permit(b) && near(text, at, json, ids::HM_PERMIT_WORDS) {
            push(out, "mainland-travel-permit");
        }
        if on.nino && ids::uk_nino(b) {
            push(out, "uk-nino");
        }
        if on.sg && ids::sg_nric(b) {
            push(out, "sg-nric");
        }
    }
    if on.vin && device::vin(b) {
        push(out, "vin");
    }
    if on.mac && device::mac_dashed(b) {
        push(out, "mac-address");
    }
}

const IMEI_WORDS: &[&str] = &["imei"];

/// `at` 前面 20 个字符里有没有其中一个关键词（[`common::preceded_by`]）。
///
/// JSON 里的中文可能写成 `\uXXXX`（Python 写的客户端默认这样），`护照` 在原文里是
/// `护照`：先把那一小段解开再找，不然这种请求里的关键词一个都认不出
fn near(text: &str, at: usize, json: bool, words: &[&str]) -> bool {
    // 20 个字符写成 `\uXXXX` 最多 120 个字节
    let mut from = at.saturating_sub(160);
    while !text.is_char_boundary(from) {
        from += 1;
    }
    let head = &text[from..at];
    if !json || !head.contains('\\') {
        return common::preceded_by(text, at, WINDOW, words);
    }
    let decoded = unescape(head);
    common::preceded_by(&decoded, decoded.len(), WINDOW, words)
}

/// 关键词要在号码前面几个字符以内
const WINDOW: usize = 20;

/// 解开一小段 JSON 字符串里的转义。解不开的照抄
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None => out.push(' '),
                }
            }
            Some('n' | 'r' | 't') => out.push('\n'),
            Some(c) => out.push(c),
            None => {}
        }
    }
    out
}

/// `text[..at]` 是不是以一个 JSON 转义结尾（`\n`、`\"`、`\uXXXX`），是的话它有几个字节。
///
/// 往回找键名、往两边扩一段号码时用：行首的 `\nname: …` 里那个 `n` 是转义的一部分，不是
/// 键名的头一个字
fn escape_before(b: &[u8], at: usize) -> usize {
    let odd = |i: usize| {
        // b[i] 是反斜杠，它前面连着几个反斜杠
        let mut n = 0;
        while n < i && b[i - 1 - n] == b'\\' {
            n += 1;
        }
        n % 2 == 0
    };
    if at >= 6
        && b[at - 6] == b'\\'
        && b[at - 5] == b'u'
        && b[at - 4..at].iter().all(u8::is_ascii_hexdigit)
        && odd(at - 6)
    {
        return 6;
    }
    if at >= 2 && b[at - 2] == b'\\' && odd(at - 2) {
        return 2;
    }
    0
}

/// 打码：个人信息里不按「留最后四位」的那几条。`None` 照通常的办
pub(super) fn masked(id: &str, value: &str) -> Option<String> {
    match id {
        "passport-mrz" => Some(ids::mask_mrz(value)),
        "cn-address" | "labeled-address" => Some(match place::county_prefix(value) {
            Some(p) => format!("{p}…"),
            None => "…".to_string(),
        }),
        "labeled-name" => Some(match value.trim().chars().next() {
            Some(c) => format!("{c}…"),
            None => "…".to_string(),
        }),
        "geo-coordinates" => Some(place::mask_coordinate(value)),
        "labeled-health" | "labeled-birth-date" => Some("…".to_string()),
        _ => None,
    }
}

#[cfg(test)]
pub(super) mod tests;
