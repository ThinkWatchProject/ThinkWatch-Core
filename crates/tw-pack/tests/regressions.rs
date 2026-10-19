//! 审查查出来的几条，各一个回归测试：预览能过的，写下去一定成；不带密钥的包不抹本机的密钥；
//! 安全模式下整份恢复一个不带本机设置的版本。

use std::collections::{BTreeMap, BTreeSet};

use tw_pack::import::{Action, Context, Decision, Flag, Status};
use tw_pack::model::{ItemId, ItemKind};
use tw_pack::restore::{Change, Kept};
use tw_pack::{Manifest, Pack, PackKind, PackOrigin};

const KEY: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

fn pack(text: &str) -> Pack {
    Pack {
        manifest: Manifest::new(PackKind::Full, PackOrigin::Ui),
        config_yaml: text.to_string(),
        plugins: BTreeMap::new(),
        client_prefs: None,
    }
}

fn ctx() -> Context<'static> {
    Context {
        has_aws_profile: &|_| false,
        new_key: &|| "tw-regenerated".to_string(),
        with_prefs: true,
    }
}

fn local() -> Pack {
    pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\n  gateway:\n    port: 9000\nclients:\n  - name: default\n    key: tw-local\nproxies:\n  - name: px\n    type: http\n    addr: 10.0.0.1:8080\n    auth: {{ user: u, pass: secret-pass }}\nproviders:\n  - name: relay\n    base_url: https://relay.example\n    key: sk-old\n    proxy: px\n    headers:\n      X-Token: tok-old\n"
    ))
}

fn roundtrip(p: &Pack) -> Pack {
    tw_pack::read(
        &tw_pack::write(p, tw_pack::Seal::None).unwrap(),
        tw_pack::Unseal::None,
    )
    .unwrap()
}

fn export(p: &Pack, secrets: bool, origin: PackOrigin) -> Pack {
    roundtrip(
        &tw_pack::export::export(
            p,
            tw_pack::export::Options {
                scope: None,
                secrets,
                client_prefs: None,
                origin,
            },
        )
        .unwrap()
        .pack,
    )
}

fn decide(item: &ItemId, action: Action) -> Decision {
    Decision {
        item: item.clone(),
        action,
    }
}

/// 不带密钥的包：除了密钥都一样的是「相同」；覆盖时本机的密钥留着；网关密钥只给新加的那把
/// 重新生成
#[test]
fn a_pack_without_secrets_keeps_the_secrets_here() {
    let l = local();
    let incoming = export(&l, false, PackOrigin::Export);
    let rows = tw_pack::import::preview(&l, &incoming, &ctx()).unwrap();
    for r in &rows {
        assert_eq!(r.status, Status::Same, "{} {:?}", r.item, r.differs);
        assert!(r.flags.is_empty(), "{} {:?}", r.item, r.flags);
    }
    // 包里的上游改了地址、多了一家、多了一把密钥
    let mut changed = incoming.clone();
    changed.config_yaml = "version: 1\nclients:\n  - name: default\n    key: ''\n  - name: other\n    key: ''\nproxies:\n  - name: px\n    type: http\n    addr: 10.0.0.1:8080\n    auth: { user: u, pass: '' }\nproviders:\n  - name: relay\n    base_url: https://relay.example/v2\n    key: ''\n    proxy: px\n    headers:\n      X-Token: ''\n  - name: fresh\n    base_url: https://fresh.example\n    key: ''\n".into();
    let rows = tw_pack::import::preview(&l, &changed, &ctx()).unwrap();
    let row = |kind, name: &str| {
        rows.iter()
            .find(|r| r.item == ItemId::new(kind, name))
            .unwrap_or_else(|| panic!("{name}: {rows:?}\n{}", changed.config_yaml))
    };
    assert_eq!(row(ItemKind::Provider, "relay").status, Status::Conflict);
    assert_eq!(row(ItemKind::Provider, "relay").flags, Vec::<Flag>::new());
    assert_eq!(row(ItemKind::Provider, "fresh").flags, [Flag::MissingKey]);
    assert_eq!(row(ItemKind::Key, "default").status, Status::Same);
    let out = tw_pack::import::apply(
        &l,
        &changed,
        &[
            decide(&ItemId::new(ItemKind::Provider, "relay"), Action::Overwrite),
            decide(&ItemId::new(ItemKind::Provider, "fresh"), Action::Import),
            decide(&ItemId::new(ItemKind::Key, "other"), Action::Import),
            decide(&ItemId::new(ItemKind::Proxy, "px"), Action::Overwrite),
        ],
        &ctx(),
    )
    .unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
    let relay = cfg.providers.iter().find(|p| p.name == "relay").unwrap();
    assert_eq!(relay.base_url, "https://relay.example/v2");
    assert_eq!(relay.key.as_ref().map(|k| k.raw()), Some("sk-old"));
    assert_eq!(relay.headers.get("X-Token").unwrap().value.raw(), "tok-old");
    assert!(!relay.disabled);
    let fresh = cfg.providers.iter().find(|p| p.name == "fresh").unwrap();
    assert!(fresh.disabled);
    assert_eq!(
        cfg.proxies[0].auth.as_ref().unwrap().pass.raw(),
        "secret-pass"
    );
    let key = |n: &str| {
        cfg.clients
            .iter()
            .find(|c| c.name == n)
            .unwrap()
            .key
            .clone()
    };
    assert_eq!(key("default"), "tw-local");
    assert_eq!(key("other"), "tw-regenerated");
    assert!(out.changed_keys.is_empty(), "{:?}", out.changed_keys);
}

