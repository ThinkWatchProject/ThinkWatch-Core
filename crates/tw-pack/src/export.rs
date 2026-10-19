//! 导出：把本机的配置（全部，或者选的几项连同它们引用的）做成一个配置包。
//!
//! 包里**没有本机设置**（[`crate::model::LOCAL_SECTIONS`]、ChatGPT 的登录），配置按项目
//! 重新生成（[`crate::model::materialize`] 落在一份空配置上）。不带密钥时，密钥类的值换成
//! 空字符串：写成 `${变量名}` 的不是密钥，照样带着。

use std::collections::BTreeSet;

use serde_json::Value as Json;

use crate::model::{self, ItemId, ItemKind, ModelError, PluginCodec, Snapshot, unit};
use crate::{Manifest, Pack, PackKind, PackOrigin};

/// 导出什么、怎么导出。
#[derive(Debug, Clone)]
pub struct Options {
    /// None 是全部配置；否则是这几项，连同它们引用的
    pub scope: Option<Vec<ItemId>>,
    /// 带上密钥
    pub secrets: bool,
    /// 换掉通用偏好（经远程连接导出时 Lite 给的那一份）。None 是用包里本来的
    pub client_prefs: Option<Json>,
    pub origin: PackOrigin,
}

/// 导出的结果。
#[derive(Debug, Clone)]
pub struct Exported {
    pub pack: Pack,
    /// 包里有哪些项目（随附的也在），顺序项目不算
    pub included: Vec<ItemId>,
    /// 登录没有随包带走的上游：ChatGPT 账号，和令牌留在本机的 OAuth 上游
    pub omitted_logins: Vec<String>,
}

/// 导出。`local` 是本机此刻的样子（[`crate::disk::current`]）
pub fn export(local: &Pack, opts: Options) -> Result<Exported, ModelError> {
    export_with(local, opts, model::codec())
}

pub fn export_with(
    local: &Pack,
    opts: Options,
    codec: &dyn PluginCodec,
) -> Result<Exported, ModelError> {
    let mut snap = model::snapshot_with(local, codec)?;
    let prefs_id = ItemId::new(ItemKind::ClientPrefs, "");
    if let Some(p) = &opts.client_prefs {
        let mut units = Default::default();
        model::json_flatten(p, &mut units);
        snap.items.remove(&prefs_id);
        if !units.is_empty() {
            snap.items.insert(
                prefs_id,
                model::Item {
                    units,
                    ..Default::default()
                },
            );
        }
    }
    if let Some(roots) = &opts.scope {
        let keep = snap.closure(roots);
        snap.retain(&keep);
    }
    if !opts.secrets {
        strip_secrets(&mut snap);
    }
    let cfg = model::parse(&local.config_yaml)?;
    let included: Vec<ItemId> = snap
        .items
        .keys()
        .filter(|id| !id.kind.is_order())
        .cloned()
        .collect();
    let picked: BTreeSet<&str> = included
        .iter()
        .filter(|id| id.kind == ItemKind::Provider)
        .map(|id| id.name.as_str())
        .collect();
    let omitted_logins = cfg
        .providers
        .iter()
        .filter(|p| picked.contains(p.name.as_str()) && model::holds_login(p))
        .map(|p| p.name.clone())
        .collect();
    let kind = if opts.scope.is_some() {
        PackKind::Partial
    } else {
        PackKind::Full
    };
    let mut manifest = Manifest::new(kind, opts.origin);
    manifest.secrets = opts.secrets;
    let base = Pack {
        manifest,
        config_yaml: format!("version: {}\n", tw_config::SCHEMA_VERSION),
        plugins: Default::default(),
        client_prefs: None,
    };
    let pack = model::materialize_for_export(&base, &snap, codec)?;
    Ok(Exported {
        pack,
        included,
        omitted_logins,
    })
}

/// 不是 `${变量名}` 的就是密钥
fn is_literal(v: &Json) -> bool {
    v.as_str()
        .is_some_and(|s| !s.is_empty() && !s.contains("${"))
}

fn blank(at: Option<&mut Json>) {
    if let Some(v) = at
        && is_literal(v)
    {
        *v = Json::String(String::new());
    }
}

