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