/// 安全模式下整份恢复一个同步版本（里面没有本机那几节）：钥匙、监听取现在这份原文里的；
/// 原文坏到读不成 YAML 时取最近一个本机备份里的
#[test]
fn a_whole_restore_brings_back_this_machines_sections() {
    let l = local();
    let synced = export(&l, true, PackOrigin::Sync);
    // 读得成 YAML、读不成一份配置（安全模式最常见的样子）
    let broken = format!("{}not_a_field: 1\n", l.config_yaml);
    assert!(tw_config::try_parse(&broken).is_err());
    let out = tw_pack::restore::whole(&synced, &broken, None);
    let cfg = tw_config::try_parse(&out.config_yaml)
        .unwrap_or_else(|e| panic!("{e}\n{}", out.config_yaml));
    assert_eq!(cfg.listen.gateway.port, 9000);
    assert!(out.config_yaml.contains(KEY));
    // 读不成 YAML：本机那几节从最近的本机备份来（钥匙在那里是打码的，换成原文里的这一把）
    let not_yaml = format!("version: 1\nlisten:\n  control:\n    key: {KEY}\nproviders: [\n");
    let mut last = l.clone();
    last.config_yaml = tw_config::control_key::mask(&l.config_yaml);
    let out = tw_pack::restore::whole(&synced, &not_yaml, Some(&last));
    let cfg = tw_config::try_parse(&out.config_yaml)
        .unwrap_or_else(|e| panic!("{e}\n{}", out.config_yaml));
    assert_eq!(cfg.listen.gateway.port, 9000);
    assert_eq!(cfg.listen.control.key.as_deref(), Some(KEY));
}

/// 改名导入一把和本机同名同值的网关密钥：副本的值重新生成
#[test]
fn a_key_imported_under_another_name_gets_its_own_value() {
    let l = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\n  - name: codex\n    key: tw-codex\n"
    ));
    let incoming =
        pack("version: 1\nclients:\n  - name: codex\n    key: tw-codex\n    max_concurrent: 2\n");
    let rows = tw_pack::import::preview(&l, &incoming, &ctx()).unwrap();
    let row = rows.iter().find(|r| r.item.name == "codex").unwrap();
    assert_eq!(row.status, Status::Conflict);
    let out = tw_pack::import::apply(
        &l,
        &incoming,
        &[decide(
            &row.item,
            Action::Rename(row.suggested_name.clone().unwrap()),
        )],
        &ctx(),
    )
    .unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
    let copy = cfg.clients.iter().find(|c| c.name == "codex (2)").unwrap();
    assert_eq!(copy.key, "tw-regenerated");
    assert_eq!(copy.max_concurrent, Some(2));
}