/// 去掉密钥：上游的密钥、请求头的值、AWS 访问密钥、OAuth 的 refresh token 和客户端密钥，
/// 代理的密码，网关密钥的值。换成空字符串，导入的一方据此知道这里本来有一个
pub fn strip_secrets(snap: &mut Snapshot) {
    for (id, item) in snap.items.iter_mut() {
        match id.kind {
            ItemKind::Provider => {
                let Some(conn) = item.units.get_mut(unit::CONN) else {
                    continue;
                };
                blank(conn.get_mut("key"));
                if let Some(Json::Array(headers)) = conn.get_mut("headers") {
                    for h in headers {
                        blank(h.get_mut(1));
                    }
                }
                if let Some(aws) = conn.get_mut("aws") {
                    for k in ["access_key_id", "secret_access_key", "session_token"] {
                        blank(aws.get_mut(k));
                    }
                }
                if let Some(Json::Object(o)) = conn.get_mut("oauth") {
                    o.remove("access");
                    o.remove("expires_at");
                    blank(o.get_mut("refresh"));
                    blank(o.get_mut("client_secret"));
                }
            }
            ItemKind::Proxy => {
                if let Some(auth) = item
                    .units
                    .get_mut(unit::ALL)
                    .and_then(|a| a.get_mut("auth"))
                {
                    blank(auth.get_mut("pass"));
                }
            }
            ItemKind::Key => {
                blank(item.units.get_mut(unit::ALL).and_then(|a| a.get_mut("key")));
            }
            _ => {}
        }
    }
}

/// 不带密钥的包里的一项，**空着的密钥换成本机同一项里的**：上游的密钥、请求头的值（按名字）、
/// AWS 访问密钥、OAuth 的客户端密钥，代理的密码，网关密钥的值。比较和覆盖都照这个样子：除了
/// 密钥都一样的就是「相同」，覆盖不会把本机的密钥抹成空的。
///
/// **只在目的地不变时**（[`same_destination`]）：包里的上游换了主机、端口、AWS 区域或 OAuth
/// 令牌端点，代理换了地址，本机的密钥就不跟过去 —— 否则一份不带密钥的包改个地址，覆盖之后
/// 本机的密钥就发给了包里写的那个地方
pub fn fill_secrets(id: &ItemId, item: &mut model::Item, mine: &model::Item) {
    if !same_destination(id, item, mine) {
        return;
    }
    let empty = |v: Option<&Json>| v.and_then(Json::as_str) == Some("");
    let take = |at: Option<&mut Json>, from: Option<&Json>| {
        if let (Some(v), Some(f)) = (at, from)
            && v.as_str() == Some("")
            && f.as_str().is_some_and(|s| !s.is_empty())
        {
            *v = f.clone();
        }
    };
    match id.kind {
        ItemKind::Provider => {
            let (Some(conn), Some(local)) =
                (item.units.get_mut(unit::CONN), mine.units.get(unit::CONN))
            else {
                return;
            };
            take(conn.get_mut("key"), local.get("key"));
            if let Some(Json::Array(headers)) = conn.get_mut("headers") {
                let theirs = local.get("headers").and_then(Json::as_array);
                for h in headers {
                    if !empty(h.get(1)) {
                        continue;
                    }
                    let name = h
                        .get(0)
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let found = theirs.and_then(|hs| {
                        hs.iter().find(|x| {
                            x.get(0)
                                .and_then(Json::as_str)
                                .is_some_and(|n| n.eq_ignore_ascii_case(&name))
                        })
                    });
                    take(h.get_mut(1), found.and_then(|x| x.get(1)));
                }
            }
            for k in ["access_key_id", "secret_access_key", "session_token"] {
                take(
                    conn.get_mut("aws").and_then(|a| a.get_mut(k)),
                    local.get("aws").and_then(|a| a.get(k)),
                );
            }
            take(
                conn.get_mut("oauth")
                    .and_then(|o| o.get_mut("client_secret")),
                local.get("oauth").and_then(|o| o.get("client_secret")),
            );
        }
        ItemKind::Proxy => {
            let local = mine.units.get(unit::ALL).and_then(|a| a.get("auth"));
            take(
                item.units
                    .get_mut(unit::ALL)
                    .and_then(|a| a.get_mut("auth"))
                    .and_then(|a| a.get_mut("pass")),
                local.and_then(|a| a.get("pass")),
            );
        }
        ItemKind::Key => {
            take(
                item.units.get_mut(unit::ALL).and_then(|a| a.get_mut("key")),
                mine.units.get(unit::ALL).and_then(|a| a.get("key")),
            );
        }
        _ => {}
    }
}

