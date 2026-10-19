//! 两台设备（两个独立的数据目录、两份配置）经同一个同步位置交替修改、同时上传、冲突、离线、
//! 加密开关与改口令、版本过新、不同步的上游、已接管客户端的密钥。
//!
//! 设备这一边（[`Host`]）是最简单的那种：配置就是数据目录里的 `config.yaml`，应用就是校验
//! 之后写回去。控制面那一层（写入流程、事件）在 `tw-control` 的测试里另测。

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tw_pack::{Pack, PackKind, PackOrigin};
use tw_sync::engine::{ApplyError, Current, Decision, Engine, Host, RoundError, Settings, Take};
use tw_sync::store::{AnyStore, Folder};
use tw_sync::{Store, StoreError};
use tw_types::Msg;

const PASS: &str = "correct horse battery staple";

/// 一台设备的配置：控制面钥匙各不相同（本机设置，不该被同步走）
fn base_config(control_key_byte: char) -> String {
    format!(
        "version: 1\nlisten:\n  control:\n    key: {}\nclients:\n  - name: default\n    key: tw-default-key-0001\n",
        control_key_byte.to_string().repeat(64)
    )
}

#[derive(Clone)]
struct Dev {
    dir: Arc<tempfile::TempDir>,
    pinned: Arc<Mutex<BTreeSet<String>>>,
    /// 应用过几次，每次是从哪些设备来的
    applied: Arc<Mutex<Vec<String>>>,
}

impl Dev {
    fn new(key: char) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.yaml"), base_config(key)).unwrap();
        Self {
            dir: Arc::new(dir),
            pinned: Default::default(),
            applied: Default::default(),
        }
    }
    fn path(&self) -> PathBuf {
        self.dir.path().join("config.yaml")
    }
    fn text(&self) -> String {
        std::fs::read_to_string(self.path()).unwrap()
    }
    fn config(&self) -> tw_config::Config {
        tw_config::try_parse(&self.text()).unwrap_or_else(|e| panic!("{e}\n{}", self.text()))
    }
    /// 用户改配置：在原文上追加或者整段改
    fn edit(&self, f: impl FnOnce(&str) -> String) {
        let next = f(&self.text());
        tw_config::try_parse(&next).unwrap_or_else(|e| panic!("测试写坏了配置：{e}\n{next}"));
        std::fs::write(self.path(), next).unwrap();
    }
    fn provider(&self, name: &str) -> Option<tw_config::Provider> {
        self.config().providers.into_iter().find(|p| p.name == name)
    }
}

impl Host for Dev {
    fn current(&self) -> Result<Current, Msg> {
        let text = self.text();
        Ok(Current {
            pack: Pack {
                manifest: tw_pack::Manifest::new(PackKind::Full, PackOrigin::Ui),
                config_yaml: text.clone(),
                plugins: Default::default(),
                client_prefs: None,
            },
            version: tw_config::version_of(&text),
            changed_ms: 1,
        })
    }
    async fn apply(&self, pack: Pack, base_version: &str, from: &str) -> Result<(), ApplyError> {
        if tw_config::version_of(&self.text()) != base_version {
            return Err(ApplyError::Stale);
        }
        tw_config::try_parse(&pack.config_yaml).map_err(|r| ApplyError::Rejected(r.msg()))?;
        std::fs::write(self.path(), &pack.config_yaml).unwrap();
        self.applied.lock().unwrap().push(from.to_string());
        Ok(())
    }
    fn pinned_keys(&self) -> BTreeSet<String> {
        self.pinned.lock().unwrap().clone()
    }
    fn backup(&self, _content_id: &str) -> Option<Pack> {
        None
    }
}

fn settings(id: &str, name: &str, encrypt: bool, place: &Path) -> Settings {
    Settings {
        device_id: id.into(),
        device_name: name.into(),
        encrypt,
        passphrase: encrypt.then(|| PASS.to_string()),
        exclude: BTreeSet::new(),
        place: format!("folder:{}", place.display()),
        remote_core: false,
    }
}