/// 恢复留着固定密钥，它绑的路由也留着；预览如实说
#[test]
fn a_kept_key_keeps_the_route_it_is_bound_to() {
    let now = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\n  - name: codex\n    key: tw-cx\n    route: fast\nproviders:\n  - name: kept\n    base_url: https://kept.example\n    key: sk-k\nroutes:\n  - name: fast\n    rules:\n      - name: all\n        to: kept\n"
    ));
    let then = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\nproviders:\n  - name: kept\n    base_url: https://kept.example\n    key: sk-k\n"
    ));
    let pinned: BTreeSet<String> = ["codex".to_string()].into();
    let rows = tw_pack::restore::preview(&now, &then, &pinned, true).unwrap();
    let kept: Vec<(String, Change, Option<Kept>)> = rows
        .iter()
        .map(|r| (r.item.to_string(), r.change, r.kept))
        .collect();
    assert!(
        kept.contains(&(
            "route `fast`".into(),
            Change::Remove,
            Some(Kept::Referenced)
        )),
        "{kept:?}"
    );
    assert!(
        kept.contains(&("key `codex`".into(), Change::Remove, Some(Kept::KeyInUse))),
        "{kept:?}"
    );
    let out = tw_pack::restore::apply(&now, &then, None, &pinned, true).unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
    assert!(cfg.routes.iter().any(|r| r.name == "fast"));
    assert!(cfg.clients.iter().any(|c| c.name == "codex"));
}

/// 只选了一个已删的上游，它用的出站代理也删了：一起恢复
#[test]
fn restoring_one_item_brings_what_it_needs() {
    let now = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\n"
    ));
    let then = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\nproxies:\n  - name: px\n    type: socks5h\n    addr: 127.0.0.1:7890\nproviders:\n  - name: gone\n    base_url: https://gone.example\n    key: sk-g\n    proxy: px\n"
    ));
    let out = tw_pack::restore::apply(
        &now,
        &then,
        Some(&[ItemId::new(ItemKind::Provider, "gone")]),
        &BTreeSet::new(),
        true,
    )
    .unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
    assert_eq!(cfg.providers[0].proxy, "px");
    assert_eq!(cfg.proxies[0].name, "px");
}

/// 恢复一个后来换了令牌端点的 OAuth 上游：本机的令牌用不上；那一版有令牌的取回来，没有的
/// 停用着回来，都不是恢复失败
#[test]
fn an_oauth_upstream_whose_endpoint_changed_comes_back() {
    let oauth = |rt: &str, ep: &str| {
        format!(
            "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\nproviders:\n  - name: o\n    base_url: https://corp.example\n    oauth:\n      refresh: '{rt}'\n      endpoint: {ep}\n"
        )
    };
    let now = pack(&oauth("rt-now", "https://t2.example/token"));
    let then = pack(&oauth("rt-then", "https://t1.example/token"));
    let out = tw_pack::restore::apply(&now, &then, None, &BTreeSet::new(), true).unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
    let o = cfg.providers[0].oauth.as_ref().unwrap();
    assert_eq!(o.endpoint, "https://t1.example/token");
    assert_eq!(o.refresh, "rt-then");
    assert!(!cfg.providers[0].disabled);
    // 那一版里也没有令牌：停用着回来
    let blank = pack(
        &oauth("", "https://t1.example/token")
            .replace("    oauth:", "    disabled: true\n    oauth:"),
    );
    let out = tw_pack::restore::apply(&now, &blank, None, &BTreeSet::new(), true).unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
    assert!(cfg.providers[0].disabled);
}