/// 两份同名的项目，密钥是发去同一个地方的吗：上游的协议、主机、端口一样，AWS 区域一样，
/// OAuth 的令牌端点一样；代理的类型和地址一样。网关密钥是发给这台网关自己的，总是
pub fn same_destination(id: &ItemId, theirs: &model::Item, mine: &model::Item) -> bool {
    let at = |it: &model::Item, u: &str, path: &[&str]| -> Option<Json> {
        let mut cur = it.units.get(u)?;
        for p in path {
            cur = cur.get(*p)?;
        }
        Some(cur.clone())
    };
    match id.kind {
        ItemKind::Provider => {
            let url = |it: &model::Item| {
                at(it, unit::CONN, &["base_url"]).and_then(|v| v.as_str().and_then(destination))
            };
            url(theirs).is_some()
                && url(theirs) == url(mine)
                && at(theirs, unit::CONN, &["aws", "region"])
                    == at(mine, unit::CONN, &["aws", "region"])
                && at(theirs, unit::CONN, &["oauth", "endpoint"])
                    == at(mine, unit::CONN, &["oauth", "endpoint"])
        }
        ItemKind::Proxy => {
            let addr = |it: &model::Item| {
                at(it, unit::ALL, &["addr"])
                    .and_then(|v| v.as_str().map(|a| a.trim().to_ascii_lowercase()))
            };
            addr(theirs).is_some()
                && addr(theirs) == addr(mine)
                && at(theirs, unit::ALL, &["type"]) == at(mine, unit::ALL, &["type"])
        }
        _ => true,
    }
}

/// 一个地址发去哪儿：协议、主机（小写）、端口（没写时按协议的缺省）。读不懂是 None
pub fn destination(url: &str) -> Option<(String, String, u16)> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next()?;
    let hostport = authority.rsplit('@').next()?;
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let (h, after) = v6.split_once(']')?;
        (h.to_string(), after.strip_prefix(':'))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (hostport.to_string(), None),
        }
    };
    let port = match port {
        Some(p) => p.parse().ok()?,
        None => match scheme.as_str() {
            "https" => 443,
            "http" => 80,
            _ => return None,
        },
    };
    (!host.is_empty()).then(|| (scheme, host.to_ascii_lowercase(), port))
}

/// 一项里有没有被去掉的密钥（空字符串）
pub fn missing_secret(id: &ItemId, item: &model::Item) -> bool {
    let empty = |v: Option<&Json>| v.and_then(Json::as_str) == Some("");
    match id.kind {
        ItemKind::Provider => {
            let Some(conn) = item.units.get(unit::CONN) else {
                return false;
            };
            empty(conn.get("key"))
                || conn
                    .get("headers")
                    .and_then(Json::as_array)
                    .is_some_and(|hs| hs.iter().any(|h| empty(h.get(1))))
                || conn.get("aws").is_some_and(|aws| {
                    ["access_key_id", "secret_access_key", "session_token"]
                        .iter()
                        .any(|k| empty(aws.get(*k)))
                })
                || conn
                    .get("oauth")
                    .is_some_and(|o| empty(o.get("refresh")) || empty(o.get("client_secret")))
        }
        ItemKind::Key => empty(item.units.get(unit::ALL).and_then(|a| a.get("key"))),
        ItemKind::Proxy => empty(
            item.units
                .get(unit::ALL)
                .and_then(|a| a.get("auth"))
                .and_then(|a| a.get("pass")),
        ),
        _ => false,
    }
}

/// 导出文件的建议名字：`ThinkWatch-2026-10-18.twconfig`（UTC 的日期）
pub fn file_name(created_at_ms: u64) -> String {
    let (y, m, d) = civil(created_at_ms / 86_400_000);
    format!("ThinkWatch-{y:04}-{m:02}-{d:02}.{}", crate::EXTENSION)
}

/// 1970-01-01 起的第几天 → 年月日（公历）
fn civil(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_destination_is_scheme_host_and_port() {
        let d = |u| destination(u);
        assert_eq!(
            d("https://Relay.example/v1"),
            Some(("https".into(), "relay.example".into(), 443))
        );
        assert_eq!(
            d("https://relay.example:443/x"),
            d("https://relay.example/y")
        );
        assert_ne!(d("https://relay.example:8443"), d("https://relay.example"));
        assert_ne!(d("http://relay.example"), d("https://relay.example"));
        assert_eq!(
            d("http://user:pw@[::1]:8080/p"),
            Some(("http".into(), "::1".into(), 8080))
        );
        assert_eq!(d("not a url"), None);
    }

    #[test]
    fn the_file_name_carries_the_date() {
        assert_eq!(file_name(0), "ThinkWatch-1970-01-01.twconfig");
        // 2026-10-18 09:30 UTC
        assert_eq!(
            file_name(1_792_315_800_000),
            "ThinkWatch-2026-10-18.twconfig"
        );
        assert_eq!(file_name(951_782_400_000), "ThinkWatch-2000-02-29.twconfig");
    }
}