const A_ID: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const B_ID: &str = "bbbbbbbb-0000-4000-8000-000000000002";

struct Bed {
    loc: tempfile::TempDir,
    a: Dev,
    b: Dev,
    ea: Engine<AnyStore, Dev>,
    eb: Engine<AnyStore, Dev>,
}

fn folder(loc: &Path) -> AnyStore {
    AnyStore::Folder(Folder::new(loc))
}

/// A 开启同步（同步位置空着），B 用同步数据加入
async fn bed(encrypt: bool) -> Bed {
    tw_sync::keyring::set_work_factor_for_tests(4);
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = (Dev::new('a'), Dev::new('b'));
    let mut ea = Engine::new(
        folder(loc.path()),
        a.clone(),
        a.dir.path(),
        settings(A_ID, "Mac mini", encrypt, loc.path()),
    );
    let mut eb = Engine::new(
        folder(loc.path()),
        b.clone(),
        b.dir.path(),
        settings(B_ID, "MacBook Air", encrypt, loc.path()),
    );
    assert!(ea.create().await.unwrap(), "空的同步位置开得起来");
    eb.join(None).await.unwrap();
    Bed { loc, a, b, ea, eb }
}

fn provider_yaml(name: &str, url: &str, key: &str) -> String {
    format!("  - name: {name}\n    base_url: {url}\n    key: {key}\n")
}

fn add_provider(text: &str, name: &str, url: &str, key: &str) -> String {
    if text.contains("\nproviders:\n") {
        text.replacen(
            "\nproviders:\n",
            &format!("\nproviders:\n{}", provider_yaml(name, url, key)),
            1,
        )
    } else {
        format!("{text}providers:\n{}", provider_yaml(name, url, key))
    }
}

fn versions(loc: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(loc.join("versions"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[tokio::test]
async fn edits_on_either_device_reach_the_other_and_local_settings_stay_put() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "relay", "https://relay.example/v1", "sk-relay"));
    let r = t.ea.round().await.unwrap();
    assert!(r.uploaded.is_some(), "A 的改动要上传");
    let r = t.eb.round().await.unwrap();
    assert_eq!(r.applied_from, ["Mac mini"]);
    let p = t.b.provider("relay").expect("B 收到了 relay");
    assert_eq!(
        p.key.as_ref().map(|k| k.raw()),
        Some("sk-relay"),
        "密钥随同步带过去"
    );
    assert!(
        t.b.text().contains(&"b".repeat(64)),
        "B 的控制面钥匙是它自己的"
    );

    // B 改，A 收
    t.b.edit(|s| s.replace("https://relay.example/v1", "https://relay2.example/v1"));
    assert!(t.eb.round().await.unwrap().uploaded.is_some());
    t.ea.round().await.unwrap();
    assert_eq!(
        t.a.provider("relay").unwrap().base_url,
        "https://relay2.example/v1"
    );
    assert!(t.a.text().contains(&"a".repeat(64)));

    // 都没变：不再上传、不再应用
    let before = versions(t.loc.path());
    let ra = t.ea.round().await.unwrap();
    let rb = t.eb.round().await.unwrap();
    assert!(ra.uploaded.is_none() && rb.uploaded.is_none());
    assert!(ra.applied_from.is_empty() && rb.applied_from.is_empty());
    assert_eq!(versions(t.loc.path()), before);
    // 同步位置只留最新与上一个
    assert!(before.len() <= 2, "{before:?}");
    // 两台设备都在设备列表里
    let names: Vec<String> = rb.devices.iter().map(|d| d.name.clone()).collect();
    assert_eq!(names, ["Mac mini", "MacBook Air"]);
}

