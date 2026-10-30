//! 测试样本：每条代码实现的凭据规则至少三个该认的、三个不该认的。
//!
//! **值全是在这里拼出来的假值**（伪随机串、自己算的校验和），不提交任何真的凭据。

use sha2::{Digest, Sha256};

/// 一条规则的样本：`hits` 是（一段文本，其中该被换掉的那个值），`misses` 是一个都不该认的
pub(crate) struct Sample {
    pub hits: Vec<(String, String)>,
    pub misses: Vec<String>,
}

/// 确定的伪随机串：同一个种子每次一样
pub(crate) fn rnd(seed: u64, n: usize, alphabet: &[u8]) -> String {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            alphabet[(x % alphabet.len() as u64) as usize] as char
        })
        .collect()
}

pub(crate) const HEX: &[u8] = b"0123456789abcdef";
pub(crate) const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const URLSAFE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz";

fn bytes(seed: u64, n: usize) -> Vec<u8> {
    rnd(seed, n * 2, HEX)
        .as_bytes()
        .chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
        .collect()
}

/// Base58 编码（比特币字母表）：和被测的解码各写一遍
pub(crate) fn base58_encode(data: &[u8]) -> String {
    const A: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut digits: Vec<u8> = Vec::new();
    for &byte in data {
        let mut carry = u32::from(byte);
        for d in digits.iter_mut() {
            carry += u32::from(*d) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let zeros = data.iter().take_while(|&&b| b == 0).count();
    "1".repeat(zeros)
        + &digits
            .iter()
            .rev()
            .map(|&d| A[d as usize] as char)
            .collect::<String>()
}

pub(crate) fn check_encode(payload: &[u8]) -> String {
    let sum = Sha256::digest(Sha256::digest(payload));
    let mut v = payload.to_vec();
    v.extend_from_slice(&sum[..4]);
    base58_encode(&v)
}

pub(crate) fn wif_of(seed: u64, version: u8, compressed: bool) -> String {
    let mut p = vec![version];
    p.extend(bytes(seed, 32));
    if compressed {
        p.push(0x01);
    }
    check_encode(&p)
}

pub(crate) fn xprv_of(seed: u64, version: [u8; 4]) -> String {
    let mut p = version.to_vec();
    p.extend(bytes(seed, 74));
    check_encode(&p)
}

/// 照 BIP39 从熵算出助记词的词序号：熵 + SHA-256 的头 `ENT/32` 位，每 11 位一个词
pub(crate) fn mnemonic_indices(entropy: &[u8]) -> Vec<u16> {
    let cs = entropy.len() * 8 / 32;
    let hash = Sha256::digest(entropy);
    let bit = |i: usize| {
        let byte = if i < entropy.len() * 8 {
            entropy[i / 8]
        } else {
            hash[(i - entropy.len() * 8) / 8]
        };
        (byte >> (7 - i % 8)) & 1
    };
    let total = entropy.len() * 8 + cs;
    (0..total / 11)
        .map(|w| (0..11).fold(0u16, |v, j| (v << 1) | u16::from(bit(w * 11 + j))))
        .collect()
}

pub(crate) fn english_phrase(entropy: &[u8]) -> String {
    let list = crate::redact::secrets::crypto::english();
    mnemonic_indices(entropy)
        .iter()
        .map(|&i| list[usize::from(i)])
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn chinese_phrase(entropy: &[u8], sep: &str) -> String {
    let list: Vec<&str> = crate::redact::secrets::crypto::CHINESE.lines().collect();
    mnemonic_indices(entropy)
        .iter()
        .map(|&i| list[usize::from(i)])
        .collect::<Vec<_>>()
        .join(sep)
}

fn same(v: String) -> (String, String) {
    (v.clone(), v)
}

/// 一条代码实现的凭据规则的样本。没有样本的规则是 `None`（测试会因此失败）
pub(crate) fn sample(id: &str) -> Option<Sample> {
    let s = match id {
        "zhipu-api-key" => {
            let key = |n| format!("{}.{}Q", rnd(n, 32, HEX), rnd(n + 1, 15, ALNUM));
            Sample {
                hits: vec![
                    same(key(1)),
                    (format!("ZHIPU_KEY 是 {}，别外传", key(2)), key(2)),
                    (format!("keys:\n{}\nnext", key(3)), key(3)),
                ],
                misses: vec![
                    // 扩展名全是小写
                    format!("{}.tarballxzipped01", rnd(4, 32, HEX)),
                    // 前面不是十六进制
                    format!(
                        "{}.{}Q",
                        rnd(5, 32, ALNUM).to_uppercase(),
                        rnd(6, 15, ALNUM)
                    ),
                    // 截断的
                    format!("{}.{}", rnd(7, 32, HEX), rnd(8, 10, ALNUM)),
                ],
            }
        }
        "qianfan-api-key" => {
            let key = |n| format!("bce-v3/ALTAK-{}/{}", rnd(n, 21, ALNUM), rnd(n + 1, 40, HEX));
            Sample {
                hits: vec![
                    same(key(1)),
                    (format!("\"api_key\": \"{}\"", key(2)), key(2)),
                    (format!("Authorization: Bearer {}", key(3)), key(3)),
                ],
                misses: vec![
                    format!("bce-v3/ALTAK-{}/{}", rnd(4, 20, ALNUM), rnd(5, 40, HEX)),
                    format!("xbce-v3/ALTAK-{}/{}", rnd(6, 21, ALNUM), rnd(7, 40, HEX)),
                    format!("bce-v3/ALTAK-{}/{}", rnd(8, 21, ALNUM), rnd(9, 30, HEX)),
                ],
            }
        }
        "sk-compat-key" => {
            let hex = |n| format!("sk-{}a1", rnd(n, 30, HEX));
            Sample {
                hits: vec![
                    same(hex(1)),
                    (format!("DEEPSEEK_KEY {}", hex(2)), hex(2)),
                    same(format!("sk-{}", rnd(3, 48, LOWER))),
                ],
                misses: vec![
                    format!("sk-{}", "a".repeat(32)),
                    format!("sk-{}", "1".repeat(32)),
                    format!("sk-{}", rnd(4, 31, HEX)),
                    format!("sk-{}", "ab".repeat(24)),
                    format!("xsk-{}a1", rnd(5, 30, HEX)),
                ],
            }
        }
        "azure-ad-client-secret" => {
            let key = |n| format!("{}8Q~{}", rnd(n, 3, ALNUM), rnd(n + 1, 34, ALNUM));
            Sample {
                hits: vec![
                    same(key(1)),
                    (format!("AZURE_CLIENT_SECRET={}", key(2)), key(2)),
                    (format!("secret: {}.", key(3)), key(3)),
                ],
                misses: vec![
                    format!("{}8Q~{}", rnd(4, 3, ALNUM), rnd(5, 20, ALNUM)),
                    format!("{}xQ~{}", rnd(6, 3, ALNUM), rnd(7, 34, ALNUM)),
                    "~/Q~/home path".to_string(),
                ],
            }
        }
        "terraform-token" => {
            let key = |n| format!("{}.atlasv1.{}", rnd(n, 14, ALNUM), rnd(n + 1, 67, URLSAFE));
            Sample {
                hits: vec![
                    same(key(1)),
                    (format!("token = \"{}\"", key(2)), key(2)),
                    (format!("TFE_TOKEN={}", key(3)), key(3)),
                ],
                misses: vec![
                    format!("{}.atlasv1.{}", rnd(4, 13, ALNUM), rnd(5, 67, URLSAFE)),
                    format!("{}.atlasv2.{}", rnd(6, 14, ALNUM), rnd(7, 67, URLSAFE)),
                    format!("{}.atlasv1.{}", rnd(8, 14, ALNUM), rnd(9, 40, URLSAFE)),
                ],
            }
        }
        "telegram-bot-token" => {
            let key = |n| format!("{}:AA{}", 123_456_780 + n, rnd(n, 33, URLSAFE));
            Sample {
                hits: vec![
                    same(key(1)),
                    (
                        format!("https://api.telegram.org/bot{}/getMe", key(2)),
                        key(2),
                    ),
                    (format!("TELEGRAM_TOKEN={}", key(3)), key(3)),
                ],
                misses: vec![
                    format!("1234567:AA{}", rnd(4, 33, URLSAFE)),
                    format!("123456789:AB{}", rnd(5, 33, URLSAFE)),
                    format!("123456789:AA{}", rnd(6, 20, URLSAFE)),
                ],
            }
        }
        "auth-header-credential" => {
            let tok = |n| format!("{}9x", rnd(n, 30, ALNUM));
            Sample {
                hits: vec![
                    (
                        format!(
                            "curl -H 'Authorization: Bearer {}' https://api.example.com",
                            tok(1)
                        ),
                        tok(1),
                    ),
                    (
                        format!("headers = {{\"x-api-key\": \"{}\"}}", tok(2)),
                        tok(2),
                    ),
                    (
                        format!("req.Header.Set(\"Authorization\", \"Token {}\")", tok(3)),
                        tok(3),
                    ),
                    (format!("X-Goog-Api-Key: {}", tok(4)), tok(4)),
                    // 鉴权方式和值之间是制表符（请求体里是转义过的 `\t`）
                    (format!("Authorization: Bearer\t{}", tok(5)), tok(5)),
                ],
                misses: vec![
                    "Authorization: Bearer $TOKEN".to_string(),
                    "headers = {\"Authorization\": \"Bearer \" + token}".to_string(),
                    "Authorization: Bearer <your-token>".to_string(),
                    "x-api-key: YOUR_API_KEY_HERE".to_string(),
                    "authorization: the process of granting access".to_string(),
                    "f\"Bearer {api_key}\" for Authorization: Bearer {api_key}".to_string(),
                ],
            }
        }
        "command-line-password" => Sample {
            hits: vec![
                (
                    "curl -u admin:S3cretPass9 https://example.com/api".to_string(),
                    "S3cretPass9".to_string(),
                ),
                (
                    "mysql -h db -uroot -pR00tPass42 shop".to_string(),
                    "R00tPass42".to_string(),
                ),
                (
                    "PGPASSWORD=pg5ecretX psql -h db -U app".to_string(),
                    "pg5ecretX".to_string(),
                ),
                (
                    "curl --user \"bob:Hunter2Two\" https://x.example.com".to_string(),
                    "Hunter2Two".to_string(),
                ),
            ],
            misses: vec![
                "curl -u $USER:$PASS https://example.com".to_string(),
                "mysql -u root -p shop".to_string(),
                "docker run -u 1000:1000 alpine".to_string(),
                "curl -u user:password https://example.com".to_string(),
                "curl -u sk_live_abc: https://api.stripe.com".to_string(),
            ],
        },
        "url-token" => {
            let tok = |n| format!("{}7Q", rnd(n, 30, URLSAFE));
            Sample {
                hits: vec![
                    (
                        format!("https://api.example.com/v1?access_token={}&page=2", tok(1)),
                        tok(1),
                    ),
                    (format!("GET /data?format=json&api_key={}", tok(2)), tok(2)),
                    (
                        format!(
                            "https://acct.blob.core.windows.net/c/f?sv=2022&sig={}%3D",
                            tok(3)
                        ),
                        format!("{}%3D", tok(3)),
                    ),
                ],
                misses: vec![
                    "https://api.example.com/?access_token=YOUR_TOKEN".to_string(),
                    "https://x.example.com/?api_key=${API_KEY}".to_string(),
                    "api_key=abc123def456 in prose".to_string(),
                    "https://x.example.com/?sig=short1".to_string(),
                ],
            }
        }
        "bot-webhook" => {
            let uuid = |n| {
                let h = rnd(n, 32, HEX);
                format!(
                    "{}-{}-{}-{}-{}",
                    &h[..8],
                    &h[8..12],
                    &h[12..16],
                    &h[16..20],
                    &h[20..]
                )
            };
            let ding = rnd(11, 64, HEX);
            let slack = rnd(12, 24, ALNUM);
            let discord = rnd(13, 68, URLSAFE);
            let teams = rnd(14, 32, HEX);
            Sample {
                hits: vec![
                    (
                        format!("https://open.feishu.cn/open-apis/bot/v2/hook/{}", uuid(1)),
                        uuid(1),
                    ),
                    (
                        format!("https://oapi.dingtalk.com/robot/send?access_token={ding}"),
                        ding,
                    ),
                    (
                        format!(
                            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key={}",
                            uuid(2)
                        ),
                        uuid(2),
                    ),
                    // 句末的标点、结尾的 `/` 不算令牌
                    (
                        format!(
                            "post to https://hooks.slack.com/services/T0123ABCD/B0123ABCD/{slack}."
                        ),
                        slack.clone(),
                    ),
                    (
                        format!(
                            "https://hooks.slack.com/services/T0123ABCD/B0123ABCD/{slack}/, ok"
                        ),
                        slack.clone(),
                    ),
                    (
                        format!("https://hooks.slack.com/services/T0123ABCD/B0123ABCD/{slack}"),
                        slack,
                    ),
                    (
                        format!("https://discord.com/api/webhooks/123456789012345678/{discord}"),
                        discord,
                    ),
                    (
                        format!(
                            "https://contoso.webhook.office.com/webhookb2/{}@{}/IncomingWebhook/{teams}/{}",
                            uuid(3),
                            uuid(4),
                            uuid(5)
                        ),
                        teams,
                    ),
                ],
                misses: vec![
                    // Slack 文档里的示例地址（拼出来：完整的样子会被代码托管平台当成泄漏的密钥拦下）
                    format!(
                        "https://hooks.slack.com/services/T{0}/B{0}/{1}",
                        "0".repeat(8),
                        "X".repeat(24)
                    ),
                    "https://open.feishu.cn/open-apis/bot/v2/hook/xxxxxxxxxxxxxxxxxxxx".to_string(),
                    "https://discord.com/api/webhooks/{id}/{token}".to_string(),
                    "https://oapi.dingtalk.com/robot/send?access_token=".to_string(),
                ],
            }
        }
        "config-assignment" => Sample {
            hits: vec![
                (
                    "DB_PASSWORD=hunter2hunter2".to_string(),
                    "hunter2hunter2".to_string(),
                ),
                (
                    "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYzQ8r2Lk0".to_string(),
                    "wJalrXUtnFEMI/K7MDENG/bPxRfiCYzQ8r2Lk0".to_string(),
                ),
                (
                    "{\"clientSecret\": \"q9Zx-77ab-Lm3n\"}".to_string(),
                    "q9Zx-77ab-Lm3n".to_string(),
                ),
                (
                    "api_key: 'k3y-for-prod-2026'".to_string(),
                    "k3y-for-prod-2026".to_string(),
                ),
                (
                    "export GITHUB_TOKEN=abc123def456ghi".to_string(),
                    "abc123def456ghi".to_string(),
                ),
                // `.env` 的写法：不加引号的值带着代码里才是分隔的标点，引号里的带着空格
                (
                    "DB_PASSWORD=Tr0ub4dor&3xyz".to_string(),
                    "Tr0ub4dor&3xyz".to_string(),
                ),
                (
                    "MAIL_PASSWORD=x9;Q[2]{a}<b>,c # prod".to_string(),
                    "x9;Q[2]{a}<b>,c".to_string(),
                ),
                (
                    "DB_PASSWORD=\"correct horse battery staple 9\"".to_string(),
                    "correct horse battery staple 9".to_string(),
                ),
                (
                    "client(api_token=abc123def456)".to_string(),
                    "abc123def456".to_string(),
                ),
                (
                    "curl -d token=abc123def456&state=xyz".to_string(),
                    "abc123def456".to_string(),
                ),
                // shell 里分隔命令的 `;` 不是值的一部分
                (
                    "if [ -n \"$x\" ]; then TOKEN=abc123def;fi".to_string(),
                    "abc123def".to_string(),
                ),
            ],
            misses: vec![
                "password = request.form[\"password\"]".to_string(),
                "API_KEY = os.getenv(\"API_KEY\")".to_string(),
                "password: string;".to_string(),
                "DB_PASSWORD=${DB_PASSWORD}".to_string(),
                "token = access_token".to_string(),
                "PWD=/home/someone/project1".to_string(),
                "\"nextPageToken\": \"CAoQAA2b3c4d\"".to_string(),
                "\"resetPassword\": \"Reset password 2\"".to_string(),
                "if password == \"hunter2hunter2\":".to_string(),
                "secret = \"abc\" + suffix123".to_string(),
                "max_tokens = 4096".to_string(),
                "TOKEN=generate_token(user_id=12345)".to_string(),
                "\"password\": \"Must be 8 characters long\"".to_string(),
                "SECRET=[1,2,3]".to_string(),
                // 下标、泛型类型、表达式
                "token=cfg[key2]".to_string(),
                "password=arr[i+1]".to_string(),
                "auth_token=self.tokens[0]".to_string(),
                "secret=Map<String,V2>".to_string(),
                "token=Vec<u8>::new2".to_string(),
                "secret=x<y&&z>w1".to_string(),
                "DB_PASSWORD=\"unterminated 12345\nnext line\"".to_string(),
            ],
        },
        "crypto-mnemonic" => {
            let e1 = bytes(21, 16);
            let e2 = bytes(22, 32);
            let e3 = bytes(23, 16);
            let numbered: String = english_phrase(&e3)
                .split(' ')
                .enumerate()
                .map(|(i, w)| format!("{}. {w}", i + 1))
                .collect::<Vec<_>>()
                .join("\n");
            let numbered_value = numbered[3..].to_string();
            Sample {
                hits: vec![
                    same(english_phrase(&e1)),
                    (format!("seed: {}", english_phrase(&e2)), english_phrase(&e2)),
                    (numbered, numbered_value),
                    same(chinese_phrase(&bytes(24, 16), " ")),
                    same(chinese_phrase(&bytes(25, 20), ", ")),
                ],
                misses: vec![
                    // 官方测试向量和开发工具的默认助记词
                    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about".to_string(),
                    "test test test test test test test test test test test junk".to_string(),
                    "legal winner thank year wave sausage worth useful legal winner thank yellow".to_string(),
                    // 校验和不对：最后一个词换掉
                    {
                        let p = english_phrase(&e1);
                        let mut w: Vec<&str> = p.split(' ').collect();
                        let last = w[11];
                        w[11] = if last == "zoo" { "zone" } else { "zoo" };
                        w.join(" ")
                    },
                    // 十一个词
                    english_phrase(&e1).rsplit_once(' ').unwrap().0.to_string(),
                    // 连着写的汉字是一句话
                    chinese_phrase(&bytes(26, 16), ""),
                ],
            }
        }
        "crypto-wif-key" => Sample {
            hits: vec![
                same(wif_of(31, 0x80, false)),
                same(wif_of(32, 0x80, true)),
                (
                    format!("privkey: {}", wif_of(33, 0xEF, true)),
                    wif_of(33, 0xEF, true),
                ),
            ],
            misses: vec![
                // 校验和坏了
                {
                    let mut w = wif_of(34, 0x80, true).into_bytes();
                    let i = w.len() - 1;
                    w[i] = if w[i] == b'2' { b'3' } else { b'2' };
                    String::from_utf8(w).unwrap()
                },
                // 版本字节不对
                check_encode(&[vec![0x81], bytes(35, 32), vec![0x01]].concat()),
                // 不是 Base58
                format!("K{}", "0".repeat(51)),
            ],
        },
        "crypto-extended-key" => Sample {
            hits: vec![
                same(xprv_of(41, [0x04, 0x88, 0xAD, 0xE4])),
                same(xprv_of(42, [0x04, 0x35, 0x83, 0x94])),
                (
                    format!("\"xprv\": \"{}\"", xprv_of(43, [0x04, 0xB2, 0x43, 0x0C])),
                    xprv_of(43, [0x04, 0xB2, 0x43, 0x0C]),
                ),
            ],
            misses: vec![
                // 扩展公钥不是私钥
                xprv_of(44, [0x04, 0x88, 0xB2, 0x1E]),
                {
                    let mut w = xprv_of(45, [0x04, 0x88, 0xAD, 0xE4]).into_bytes();
                    let i = w.len() - 2;
                    w[i] = if w[i] == b'a' { b'b' } else { b'a' };
                    String::from_utf8(w).unwrap()
                },
                format!("xprv{}", "1".repeat(107)),
            ],
        },
        _ => return None,
    };
    Some(s)
}