/// 不带密钥的包把上游指去了别的地方（换主机、换端口、换协议），或者改名导入：本机的密钥
/// **不跟过去** —— 留空、标「缺少密钥」、停用。代理换了地址也一样。只换路径照样沿用（见上一条）
#[test]
fn a_pack_that_moves_an_upstream_does_not_take_the_key_along() {
    let l = local();
    let relay = ItemId::new(ItemKind::Provider, "relay");
    let px = ItemId::new(ItemKind::Proxy, "px");
    let with = |base_url: &str, addr: &str| {
        let mut p = export(&l, false, PackOrigin::Export);
        p.config_yaml = format!(
            "version: 1\nproxies:\n  - name: px\n    type: http\n    addr: {addr}\n    auth: {{ user: u, pass: '' }}\nproviders:\n  - name: relay\n    base_url: {base_url}\n    key: ''\n    proxy: px\n    headers:\n      X-Token: ''\n"
        );
        p
    };
    for base_url in [
        "https://evil.example",
        "https://relay.example:8443",
        "http://relay.example",
    ] {
        let incoming = with(base_url, "10.0.0.1:8080");
        let rows = tw_pack::import::preview(&l, &incoming, &ctx()).unwrap();
        let row = rows.iter().find(|r| r.item == relay).unwrap();
        assert_eq!(row.status, Status::Conflict, "{base_url}");
        assert_eq!(row.flags, [Flag::MissingKey], "{base_url}");
        let out =
            tw_pack::import::apply(&l, &incoming, &[decide(&relay, Action::Overwrite)], &ctx())
                .unwrap();
        let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
        let p = cfg.providers.iter().find(|p| p.name == "relay").unwrap();
        assert_eq!(p.base_url, base_url);
        assert_eq!(p.key.as_ref().map(|k| k.raw()), Some(""), "{base_url}");
        assert_eq!(
            p.headers.get("X-Token").unwrap().value.raw(),
            "",
            "{base_url}"
        );
        assert!(p.disabled, "{base_url}");
    }

    // 改名导入是另一个项目：地址没变也不带走本机那一家的密钥
    let incoming = with("https://relay.example", "10.0.0.1:8080");
    let out = tw_pack::import::apply(
        &l,
        &incoming,
        &[decide(&relay, Action::Rename("relay (2)".into()))],
        &ctx(),
    )
    .unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
    let p = |n: &str| cfg.providers.iter().find(|p| p.name == n).unwrap();
    assert_eq!(p("relay (2)").key.as_ref().map(|k| k.raw()), Some(""));
    assert!(p("relay (2)").disabled);
    assert_eq!(p("relay").key.as_ref().map(|k| k.raw()), Some("sk-old"));

    // 代理换了地址：密码不跟过去
    let incoming = with("https://relay.example", "203.0.113.9:3128");
    let rows = tw_pack::import::preview(&l, &incoming, &ctx()).unwrap();
    let row = rows.iter().find(|r| r.item == px).unwrap();
    assert_eq!(row.status, Status::Conflict);
    assert_eq!(row.flags, [Flag::MissingKey]);
    let out =
        tw_pack::import::apply(&l, &incoming, &[decide(&px, Action::Overwrite)], &ctx()).unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
    assert_eq!(cfg.proxies[0].auth.as_ref().unwrap().pass.raw(), "");
}

/// AWS 区域、OAuth 令牌端点换了也是换了目的地
#[test]
fn a_moved_aws_region_or_oauth_endpoint_does_not_take_the_secret_along() {
    let l = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-local\nproviders:\n  - name: br\n    base_url: https://bedrock.corp.example\n    protocol: bedrock\n    aws:\n      access_key_id: AKIAOLD\n      secret_access_key: old-secret\n      region: us-east-1\n  - name: oa\n    base_url: https://api.example\n    oauth:\n      refresh: r-old\n      endpoint: https://auth.example/token\n      client_id: cid\n      client_secret: cs-old\n"
    ));
    let incoming = {
        let mut p = export(&l, false, PackOrigin::Export);
        p.config_yaml = "version: 1\nproviders:\n  - name: br\n    base_url: https://bedrock.corp.example\n    protocol: bedrock\n    aws:\n      access_key_id: ''\n      secret_access_key: ''\n      region: eu-west-1\n  - name: oa\n    base_url: https://api.example\n    oauth:\n      refresh: ''\n      endpoint: https://other.example/token\n      client_id: cid\n      client_secret: ''\n".into();
        p
    };
    let br = ItemId::new(ItemKind::Provider, "br");
    let oa = ItemId::new(ItemKind::Provider, "oa");
    let out = tw_pack::import::apply(
        &l,
        &incoming,
        &[
            decide(&br, Action::Overwrite),
            decide(&oa, Action::Overwrite),
        ],
        &ctx(),
    )
    .unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml)
        .unwrap_or_else(|e| panic!("{e}\n{}", out.pack.config_yaml));
    let p = |n: &str| cfg.providers.iter().find(|p| p.name == n).unwrap();
    let aws = p("br").aws.as_ref().unwrap();
    assert_eq!(aws.secret_access_key.as_ref().map(|k| k.raw()), Some(""));
    assert!(p("br").disabled);
    let oauth = p("oa").oauth.as_ref().unwrap();
    assert_eq!(oauth.client_secret.as_deref(), Some(""));
    assert!(p("oa").disabled);
}