#[tokio::test]
async fn different_objects_changed_on_both_sides_merge_without_a_conflict() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "one", "https://one.example", "sk-1"));
    t.b.edit(|s| add_provider(s, "two", "https://two.example", "sk-2"));
    t.ea.round().await.unwrap();
    let r = t.eb.round().await.unwrap();
    assert!(r.conflicts.is_empty(), "{:?}", r.conflicts);
    assert!(r.uploaded.is_some() && !r.applied_from.is_empty());
    t.ea.round().await.unwrap();
    for d in [&t.a, &t.b] {
        assert!(
            d.provider("one").is_some() && d.provider("two").is_some(),
            "{}",
            d.text()
        );
    }
}

/// 一个存储，**假装还没收到某些文件**：云盘里别的设备的版本还没传到这台设备上
struct Lagging {
    inner: AnyStore,
    hidden: Arc<Mutex<BTreeSet<String>>>,
}

impl Store for Lagging {
    async fn list(&self, dir: &str) -> Result<Vec<tw_sync::Entry>, StoreError> {
        let hidden = self.hidden.lock().unwrap().clone();
        Ok(self
            .inner
            .list(dir)
            .await?
            .into_iter()
            .filter(|e| !hidden.contains(&e.name))
            .collect())
    }
    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        if self
            .hidden
            .lock()
            .unwrap()
            .iter()
            .any(|h| path.ends_with(h.as_str()))
        {
            return Ok(None);
        }
        self.inner.get(path).await
    }
    async fn put_new(&self, path: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.inner.put_new(path, bytes).await
    }
    async fn put(&self, path: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.inner.put(path, bytes).await
    }
    async fn delete(&self, path: &str) -> Result<(), StoreError> {
        self.inner.delete(path).await
    }
    async fn create_exclusive(&self, path: &str, bytes: &[u8]) -> Result<bool, StoreError> {
        self.inner.create_exclusive(path, bytes).await
    }
}

/// 两台设备同时上传：序号相同的两个版本，第三台（这里是 A 自己的下一轮）以它们共同的上一版本
/// 为基准合并；合并之前它们和共同的上一版本都留着
#[tokio::test]
async fn simultaneous_uploads_are_merged_on_their_common_parent() {
    tw_sync::keyring::set_work_factor_for_tests(4);
    let t = bed(false).await;
    let hidden: Arc<Mutex<BTreeSet<String>>> = Default::default();
    let mut eb = Engine::new(
        Lagging {
            inner: folder(t.loc.path()),
            hidden: hidden.clone(),
        },
        t.b.clone(),
        t.b.dir.path(),
        settings(B_ID, "MacBook Air", false, t.loc.path()),
    );
    let mut ea = t.ea;
    t.a.edit(|s| add_provider(s, "one", "https://one.example", "sk-1"));
    t.b.edit(|s| add_provider(s, "two", "https://two.example", "sk-2"));
    let up_a = ea.round().await.unwrap().uploaded.unwrap();
    // B 看不到 A 刚传的那一版：它也传一个同序号的
    hidden.lock().unwrap().insert(format!("{up_a}.twconfig"));
    let up_b = eb.round().await.unwrap().uploaded.unwrap();
    assert_eq!(up_a[..10], up_b[..10], "同一个序号：{up_a} / {up_b}");
    let listed = versions(t.loc.path());
    assert_eq!(
        listed.len(),
        3,
        "两个并列的头和它们共同的上一版本都在：{listed:?}"
    );

    // 文件到了。A 的下一轮看到两个头，合并成一个新版本
    hidden.lock().unwrap().clear();
    let r = ea.round().await.unwrap();
    assert!(r.conflicts.is_empty(), "{:?}", r.conflicts);
    assert!(r.uploaded.is_some(), "并列的版本要合并成一个");
    assert!(t.a.provider("one").is_some() && t.a.provider("two").is_some());
    eb.round().await.unwrap();
    assert!(t.b.provider("one").is_some() && t.b.provider("two").is_some());
    let listed = versions(t.loc.path());
    assert!(
        listed.len() <= 3,
        "合并之后共同的上一版本删掉了：{listed:?}"
    );
}

