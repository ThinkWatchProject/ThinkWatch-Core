//! `listen.control.remote` 在配置原文里的几件事：写出这一节、打开、关上。
//!
//! 和钥匙一样**都是最小替换**：只动这一节里要动的那几个值，用户的注释和
//! 排版原样留着。`twcore init` 和 `twcore remote enable/disable` 走这里 ——
//! 远程端口只能在服务器本机上开关，控制面改不了它（见 tw-control）。

use std::path::Path;

use tw_yaml::{PatchError, Put, Scalar, Step};

use crate::control_key::EnsureError;
use crate::store;
use crate::{Bind, RemoteListen};

fn at(k: Option<&str>) -> Vec<Step> {
    let mut p: Vec<Step> = ["listen", "control", "remote"]
        .iter()
        .map(|s| Step::key(*s))
        .collect();
    if let Some(k) = k {
        p.push(Step::key(k));
    }
    p
}

/// 打开时要改的地方。`None` 的留着原样（第一次写出这一节时用默认值）。
#[derive(Debug, Clone, Default)]
pub struct Enable {
    pub bind: Option<Bind>,
    pub port: Option<u16>,
    pub allow_from: Option<Vec<String>>,
}

fn flow_list(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("[{}]", quoted.join(", "))
}

/// 这一节此刻在原文里是什么样。没写是 `None`。
fn current(text: &str) -> Option<RemoteListen> {
    let cfg: crate::Config = serde_yaml_ng::from_str(text).ok()?;
    cfg.listen.control.remote
}

fn gateway_port(text: &str) -> u16 {
    serde_yaml_ng::from_str::<crate::Config>(text)
        .map(|c| c.listen.gateway.port)
        .unwrap_or(crate::DEFAULT_GATEWAY_PORT)
}

/// 没有这一节就写出来，**端口随机、默认关着**。已经有了就不动。
///
/// `twcore init` 用它：服务器上手工部署的人打开配置就能看到这一节，改一个
/// `enabled` 就能用；端口是现挑的，不是人人都知道的那一个。
pub fn ensure_section(text: &str, enabled: bool) -> Result<Option<String>, PatchError> {
    if current(text).is_some() {
        return Ok(None);
    }
    let port = crate::generate_remote_port(gateway_port(text));
    let block = format!(
        "enabled: {enabled}\nbind: all\nport: {port}\nallow_from: {}",
        flow_list(&crate::default_allow_from())
    );
    tw_yaml::put(text, &at(None), Put::Block(&block)).map(Some)
}

/// 打开远程端口，按 `e` 改几处。没有这一节就先写出来（端口随机）。
pub fn enable(text: &str, e: &Enable) -> Result<String, PatchError> {
    let mut out = match ensure_section(text, true)? {
        Some(t) => t,
        None => tw_yaml::insert(text, &at(Some("enabled")), &Scalar::Bool(true))?,
    };
    if let Some(b) = &e.bind {
        out = tw_yaml::insert(&out, &at(Some("bind")), &Scalar::s(b.to_string()))?;
    }
    if let Some(p) = e.port {
        out = tw_yaml::insert(&out, &at(Some("port")), &Scalar::Int(p.into()))?;
    }
    if let Some(list) = &e.allow_from {
        out = tw_yaml::put(&out, &at(Some("allow_from")), Put::Inline(&flow_list(list)))?;
    }
    Ok(out)
}

/// 关上远程端口：`enabled: false`，其余（端口、名单）留着，下次打开还是它们。
/// 没有这一节就什么都不做。
pub fn disable(text: &str) -> Result<Option<String>, PatchError> {
    match current(text) {
        Some(r) if r.enabled => {
            tw_yaml::insert(text, &at(Some("enabled")), &Scalar::Bool(false)).map(Some)
        }
        _ => Ok(None),
    }
}

/// 改完先校验、再写，前后两版进历史。和 `control_key::rotate_file` 同一条路。
fn write_checked(
    path: &Path,
    f: impl FnOnce(&str) -> Result<Option<String>, PatchError>,
) -> Result<Option<RemoteListen>, EnsureError> {
    let cur = store::read(path)?;
    let Some(next) = f(&cur.text)? else {
        return Ok(current(&cur.text));
    };
    let cfg = crate::try_parse(&next).map_err(|r| EnsureError::Rejected(Box::new(r)))?;
    let _ = crate::history::snapshot(path, &cur.text, crate::history::Origin::Cli);
    store::write_if_unchanged(path, &cur.fingerprint, &next)?;
    let _ = crate::history::snapshot(path, &next, crate::history::Origin::Cli);
    Ok(cfg.listen.control.remote)
}