/// 留着的固定密钥绑的路由指向一个策略组，那一版里路由和组都没有：路由和组一起留着，恢复照样
/// 成。只选这条已删路由时，组一起恢复
#[test]
fn a_kept_route_that_points_at_a_group_keeps_the_group() {
    let now = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\n  - name: codex\n    key: tw-cx\n    route: fast\nproviders:\n  - name: p1\n    base_url: https://p1.example\n    key: sk-1\ngroups:\n  - name: g\n    type: fallback\n    providers: [p1]\nroutes:\n  - name: fast\n    rules:\n      - name: all\n        to: g\n"
    ));
    let then = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\nproviders:\n  - name: p1\n    base_url: https://p1.example\n    key: sk-1\n"
    ));
    let pinned: BTreeSet<String> = ["codex".to_string()].into();
    let rows = tw_pack::restore::preview(&now, &then, &pinned, true).unwrap();
    let kept = |kind, name: &str| {
        rows.iter()
            .find(|r| r.item == ItemId::new(kind, name))
            .and_then(|r| r.kept)
    };
    assert_eq!(kept(ItemKind::Key, "codex"), Some(Kept::KeyInUse));
    assert_eq!(kept(ItemKind::Route, "fast"), Some(Kept::Referenced));
    assert_eq!(kept(ItemKind::Group, "g"), Some(Kept::Referenced));
    let out = tw_pack::restore::apply(&now, &then, None, &pinned, true).unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
    assert_eq!(cfg.groups.len(), 1);

    // 反过来：现在没有路由和组，只选那一版里那条指向组的路由
    let now2 = then.clone();
    let then2 = now.clone();
    let pick = [ItemId::new(ItemKind::Route, "fast")];
    let out = tw_pack::restore::apply(&now2, &then2, Some(&pick), &BTreeSet::new(), true).unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml)
        .unwrap_or_else(|e| panic!("{e}\n{}", out.pack.config_yaml));
    assert_eq!(
        cfg.groups
            .iter()
            .map(|g| g.name.as_str())
            .collect::<Vec<_>>(),
        ["g"]
    );
    assert!(
        out.rows
            .iter()
            .any(|r| r.item == ItemId::new(ItemKind::Group, "g") && r.change == Change::Add)
    );
}

const SYNC: &str = "sync:\n  device_id: 3f9a2c1e-0b1d-4e2f-9a3b-5c6d7e8f9a0b\n  device_name: srv\n  encrypt: false\n  target:\n    type: webdav\n    url: https://dav.example.com/tw\n    user: u\n    pass: p\n    proxy: corp\n";

/// 同步设置走的出站代理那一版里没有：恢复留着它（本机那几节引用的），整份恢复从现在这份
/// 原文里一起取来
#[test]
fn the_proxy_the_sync_settings_use_stays() {
    let now = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\nproxies:\n  - name: corp\n    type: socks5h\n    addr: 127.0.0.1:7890\n{SYNC}"
    ));
    tw_config::try_parse(&now.config_yaml).unwrap();
    let then = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\n"
    ));
    let rows = tw_pack::restore::preview(&now, &then, &BTreeSet::new(), true).unwrap();
    let corp = ItemId::new(ItemKind::Proxy, "corp");
    assert_eq!(
        rows.iter().find(|r| r.item == corp).and_then(|r| r.kept),
        Some(Kept::Referenced)
    );
    let out = tw_pack::restore::apply(&now, &then, None, &BTreeSet::new(), true).unwrap();
    let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
    assert_eq!(cfg.proxies.len(), 1);
    assert_eq!(out.kept, [corp]);

    let synced = export(&then, true, PackOrigin::Sync);
    let w = tw_pack::restore::whole(
        &synced,
        &format!("{}not_a_field: 1\n", now.config_yaml),
        None,
    );
    let cfg =
        tw_config::try_parse(&w.config_yaml).unwrap_or_else(|e| panic!("{e}\n{}", w.config_yaml));
    assert_eq!(cfg.proxies[0].addr, "127.0.0.1:7890");
    assert_eq!(cfg.sync.unwrap().target.proxy(), "corp");
}