/// 同一个上游的连接两边改得不一样：冲突。两台设备看到同一处冲突；冲突期间各用各的；
/// 一台处理之后，另一台随之清除，内容一致
#[tokio::test]
async fn a_conflict_is_the_same_on_both_devices_and_handled_once() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "relay", "https://relay.example", "sk-base"));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();

    t.a.edit(|s| s.replace("sk-base", "sk-from-a"));
    t.b.edit(|s| s.replace("sk-base", "sk-from-b"));
    // 另一个上游上互不相干的改动照常合并
    t.b.edit(|s| add_provider(s, "other", "https://other.example", "sk-o"));
    t.ea.round().await.unwrap();
    let rb = t.eb.round().await.unwrap();
    assert_eq!(rb.conflicts.len(), 1, "{:?}", rb.conflicts);
    let c = &rb.conflicts[0];
    assert_eq!(c.item.name, "relay");
    assert_eq!(c.units, ["conn"]);
    // 冲突期间各用各的
    assert_eq!(
        t.b.provider("relay").unwrap().key.unwrap().raw(),
        "sk-from-b"
    );
    let ra = t.ea.round().await.unwrap();
    assert_eq!(ra.conflicts.len(), 1);
    assert_eq!(ra.conflicts[0].id, c.id, "两台设备看到的是同一处冲突");
    assert_eq!(
        t.a.provider("relay").unwrap().key.unwrap().raw(),
        "sk-from-a"
    );
    assert!(t.a.provider("other").is_some(), "不相干的改动照常合并");
    // 再同步几轮也不会反复上传
    let before = versions(t.loc.path());
    t.eb.round().await.unwrap();
    t.ea.round().await.unwrap();
    assert_eq!(versions(t.loc.path()), before);

    // A 选 B 的
    let ra =
        t.ea.round_with(&[Decision {
            id: c.id.clone(),
            take: Take::Remote,
            new_name: None,
        }])
        .await
        .unwrap();
    assert!(ra.conflicts.is_empty());
    assert_eq!(
        t.a.provider("relay").unwrap().key.unwrap().raw(),
        "sk-from-b"
    );
    let rb = t.eb.round().await.unwrap();
    assert!(rb.conflicts.is_empty(), "B 上的这处冲突随之清除");
    assert_eq!(
        t.b.provider("relay").unwrap().key.unwrap().raw(),
        "sk-from-b"
    );
}

#[tokio::test]
async fn keeping_both_adds_the_other_side_under_the_device_name() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "relay", "https://relay.example", "sk-base"));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    t.a.edit(|s| s.replace("https://relay.example", "https://a.example"));
    t.b.edit(|s| s.replace("https://relay.example", "https://b.example"));
    t.ea.round().await.unwrap();
    let rb = t.eb.round().await.unwrap();
    let id = rb.conflicts[0].id.clone();
    let rb =
        t.eb.round_with(&[Decision {
            id,
            take: Take::Both,
            new_name: None,
        }])
        .await
        .unwrap();
    assert!(rb.conflicts.is_empty());
    assert_eq!(t.b.provider("relay").unwrap().base_url, "https://b.example");
    assert_eq!(
        t.b.provider("relay (Mac mini)").unwrap().base_url,
        "https://a.example"
    );
    t.ea.round().await.unwrap();
    assert_eq!(t.a.provider("relay").unwrap().base_url, "https://b.example");
    assert_eq!(
        t.a.provider("relay (Mac mini)").unwrap().base_url,
        "https://a.example"
    );
}