/// `twcore remote enable`。返回写下去的那一节。
pub fn enable_file(path: &Path, e: &Enable) -> Result<Option<RemoteListen>, EnsureError> {
    write_checked(path, |t| enable(t, e).map(Some))
}

/// `twcore remote disable`。
pub fn disable_file(path: &Path) -> Result<Option<RemoteListen>, EnsureError> {
    write_checked(path, disable)
}

/// `twcore init`：写出一节关着的。
pub fn ensure_section_file(path: &Path) -> Result<Option<RemoteListen>, EnsureError> {
    write_checked(path, |t| ensure_section(t, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "version: 1 # 别动我\nlisten:\n  control:\n    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00\nclients:\n  - { name: default, key: tw-abc }\n";

    fn parse(t: &str) -> crate::Config {
        crate::try_parse(t).unwrap_or_else(|e| panic!("{e}\n{t}"))
    }

    #[test]
    fn the_section_is_written_closed_with_a_random_port_and_the_default_list() {
        let t = ensure_section(BASE, false).unwrap().unwrap();
        assert!(t.starts_with("version: 1 # 别动我\n"), "{t}");
        let r = parse(&t).listen.control.remote.unwrap();
        assert!(!r.enabled);
        assert_eq!(r.bind, Bind::All);
        assert!(crate::REMOTE_PORT_RANGE.contains(&r.port));
        assert_eq!(
            r.allow_from,
            crate::default_allow_from(),
            "默认名单要写出来"
        );
        assert!(t.contains("allow_from:"), "{t}");
        // 已经有了就不动
        assert_eq!(ensure_section(&t, true).unwrap(), None);
        // 两次各挑各的端口
        let ports: std::collections::HashSet<u16> = (0..20)
            .map(|_| {
                parse(&ensure_section(BASE, false).unwrap().unwrap())
                    .listen
                    .control
                    .remote
                    .unwrap()
                    .port
            })
            .collect();
        assert!(ports.len() > 1, "端口该是随机的：{ports:?}");
    }

    #[test]
    fn enabling_changes_only_what_was_asked_and_disabling_keeps_the_port() {
        let t = enable(BASE, &Enable::default()).unwrap();
        let r = parse(&t).listen.control.remote.unwrap();
        assert!(r.enabled);
        let port = r.port;

        let t = enable(
            &t,
            &Enable {
                bind: Some(Bind::Nic("en0".into())),
                port: None,
                allow_from: Some(vec!["192.168.1.0/24".into(), "fc00::/7".into()]),
            },
        )
        .unwrap();
        let r = parse(&t).listen.control.remote.unwrap();
        assert_eq!(r.port, port, "没要求就不换端口");
        assert_eq!(r.bind, Bind::Nic("en0".into()));
        assert_eq!(r.allow_from, ["192.168.1.0/24", "fc00::/7"]);

        let off = disable(&t).unwrap().unwrap();
        let r2 = parse(&off).listen.control.remote.unwrap();
        assert!(!r2.enabled);
        assert_eq!(r2.port, port);
        assert_eq!(r2.allow_from, r.allow_from);
        assert_eq!(disable(&off).unwrap(), None, "已经关着就不写");
        assert_eq!(disable(BASE).unwrap(), None, "没有这一节就不写");
    }

    #[test]
    fn the_port_never_lands_on_the_gateways() {
        for _ in 0..200 {
            assert_ne!(crate::generate_remote_port(20000), 20000);
        }
    }

    #[test]
    fn the_file_helpers_validate_before_writing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        std::fs::write(&p, BASE).unwrap();
        let r = enable_file(&p, &Enable::default()).unwrap().unwrap();
        assert!(r.enabled);
        // 和网关同一个端口：不写
        let before = std::fs::read_to_string(&p).unwrap();
        let e = enable_file(
            &p,
            &Enable {
                port: Some(crate::DEFAULT_GATEWAY_PORT),
                ..Default::default()
            },
        );
        assert!(e.is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
        assert!(!disable_file(&p).unwrap().unwrap().enabled);
    }
}