/// 恢复同步位置上的版本：本机不同步的上游（和随之不同步的路由）那一版里本来就没有，不列成
/// 删除，排列顺序里也照现在的位置
#[test]
fn restoring_a_synced_version_leaves_what_this_machine_does_not_sync() {
    let now = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\nproviders:\n  - name: a\n    base_url: https://a.example\n    key: sk-a\n  - name: ollama\n    base_url: http://127.0.0.1:11434\n  - name: b\n    base_url: https://b.example\n    key: sk-b\nroutes:\n  - name: local\n    rules:\n      - name: all\n        to: ollama\n"
    ));
    tw_config::try_parse(&now.config_yaml).unwrap();
    // 那一版：同步位置上的，没有 ollama 和指向它的路由；b 在 a 前面，a 的密钥不一样
    let then = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\nproviders:\n  - name: b\n    base_url: https://b.example\n    key: sk-b\n  - name: a\n    base_url: https://a.example\n    key: sk-a2\n"
    ));
    let l = tw_pack::model::snapshot(&now).unwrap();
    let b = tw_pack::model::snapshot(&then).unwrap();
    let skip = {
        // 和同步那一路的算法一样：不同步的上游，连同直接或间接引用它们的
        let mut out: BTreeSet<ItemId> = [ItemId::new(ItemKind::Provider, "ollama")].into();
        for s in [&l, &b] {
            for (id, it) in &s.items {
                if !id.kind.is_order() && it.refs.iter().any(|r| out.contains(r)) {
                    out.insert(id.clone());
                }
            }
        }
        out
    };
    assert!(skip.contains(&ItemId::new(ItemKind::Route, "local")));
    let opts = tw_pack::restore::Options {
        with_prefs: true,
        skip,
        ..Default::default()
    };
    let rows = tw_pack::restore::preview_opts(&now, &then, &opts).unwrap();
    assert!(
        rows.iter().all(|r| r.change != Change::Remove),
        "{:?}",
        rows.iter()
            .map(|r| (r.item.to_string(), r.change))
            .collect::<Vec<_>>()
    );
    let out = tw_pack::restore::apply_opts(&now, &then, &opts).unwrap();
    assert_eq!(out.rows, rows);
    let cfg = tw_config::try_parse(&out.pack.config_yaml).unwrap();
    let names: Vec<&str> = cfg.providers.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["b", "a", "ollama"]);
    assert_eq!(cfg.routes.len(), 1);
    let a = cfg.providers.iter().find(|p| p.name == "a").unwrap();
    assert_eq!(a.key.as_ref().map(|k| k.raw()), Some("sk-a2"));
}

/// 只选几项时，预览和写入走同一个计划：预览列出的就是写下去的
#[test]
fn a_partial_preview_is_what_the_restore_writes() {
    let now = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\nproviders:\n  - name: p1\n    base_url: https://p1.example\n    key: sk-1\n"
    ));
    let then = pack(&format!(
        "version: 1\nlisten:\n  control:\n    key: {KEY}\nclients:\n  - name: default\n    key: tw-a\nproxies:\n  - name: px\n    type: http\n    addr: 10.0.0.1:8080\nproviders:\n  - name: p1\n    base_url: https://p1.example\n    key: sk-1\n  - name: gone\n    base_url: https://gone.example\n    key: sk-g\n    proxy: px\n"
    ));
    let opts = tw_pack::restore::Options {
        with_prefs: true,
        items: Some([ItemId::new(ItemKind::Provider, "gone")].into()),
        ..Default::default()
    };
    let rows = tw_pack::restore::preview_opts(&now, &then, &opts).unwrap();
    let ids: BTreeSet<String> = rows.iter().map(|r| r.item.to_string()).collect();
    assert!(
        ids.contains(&ItemId::new(ItemKind::Proxy, "px").to_string()),
        "{ids:?}"
    );
    let out = tw_pack::restore::apply_opts(&now, &then, &opts).unwrap();
    assert_eq!(out.rows, rows);
}