/// 一边删了、另一边改了：冲突；删了的一边选「本机」就是删
#[tokio::test]
async fn a_delete_against_an_edit_is_a_conflict() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "relay", "https://relay.example", "sk-base"));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    t.a.edit(|s| {
        s.replace(
            &provider_yaml("relay", "https://relay.example", "sk-base"),
            "",
        )
        .replace("providers:\n", "")
    });
    assert!(t.a.provider("relay").is_none());
    t.b.edit(|s| s.replace("sk-base", "sk-new"));
    t.ea.round().await.unwrap();
    let rb = t.eb.round().await.unwrap();
    assert_eq!(rb.conflicts.len(), 1, "{:?}", rb.conflicts);
    assert!(rb.conflicts[0].whole_item());
    assert!(!rb.conflicts[0].both_allowed());
    let ra = t.ea.round().await.unwrap();
    let id = ra.conflicts[0].id.clone();
    t.ea.round_with(&[Decision {
        id,
        take: Take::Local,
        new_name: None,
    }])
    .await
    .unwrap();
    t.eb.round().await.unwrap();
    assert!(t.b.provider("relay").is_none(), "{}", t.b.text());
}

/// 设备离线期间别的设备传了好几版，它当时的基准在同步位置上早删了：恢复连接后照本机基准合并，
/// 不报假冲突、不丢本机的改动
#[tokio::test]
async fn coming_back_after_the_base_was_deleted_still_merges() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "relay", "https://relay.example", "sk-1"));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    // B 离线。A 改了好几次
    for i in 2..6 {
        t.a.edit(|s| s.replace(&format!("sk-{}", i - 1), &format!("sk-{i}")));
        t.ea.round().await.unwrap();
    }
    let b_base = t.eb.state().base.unwrap();
    assert!(
        !versions(t.loc.path()).contains(&format!("{b_base}.twconfig")),
        "B 的基准在同步位置上已经删了"
    );
    t.b.edit(|s| add_provider(s, "offline", "https://offline.example", "sk-off"));
    let r = t.eb.round().await.unwrap();
    assert!(r.conflicts.is_empty(), "{:?}", r.conflicts);
    assert_eq!(t.b.provider("relay").unwrap().key.unwrap().raw(), "sk-5");
    t.ea.round().await.unwrap();
    assert!(t.a.provider("offline").is_some());
}

#[tokio::test]
async fn encryption_can_be_turned_on_off_and_its_passphrase_changed() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "relay", "https://relay.example", "sk-secret-value"));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();

    // A 开启加密：B 下一轮要口令
    t.ea.enable_encryption(PASS).await.unwrap();
    assert!(t.loc.path().join("keyring").exists());
    for v in versions(t.loc.path()) {
        let bytes = std::fs::read(t.loc.path().join("versions").join(&v)).unwrap();
        assert!(tw_pack::is_encrypted(&bytes), "未加密的版本删了：{v}");
        assert!(!String::from_utf8_lossy(&bytes).contains("sk-secret-value"));
    }
    assert!(matches!(t.eb.round().await, Err(RoundError::Locked(_))));
    assert!(t.eb.unlock("wrong passphrase").await.is_err());
    assert!(t.eb.unlock(PASS).await.unwrap());
    t.b.edit(|s| s.replace("sk-secret-value", "sk-after-encryption"));
    let r = t.eb.round().await.unwrap();
    assert!(r.encrypted && r.uploaded.is_some());
    t.ea.round().await.unwrap();
    assert_eq!(
        t.a.provider("relay").unwrap().key.unwrap().raw(),
        "sk-after-encryption"
    );

    // A 改口令：已有版本不变，B 要输入新口令
    let before = versions(t.loc.path());
    t.ea.change_passphrase(PASS, "a brand new passphrase")
        .await
        .unwrap();
    assert_eq!(versions(t.loc.path()), before, "改口令只重新加密 keyring");
    assert!(matches!(t.eb.round().await, Err(RoundError::Locked(_))));
    assert!(t.eb.unlock("a brand new passphrase").await.unwrap());
    t.eb.round().await.unwrap();

    // A 关闭加密：keyring 留着、改写成「已关闭」（有口令的人关的），版本不再加密；B 跟着变成
    // 不加密
    t.ea.disable_encryption("a brand new passphrase")
        .await
        .unwrap();
    let kr = std::fs::read(t.loc.path().join("keyring")).unwrap();
    assert!(tw_sync::keyring::marked_off(&kr));
    assert!(
        tw_sync::keyring::Keyring::open(&kr, "a brand new passphrase")
            .unwrap()
            .off()
    );
    for v in versions(t.loc.path()) {
        let bytes = std::fs::read(t.loc.path().join("versions").join(&v)).unwrap();
        assert!(!tw_pack::is_encrypted(&bytes), "加密过的版本删了：{v}");
    }
    let r = t.eb.round().await.unwrap();
    assert!(!r.encrypted, "同步位置不加密了，B 照着改设置");
    t.b.edit(|s| s.replace("sk-after-encryption", "sk-after-off"));
    assert!(t.eb.round().await.unwrap().uploaded.is_some());
    t.ea.round().await.unwrap();
    assert_eq!(
        t.a.provider("relay").unwrap().key.unwrap().raw(),
        "sk-after-off"
    );

    // 再开回来：B 用同一个口令接着同步
    t.ea.enable_encryption("a brand new passphrase")
        .await
        .unwrap();
    assert!(!tw_sync::keyring::marked_off(
        &std::fs::read(t.loc.path().join("keyring")).unwrap()
    ));
    let r = t.eb.round().await.unwrap();
    assert!(r.encrypted, "B 记着口令，解得开新的 keyring");
}

/// 两台设备同时开启加密同步：keyring 只建得成一份，后写的那台转去加入
#[tokio::test]
async fn only_one_keyring_is_ever_created() {
    tw_sync::keyring::set_work_factor_for_tests(4);
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = (Dev::new('a'), Dev::new('b'));
    let mut ea = Engine::new(
        folder(loc.path()),
        a.clone(),
        a.dir.path(),
        settings(A_ID, "A", true, loc.path()),
    );
    let mut eb = Engine::new(
        folder(loc.path()),
        b.clone(),
        b.dir.path(),
        settings(B_ID, "B", true, loc.path()),
    );
    assert!(ea.create().await.unwrap());
    assert!(!eb.create().await.unwrap(), "后到的一方要转入加入流程");
    eb.join(None).await.unwrap();
    assert!(eb.round().await.unwrap().encrypted);
}

/// 同步位置的最新版本由更新的 Core 写、本机读不了：暂停，不下载也不上传；设备记录里写明
#[tokio::test]
async fn a_version_from_a_newer_core_pauses_sync() {
    let mut t = bed(false).await;
    // 伪造一个更新的 Core 写的版本：配置里有本机不认识的字段
    let latest = versions(t.loc.path()).pop().unwrap();
    let bytes = std::fs::read(t.loc.path().join("versions").join(&latest)).unwrap();
    let mut pack = tw_pack::read(&bytes, tw_pack::Unseal::None).unwrap();
    let mut meta: tw_sync::record::Meta =
        serde_json::from_value(pack.manifest.sync.clone().unwrap()).unwrap();
    let parent = latest.trim_end_matches(".twconfig").to_string();
    meta.seq += 1;
    meta.device = "cccccccc-0000-4000-8000-000000000003".into();
    meta.parents = vec![parent];
    pack.config_yaml.push_str("a_field_from_the_future: true\n");
    pack.manifest.core_version = "9.0.0".into();
    pack.manifest.sync = Some(serde_json::to_value(&meta).unwrap());
    let name = format!("{:010}-cccccccc.twconfig", meta.seq);
    std::fs::write(
        t.loc.path().join("versions").join(&name),
        tw_pack::write(&pack, tw_pack::Seal::None).unwrap(),
    )
    .unwrap();

    t.b.edit(|s| add_provider(s, "local-only-for-now", "https://x.example", "sk"));
    let e = t.eb.round().await.unwrap_err();
    assert!(matches!(e, RoundError::TooNew { .. }), "{e:?}");
    assert!(
        !versions(t.loc.path())
            .iter()
            .any(|v| v.ends_with("-bbbbbbbb.twconfig") && v > &name),
        "暂停时不上传"
    );
    let ra = t.ea.round().await;
    assert!(matches!(ra, Err(RoundError::TooNew { .. })));
    // A 看得到 B 暂停着
    let probe = Engine::<AnyStore, Dev>::probe(&folder(t.loc.path()), None).await;
    let devs = probe.existing.unwrap().devices;
    assert!(devs.iter().all(|d| d.paused_update), "{devs:?}");
}

/// 不同步的上游：不上传，也不被别的设备的同名上游覆盖；引用它的路由随之不同步
#[tokio::test]
async fn an_excluded_upstream_and_what_refers_to_it_stay_local() {
    let mut t = bed(false).await;
    t.a.edit(|s| {
        let s = add_provider(s, "ollama", "http://127.0.0.1:11434", "none");
        format!(
            "{s}routes:\n  - name: local-first\n    rules:\n      - name: r\n        to: ollama\n"
        )
    });
    t.b.edit(|s| add_provider(s, "ollama", "http://192.168.1.9:11434", "none"));
    let mut sa = settings(A_ID, "Mac mini", false, t.loc.path());
    sa.exclude.insert("ollama".into());
    t.ea.set_settings(sa);
    let mut sb = settings(B_ID, "MacBook Air", false, t.loc.path());
    sb.exclude.insert("ollama".into());
    t.eb.set_settings(sb);
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    t.ea.round().await.unwrap();
    assert_eq!(
        t.a.provider("ollama").unwrap().base_url,
        "http://127.0.0.1:11434"
    );
    assert_eq!(
        t.b.provider("ollama").unwrap().base_url,
        "http://192.168.1.9:11434"
    );
    assert!(
        t.b.config().routes.is_empty(),
        "引用不同步上游的路由不上传：{}",
        t.b.text()
    );
    for v in versions(t.loc.path()) {
        let p = tw_pack::read(
            &std::fs::read(t.loc.path().join("versions").join(v)).unwrap(),
            tw_pack::Unseal::None,
        )
        .unwrap();
        assert!(!p.config_yaml.contains("ollama"), "{}", p.config_yaml);
    }
}

/// 本机已接管的客户端正在用的网关密钥被另一台设备删了：不删，作为冲突列出
#[tokio::test]
async fn a_key_in_use_here_is_not_deleted_by_another_device() {
    let mut t = bed(false).await;
    t.a.edit(|s| {
        s.replace(
            "clients:\n",
            "clients:\n  - name: codex\n    key: tw-codex-key-0002\n",
        )
    });
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    t.b.pinned.lock().unwrap().insert("codex".into());
    t.a.edit(|s| s.replace("  - name: codex\n    key: tw-codex-key-0002\n", ""));
    t.ea.round().await.unwrap();
    let r = t.eb.round().await.unwrap();
    assert!(
        t.b.config().clients.iter().any(|c| c.name == "codex"),
        "{}",
        t.b.text()
    );
    assert_eq!(r.conflicts.len(), 1);
    assert_eq!(
        r.conflicts[0].reason,
        Some(tw_sync::record::Reason::KeyInUse)
    );
}

/// 另一台设备停止同步并删除了同步数据：这台设备几轮、一段时间之后确认同步位置空了，由人
/// 决定（重新上传本机配置），同步设置和口令都不动
#[tokio::test]
async fn a_purged_location_is_noticed() {
    let mut t = bed(true).await;
    t.ea.stop(true).await.unwrap();
    assert!(versions(t.loc.path()).is_empty());
    assert!(!t.loc.path().join("keyring").exists());
    for _ in 0..3 {
        let r = t.eb.round().await;
        assert!(
            matches!(r, Err(RoundError::LooksEmpty)),
            "一轮读到空的不下结论：{:?}",
            r.map(|r| r.uploaded)
        );
    }
    // 过了十分钟（改本机记着的时刻）
    let mut st = t.eb.local().state();
    st.empty_since_ms = st.empty_since_ms.map(|t| t - 11 * 60 * 1000);
    t.eb.local().save_state(&st).unwrap();
    let r = t.eb.round().await;
    assert!(
        matches!(r, Err(RoundError::Emptied)),
        "{:?}",
        r.map(|r| r.uploaded)
    );
    assert!(t.eb.local().base().is_some(), "本机的同步状态不删");
    // 重新上传本机配置：keyring 放回这台记着的那一份，版本以本机配置上传
    t.b.edit(|s| add_provider(s, "kept", "https://kept.example", "sk-kept"));
    let r = t.eb.republish().await.unwrap();
    assert!(r.encrypted);
    assert!(t.loc.path().join("keyring").exists());
    assert_eq!(versions(t.loc.path()).len(), 1);
    // A 重新加入，拿到 B 的配置
    let mut ea = Engine::new(
        folder(t.loc.path()),
        t.a.clone(),
        t.a.dir.path(),
        settings(A_ID, "Mac mini", true, t.loc.path()),
    );
    ea.join(None).await.unwrap();
    assert!(t.a.provider("kept").is_some());
}

/// 同一套流程走 WebDAV 和 S3（进程内的假服务）
#[tokio::test]
async fn the_same_two_device_flow_over_webdav_and_s3() {
    tw_sync::keyring::set_work_factor_for_tests(4);
    let dav = common::webdav().await;
    let s3 = common::s3().await;
    let stores: Vec<Box<dyn Fn() -> AnyStore>> = vec![
        Box::new({
            let url = format!("{}/dav/root", dav.url());
            move || {
                AnyStore::WebDav(
                    tw_sync::store::WebDav::new(&url, common::USER, common::PASS, common::http())
                        .unwrap(),
                )
            }
        }),
        Box::new({
            let endpoint = s3.url();
            move || {
                AnyStore::S3(
                    tw_sync::store::S3::new(
                        tw_sync::store::s3::Config {
                            endpoint: endpoint.clone(),
                            region: "auto".into(),
                            bucket: "tw-bucket".into(),
                            prefix: "tw".into(),
                            access_key_id: common::AK.into(),
                            secret_access_key: common::SK.into(),
                        },
                        common::http(),
                    )
                    .unwrap(),
                )
            }
        }),
    ];
    for (i, mk) in stores.iter().enumerate() {
        let (a, b) = (Dev::new('a'), Dev::new('b'));
        let place = Path::new(if i == 0 { "/webdav" } else { "/s3" });
        let mut ea = Engine::new(
            mk(),
            a.clone(),
            a.dir.path(),
            settings(A_ID, "A", true, place),
        );
        let mut eb = Engine::new(
            mk(),
            b.clone(),
            b.dir.path(),
            settings(B_ID, "B", true, place),
        );
        assert!(ea.create().await.unwrap());
        eb.join(None).await.unwrap();
        a.edit(|s| add_provider(s, "relay", "https://relay.example", "sk-over-the-wire"));
        ea.round().await.unwrap();
        eb.round().await.unwrap();
        assert_eq!(
            b.provider("relay").unwrap().key.unwrap().raw(),
            "sk-over-the-wire"
        );
        b.edit(|s| s.replace("sk-over-the-wire", "sk-back"));
        eb.round().await.unwrap();
        ea.round().await.unwrap();
        assert_eq!(a.provider("relay").unwrap().key.unwrap().raw(), "sk-back");
    }
    // 同步位置上看不到明文密钥
    for f in dav.names().iter().chain(s3.names().iter()) {
        let bytes = dav.get(f).or_else(|| s3.get(f)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("sk-back"), "{f}");
    }
}
