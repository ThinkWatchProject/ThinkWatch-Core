//! 审查里找出来的同步问题，一条一条钉住：加密不可降级、等登录的上游、捷径不吞改动、基准不是
//! 祖先、晚到的头、一个坏文件挡住所有设备、同步位置读起来是空的、清除只删同步文件。
//!
//! 设备这一边和 `devices.rs` 一样是最简单的那种（配置就是数据目录里的 `config.yaml`），多一个
//! 钩子：一轮的中途（读完本机配置之后、上传之前）跑一次，模拟用户此刻改配置、别的设备此刻上传。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tw_pack::{Pack, PackKind, PackOrigin, Seal};
use tw_sync::engine::{
    ApplyError, Current, Decision, Engine, Host, RoundError, SKIP_AFTER_MS, Settings, Take,
};
use tw_sync::keyring::Keyring;
use tw_sync::store::{AnyStore, Folder};
use tw_sync::{Entry, Store, StoreError};
use tw_types::Msg;

const PASS: &str = "correct horse battery staple";

fn base_config(c: char) -> String {
    format!(
        "version: 1\nlisten:\n  control:\n    key: {}\nclients:\n  - name: default\n    key: tw-default-key-0001\n",
        c.to_string().repeat(64)
    )
}

type Hook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

#[derive(Clone)]
struct Dev {
    dir: Arc<tempfile::TempDir>,
    hook: Hook,
}

impl Dev {
    fn new(key: char) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.yaml"), base_config(key)).unwrap();
        Self {
            dir: Arc::new(dir),
            hook: Default::default(),
        }
    }
    fn path(&self) -> PathBuf {
        self.dir.path().join("config.yaml")
    }
    /// 通用偏好（和控制面一样存在配置旁边）
    fn prefs(&self) -> Option<serde_json::Value> {
        let b = std::fs::read(self.dir.path().join("prefs.json")).ok()?;
        serde_json::from_slice(&b).ok()
    }
    fn text(&self) -> String {
        std::fs::read_to_string(self.path()).unwrap()
    }
    fn config(&self) -> tw_config::Config {
        tw_config::try_parse(&self.text()).unwrap_or_else(|e| panic!("{e}\n{}", self.text()))
    }
    fn edit(&self, f: impl FnOnce(&str) -> String) {
        let next = f(&self.text());
        tw_config::try_parse(&next).unwrap_or_else(|e| panic!("测试写坏了配置：{e}\n{next}"));
        std::fs::write(self.path(), next).unwrap();
    }
    fn provider(&self, name: &str) -> Option<tw_config::Provider> {
        self.config().providers.into_iter().find(|p| p.name == name)
    }
    /// 一轮的中途跑一次
    fn during_next_round(&self, f: impl FnOnce() + Send + 'static) {
        *self.hook.lock().unwrap() = Some(Box::new(f));
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
                client_prefs: self.prefs(),
            },
            version: tw_config::version_of(&text),
            changed_ms: 1,
        })
    }
    async fn apply(&self, pack: Pack, base_version: &str, _from: &str) -> Result<(), ApplyError> {
        if tw_config::version_of(&self.text()) != base_version {
            return Err(ApplyError::Stale);
        }
        tw_config::try_parse(&pack.config_yaml).map_err(|r| ApplyError::Rejected(r.msg()))?;
        // 和控制面一样：写不进的偏好（超出上限）整个应用就失败
        if pack
            .client_prefs
            .as_ref()
            .is_some_and(tw_pack::disk::prefs_too_large)
        {
            return Err(ApplyError::Rejected(tw_types::msg!(
                "t.prefs" => "preferences too large"
            )));
        }
        std::fs::write(self.path(), &pack.config_yaml).unwrap();
        let prefs = self.dir.path().join("prefs.json");
        match &pack.client_prefs {
            Some(p) => std::fs::write(prefs, serde_json::to_vec(p).unwrap()).unwrap(),
            None => {
                let _ = std::fs::remove_file(prefs);
            }
        }
        Ok(())
    }
    fn pinned_keys(&self) -> BTreeSet<String> {
        if let Some(f) = self.hook.lock().unwrap().take() {
            f();
        }
        BTreeSet::new()
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
    assert!(ea.create().await.unwrap());
    eb.join(None).await.unwrap();
    Bed { loc, a, b, ea, eb }
}

fn add_provider_raw(text: &str, yaml: &str) -> String {
    if text.contains("\nproviders:\n") {
        text.replacen("\nproviders:\n", &format!("\nproviders:\n{yaml}"), 1)
    } else {
        format!("{text}providers:\n{yaml}")
    }
}

fn add_provider(text: &str, name: &str, url: &str, key: &str) -> String {
    add_provider_raw(
        text,
        &format!("  - name: {name}\n    base_url: {url}\n    key: {key}\n"),
    )
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

fn read_version(loc: &Path, id: &str) -> Vec<u8> {
    std::fs::read(loc.join("versions").join(format!("{id}.twconfig"))).unwrap()
}

/// 没有口令的人往同步位置里放一个不加密的版本（manifest.sync 照格式写、带 SHA-256），依据
/// 最新的那一个
fn forge_plain_version(loc: &Path, config_yaml: String) -> String {
    forge_plain_version_with(loc, config_yaml, None)
}

/// 同上，带着通用偏好
fn forge_plain_version_with(
    loc: &Path,
    config_yaml: String,
    client_prefs: Option<serde_json::Value>,
) -> String {
    let head = versions(loc)
        .last()
        .unwrap()
        .trim_end_matches(".twconfig")
        .to_string();
    let seq: u64 = head[..10].parse().unwrap();
    let mut pack = Pack {
        manifest: tw_pack::Manifest::new(PackKind::Full, PackOrigin::Sync),
        config_yaml,
        plugins: Default::default(),
        client_prefs,
    };
    let sha = tw_sync::engine::content_sha256(&pack);
    let meta = tw_sync::record::Meta {
        seq: seq + 1,
        device: "eeeeeeee-0000-4000-8000-00000000000e".into(),
        device_name: "Eve".into(),
        parents: vec![head],
        sha256: Some(sha),
        ..Default::default()
    };
    pack.manifest.sync = Some(serde_json::to_value(&meta).unwrap());
    let bytes = tw_pack::write(&pack, Seal::None).unwrap();
    let name = format!("{:010}-eeeeeeee.twconfig", seq + 1);
    std::fs::write(loc.join("versions").join(&name), bytes).unwrap();
    name
}

/// 把本机记着的「从什么时候起」往前拨，模拟时间过去了
fn age_state(e: &Engine<AnyStore, Dev>, by_ms: u64) {
    let mut st = e.local().state();
    for t in st.unreadable.values_mut().chain(st.arriving.values_mut()) {
        *t = t.saturating_sub(by_ms);
    }
    st.empty_since_ms = st.empty_since_ms.map(|t| t.saturating_sub(by_ms));
    e.local().save_state(&st).unwrap();
}

// ─────────────────────────────────────────────── 加密不可降级

/// 加密的同步位置：没有口令的人放进来一个不加密的版本 —— 不读、不合并，计入跳过
#[tokio::test]
async fn a_plaintext_version_in_an_encrypted_location_is_never_read() {
    let mut t = bed(true).await;
    t.a.edit(|s| add_provider(s, "relay", "https://relay.example", "sk-secret-relay"));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    assert!(t.b.provider("relay").is_some());
    let evil = add_provider(&t.a.text(), "evil", "https://evil.example", "sk-attacker");
    forge_plain_version(t.loc.path(), evil);
    let r = t.eb.round().await.unwrap();
    assert!(t.b.provider("evil").is_none(), "不加密的版本被合并了");
    assert!(r.encrypted);
    assert_eq!(r.skipped, 1, "跳过了，界面看得到");
}

/// keyring 被删、再放进一个不加密的版本：不降级、不上传明文、口令不删，要人处理。放回这台
/// 设备记着的 keyring 之后照常同步，伪造的那个版本照样不读
#[tokio::test]
async fn a_deleted_keyring_does_not_turn_encryption_off() {
    let mut t = bed(true).await;
    t.a.edit(|s| add_provider(s, "relay", "https://relay.example", "sk-secret-relay"));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    std::fs::remove_file(t.loc.path().join("keyring")).unwrap();
    forge_plain_version(t.loc.path(), t.a.text());
    t.b.edit(|s| add_provider(s, "bnew", "https://bnew.example", "sk-b-secret"));
    let before = versions(t.loc.path());
    let r = t.eb.round().await;
    assert!(
        matches!(r, Err(RoundError::KeyringMissing(_))),
        "{:?}",
        r.map(|r| r.uploaded)
    );
    assert_eq!(versions(t.loc.path()), before, "什么都没上传");
    assert!(t.eb.settings().encrypt && t.eb.settings().passphrase.is_some());
    // 再来一轮还是一样：不会过一会儿自己转成不加密
    assert!(matches!(
        t.eb.round().await,
        Err(RoundError::KeyringMissing(_))
    ));

    // 由人选「重新上传」：keyring 放回 B 记着的那一份
    let r = t.eb.republish().await.unwrap();
    assert!(r.encrypted);
    assert!(t.loc.path().join("keyring").exists());
    let up = r.uploaded.expect("B 的改动这时上传");
    assert!(tw_pack::is_encrypted(&read_version(t.loc.path(), &up)));
    assert!(t.b.provider("bnew").is_some());
    // A 用的还是同一把密钥，照常同步
    t.ea.round().await.unwrap();
    assert!(t.a.provider("bnew").is_some());
    assert!(t.a.provider("evil").is_none());
}

/// 关过一次加密、又开回来之后，有人把当年那份「已关闭」的 keyring 放回来（它当初是真的，
/// 口令也解得开）：代号比见过的小，不认
#[tokio::test]
async fn an_old_keyring_that_says_off_is_not_believed() {
    let mut t = bed(true).await;
    t.ea.disable_encryption(PASS).await.unwrap();
    let off = std::fs::read(t.loc.path().join("keyring")).unwrap();
    assert!(tw_sync::keyring::marked_off(&off));
    let r = t.eb.round().await.unwrap();
    assert!(!r.encrypted, "有口令的人关的：认");
    t.ea.enable_encryption(PASS).await.unwrap();
    assert!(t.eb.round().await.unwrap().encrypted);

    std::fs::write(t.loc.path().join("keyring"), &off).unwrap();
    t.b.edit(|s| add_provider(s, "secret", "https://s.example", "sk-not-for-plaintext"));
    let before = versions(t.loc.path());
    let r = t.eb.round().await;
    assert!(
        matches!(&r, Err(RoundError::KeyringMissing(m)) if m.code == "sync.keyring_rolled_back"),
        "{:?}",
        r.map(|r| r.encrypted)
    );
    assert_eq!(
        versions(t.loc.path()),
        before,
        "没有按旧的 keyring 上传明文"
    );
    // 放回 B 记着的那一份（较新的）
    let r = t.eb.republish().await.unwrap();
    assert!(r.encrypted);
    for v in versions(t.loc.path()) {
        let bytes = std::fs::read(t.loc.path().join("versions").join(&v)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("sk-not-for-plaintext"));
    }
}

/// 关了加密之后有人放进来一个只有明文标记的假 keyring（解不开）：记着口令的设备不认标记，
/// 要输入口令；没有口令的设备（本来就不加密）照常不加密同步
#[tokio::test]
async fn a_fake_off_marker_does_not_fool_a_device_with_the_passphrase() {
    let mut t = bed(true).await;
    t.ea.disable_encryption(PASS).await.unwrap();
    assert!(!t.eb.round().await.unwrap().encrypted);
    let mut fake = tw_sync::keyring::OFF_MARK.to_vec();
    fake.extend(Keyring::generate(0).seal("someone else's phrase"));
    std::fs::write(t.loc.path().join("keyring"), fake).unwrap();
    let r = t.eb.round().await;
    assert!(
        matches!(r, Err(RoundError::Locked(_))),
        "{:?}",
        r.map(|r| r.encrypted)
    );
    // 一台从没有过口令的设备照常
    let c = Dev::new('c');
    let mut s = settings(
        "cccccccc-0000-4000-8000-000000000003",
        "Mini PC",
        false,
        t.loc.path(),
    );
    s.passphrase = None;
    let mut ec = Engine::new(folder(t.loc.path()), c.clone(), c.dir.path(), s);
    assert!(!ec.join(None).await.unwrap().encrypted);
}

/// 加密的同步位置上，不加密的设备记录不列出来；之后开启加密时删掉不加密的设备记录
#[tokio::test]
async fn plaintext_device_records_do_not_count_once_encrypted() {
    let mut t = bed(false).await;
    let fake = serde_json::json!({
        "id": "cccccccc-0000-4000-8000-00000000000c", "name": "Fake", "core_version": "0.77.0",
        "last_seen_ms": 4102444800000u64, "paused_update": false, "remote_core": false
    });
    std::fs::write(
        t.loc.path().join("devices/cccccccc"),
        serde_json::to_vec(&fake).unwrap(),
    )
    .unwrap();
    let names: Vec<String> =
        t.ea.round()
            .await
            .unwrap()
            .devices
            .into_iter()
            .map(|d| d.name)
            .collect();
    assert!(
        names.contains(&"Fake".to_string()),
        "不加密时是一份普通的记录"
    );
    t.ea.enable_encryption(PASS).await.unwrap();
    assert!(
        !t.loc.path().join("devices/cccccccc").exists(),
        "开启加密时删掉不加密的设备记录"
    );
    std::fs::write(
        t.loc.path().join("devices/cccccccc"),
        serde_json::to_vec(&fake).unwrap(),
    )
    .unwrap();
    t.a.edit(|s| add_provider(s, "x", "https://x.example", "sk-x"));
    let names: Vec<String> =
        t.ea.round()
            .await
            .unwrap()
            .devices
            .into_iter()
            .map(|d| d.name)
            .collect();
    assert!(!names.contains(&"Fake".to_string()), "{names:?}");
    assert!(names.contains(&"Mac mini".to_string()));
}

// ─────────────────────────────────────────────── 捷径不吞改动

/// 一轮中途用户改了配置：下一轮照样上传
#[tokio::test]
async fn an_edit_made_during_a_round_is_uploaded_next_round() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "one", "https://one.example", "sk-1"));
    let a = t.a.clone();
    t.a.during_next_round(move || {
        a.edit(|s| add_provider(s, "two", "https://two.example", "sk-2"));
    });
    assert!(t.ea.round().await.unwrap().uploaded.is_some());
    assert!(t.a.provider("two").is_some());
    assert!(
        t.ea.round().await.unwrap().uploaded.is_some(),
        "中途的改动这一轮上传"
    );
    t.eb.round().await.unwrap();
    assert!(t.b.provider("two").is_some());
}

/// 一轮中途别的设备传了新版本：下一轮照样合并
#[tokio::test]
async fn a_version_arriving_during_a_round_is_merged_next_round() {
    let mut t = bed(false).await;
    t.b.edit(|s| add_provider(s, "fromb", "https://b.example", "sk-b"));
    let up_b = t.eb.round().await.unwrap().uploaded.unwrap();
    let parked = tempfile::tempdir().unwrap();
    let file = format!("{up_b}.twconfig");
    let in_loc = t.loc.path().join("versions").join(&file);
    let out = parked.path().join(&file);
    std::fs::rename(&in_loc, &out).unwrap();
    t.a.edit(|s| add_provider(s, "froma", "https://a.example", "sk-a"));
    t.a.during_next_round(move || std::fs::rename(&out, &in_loc).unwrap());
    t.ea.round().await.unwrap();
    assert!(t.a.provider("fromb").is_none(), "这一轮开始时还没到");
    t.ea.round().await.unwrap();
    assert!(t.a.provider("fromb").is_some());
    t.eb.round().await.unwrap();
    assert!(t.b.provider("froma").is_some());
}

// ─────────────────────────────────────────────── 等登录的上游

const CHATGPT: &str = "  - name: chatgpt\n    base_url: https://chatgpt.com/backend-api/codex\n    oauth:\n      refresh: rt-a\n      endpoint: https://auth.openai.com/oauth/token\n    headers:\n      ChatGPT-Account-Id: acct-a\n";
const CORP: &str = "  - name: corp\n    base_url: https://corp.example\n    oauth:\n      refresh: rt-corp-a\n      endpoint: https://auth.example/token\n";

/// A 上登录着的 ChatGPT 账号和 OAuth 上游到了没登录的 B：在 B 上停用着，没有冲突；A 上照旧
/// 启用、登录还在。B 登录之后照同步的样子启用
#[tokio::test]
async fn an_upstream_signed_in_elsewhere_waits_for_a_sign_in_here() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider_raw(&add_provider_raw(s, CHATGPT), CORP));
    t.ea.round().await.unwrap();
    let r = t.eb.round().await.unwrap();
    assert!(r.conflicts.is_empty(), "{:?}", r.conflicts);
    for n in ["chatgpt", "corp"] {
        let p = t.b.provider(n).unwrap_or_else(|| panic!("{n} 没到 B"));
        assert!(p.disabled, "{n} 在 B 上停用着");
    }
    assert!(t.b.provider("chatgpt").unwrap().oauth.is_none());
    assert_eq!(
        t.eb.state().held_login,
        BTreeSet::from(["chatgpt".to_string(), "corp".to_string()])
    );
    // 停用没有传回 A，也不会来回上传
    let listing = versions(t.loc.path());
    let ra = t.ea.round().await.unwrap();
    assert!(ra.uploaded.is_none() && ra.applied_from.is_empty());
    assert!(t.eb.round().await.unwrap().uploaded.is_none());
    assert_eq!(versions(t.loc.path()), listing);
    let a = t.a.provider("chatgpt").unwrap();
    assert!(!a.disabled);
    assert_eq!(a.oauth.unwrap().refresh, "rt-a");

    // A 改了并发：B 收到，还停用着
    t.a.edit(|s| {
        s.replace(
            "    base_url: https://corp.example\n",
            "    base_url: https://corp.example\n    max_concurrent: 3\n",
        )
    });
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    let corp = t.b.provider("corp").unwrap();
    assert_eq!(corp.max_concurrent, Some(3));
    assert!(corp.disabled);

    // B 上登录 corp（填上刷新令牌）：照同步的样子启用，A 不受影响
    t.b.edit(|s| {
        let mut cfg: serde_yaml_ng::Value = serde_yaml_ng::from_str(s).unwrap();
        for p in cfg["providers"].as_sequence_mut().unwrap() {
            if p["name"] == "corp" {
                p["oauth"]["refresh"] = "rt-corp-b".into();
            }
        }
        serde_yaml_ng::to_string(&cfg).unwrap()
    });
    let r = t.eb.round().await.unwrap();
    assert!(r.uploaded.is_none(), "登录是这台的事，不上传");
    let corp = t.b.provider("corp").unwrap();
    assert!(!corp.disabled, "登录之后启用");
    assert_eq!(corp.oauth.unwrap().refresh, "rt-corp-b");
    assert_eq!(
        t.eb.state().held_login,
        BTreeSet::from(["chatgpt".to_string()])
    );
    t.ea.round().await.unwrap();
    assert_eq!(
        t.a.provider("corp").unwrap().oauth.unwrap().refresh,
        "rt-corp-a"
    );
}

/// B 上自己改了等登录上游的别的设置：照常上传，停用不跟着走
#[tokio::test]
async fn editing_a_waiting_upstream_here_does_not_turn_it_off_elsewhere() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider_raw(s, CHATGPT));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    assert!(t.b.provider("chatgpt").unwrap().disabled);
    t.b.edit(|s| {
        s.replace(
            "    base_url: https://chatgpt.com/backend-api/codex\n",
            "    base_url: https://chatgpt.com/backend-api/codex\n    max_concurrent: 2\n",
        )
    });
    assert!(t.eb.round().await.unwrap().uploaded.is_some());
    t.ea.round().await.unwrap();
    let a = t.a.provider("chatgpt").unwrap();
    assert_eq!(a.max_concurrent, Some(2));
    assert!(!a.disabled, "B 上的停用没传过来");
    assert!(a.oauth.is_some());
}

/// 两台都登录了同名的 ChatGPT 上游（各自的账号）：A 改并发，B 收到后登录还是 B 自己的
#[tokio::test]
async fn two_devices_signed_in_to_one_chatgpt_upstream_keep_their_own_login() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider_raw(s, CHATGPT));
    t.b.edit(|s| {
        add_provider_raw(
            s,
            &CHATGPT.replace("rt-a", "rt-b").replace("acct-a", "acct-b"),
        )
    });
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    t.a.edit(|s| {
        s.replace(
            "    base_url: https://chatgpt.com/backend-api/codex\n",
            "    base_url: https://chatgpt.com/backend-api/codex\n    max_concurrent: 4\n",
        )
    });
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    let p = t.b.provider("chatgpt").unwrap();
    assert_eq!(p.max_concurrent, Some(4));
    assert!(!p.disabled);
    assert_eq!(p.oauth.unwrap().refresh, "rt-b");
}

// ─────────────────────────────────────────────── 冲突

/// 网关密钥的冲突选「两者都保留」：另一份是一把新的密钥，不是同一个值，也不归哪个客户端
#[tokio::test]
async fn keeping_both_gateway_keys_makes_the_copy_a_new_key() {
    let mut t = bed(false).await;
    t.a.edit(|s| {
        s.replace(
            "clients:\n",
            "clients:\n  - name: k2\n    key: tw-k2-value-0001\n    client: codex\n",
        )
    });
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    t.a.edit(|s| {
        s.replace(
            "    client: codex\n",
            "    client: codex\n    max_concurrent: 2\n",
        )
    });
    t.b.edit(|s| {
        s.replace(
            "    client: codex\n",
            "    client: codex\n    max_concurrent: 3\n",
        )
    });
    t.ea.round().await.unwrap();
    let rb = t.eb.round().await.unwrap();
    let c = rb.conflicts.iter().find(|c| c.item.name == "k2").unwrap();
    assert_eq!(
        tw_sync::engine::options(c),
        [Take::Local, Take::Remote, Take::Both]
    );
    let r =
        t.eb.round_with(&[Decision {
            id: c.id.clone(),
            take: Take::Both,
            new_name: None,
        }])
        .await
        .unwrap();
    assert!(r.conflicts.is_empty());
    let clients = t.b.config().clients;
    let orig = clients.iter().find(|c| c.name == "k2").unwrap();
    let copy = clients
        .iter()
        .find(|c| c.name.starts_with("k2 ("))
        .unwrap_or_else(|| panic!("{clients:?}"));
    assert_eq!(orig.key, "tw-k2-value-0001");
    assert_ne!(copy.key, orig.key, "另一份是一把新的密钥");
    assert!(copy.key.starts_with("tw-"));
    assert_eq!(copy.max_concurrent, Some(2), "另一份是另一边的设置");
    assert!(copy.client.is_none(), "新的那把不归哪个客户端");
}

/// 合并结果过不了校验的冲突（`invalid`）只能选本机的：选另一边是一个明确的错误
#[tokio::test]
async fn an_invalid_conflict_only_offers_this_side() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "old", "https://old.example", "sk-old"));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    t.a.edit(|s| {
        s.replace(
            "  - name: old\n    base_url: https://old.example\n    key: sk-old\n",
            "",
        )
        .replace("providers:\n", "")
    });
    t.b.edit(|s| {
        format!("{s}routes:\n  - name: viaold\n    rules:\n      - name: r\n        to: old\n")
    });
    t.eb.round().await.unwrap();
    let ra = t.ea.round().await.unwrap();
    let c = ra
        .conflicts
        .iter()
        .find(|c| c.reason == Some(tw_sync::record::Reason::Invalid))
        .unwrap_or_else(|| panic!("{:?}", ra.conflicts));
    assert_eq!(tw_sync::engine::options(c), [Take::Local]);
    let r =
        t.ea.round_with(&[Decision {
            id: c.id.clone(),
            take: Take::Remote,
            new_name: None,
        }])
        .await;
    assert!(
        matches!(&r, Err(RoundError::Local(m)) if m.code == "sync.take_not_allowed"),
        "{:?}",
        r.map(|r| r.conflicts.len())
    );
    let r =
        t.ea.round_with(&[Decision {
            id: c.id.clone(),
            take: Take::Local,
            new_name: None,
        }])
        .await
        .unwrap();
    assert!(r.conflicts.is_empty());
}

/// 全局设置：一边把一节改回默认（这一项从快照里消失），另一边改了这一节的另一个字段 ——
/// 各管各的字段，不是冲突
#[tokio::test]
async fn a_global_section_reset_to_defaults_merges_field_by_field() {
    let mut t = bed(false).await;
    t.a.edit(|s| format!("{s}failover:\n  pause_secs: 100\n"));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    assert_eq!(t.b.config().failover.pause_secs, 100);
    let default_pause = tw_config::Config::default().failover.pause_secs;
    t.a.edit(|s| s.replace("failover:\n  pause_secs: 100\n", ""));
    t.b.edit(|s| {
        s.replace(
            "failover:\n  pause_secs: 100\n",
            "failover:\n  pause_secs: 100\n  failures_to_pause: 7\n",
        )
    });
    t.ea.round().await.unwrap();
    let rb = t.eb.round().await.unwrap();
    assert!(rb.conflicts.is_empty(), "{:?}", rb.conflicts);
    let f = t.b.config().failover;
    assert_eq!((f.pause_secs, f.failures_to_pause), (default_pause, 7));
    t.ea.round().await.unwrap();
    let f = t.a.config().failover;
    assert_eq!((f.pause_secs, f.failures_to_pause), (default_pause, 7));
}

/// 合并结果过不了校验时逐项回退：另一边新增的「上游 + 它用的出站代理」，出站代理先到位，上游
/// 不算过不了校验
#[tokio::test]
async fn a_new_upstream_with_its_new_proxy_is_not_taken_for_invalid() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "old", "https://old.example", "sk-old"));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    t.a.edit(|s| {
        s.replace(
            "  - name: old\n    base_url: https://old.example\n    key: sk-old\n",
            "",
        )
        .replace("providers:\n", "")
    });
    t.b.edit(|s| {
        let s = add_provider_raw(
            s,
            "  - name: q\n    base_url: https://q.example\n    key: sk-q\n    proxy: hk\n",
        );
        format!(
            "{s}proxies:\n  - name: hk\n    type: socks5h\n    addr: 127.0.0.1:7890\nroutes:\n  - name: viaold\n    rules:\n      - name: r\n        to: old\n"
        )
    });
    t.eb.round().await.unwrap();
    let ra = t.ea.round().await.unwrap();
    assert!(
        !ra.conflicts
            .iter()
            .any(|c| c.item.name == "q" || c.item.name == "hk"),
        "{:?}",
        ra.conflicts
    );
    assert!(t.a.provider("q").is_some());
}

// ─────────────────────────────────────────────── 谱系

/// 云盘晚到：B 依据 v3 传了一个版本，A 那边还没看到时又传了两个、删掉了 v3。B 的版本到了
/// 之后，两个头共同的上一版本只在 A 之前的基准里：照样三方合并，两边改同一个上游的不同单位
/// 不是冲突
#[tokio::test]
async fn a_late_head_merges_three_way_after_its_parent_was_pruned() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "relay", "https://relay.example", "sk-r"));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    let hidden: Arc<Mutex<BTreeSet<String>>> = Default::default();
    let mut ea = Engine::new(
        Lagging {
            inner: folder(t.loc.path()),
            hidden: hidden.clone(),
        },
        t.a.clone(),
        t.a.dir.path(),
        settings(A_ID, "Mac mini", false, t.loc.path()),
    );
    t.b.edit(|s| s.replace("    key: sk-r\n", "    key: sk-r\n    max_concurrent: 3\n"));
    let up_b = t.eb.round().await.unwrap().uploaded.unwrap();
    hidden.lock().unwrap().insert(format!("{up_b}.twconfig"));
    t.a.edit(|s| s.replace("    key: sk-r\n", "    key: sk-r\n    models: [m1]\n"));
    ea.round().await.unwrap();
    t.a.edit(|s| s.replace("    models: [m1]\n", "    models: [m1, m2]\n"));
    ea.round().await.unwrap();
    hidden.lock().unwrap().clear();
    let r = ea.round().await.unwrap();
    assert!(r.conflicts.is_empty(), "{:?}", r.conflicts);
    let p = t.a.provider("relay").unwrap();
    assert_eq!(p.max_concurrent, Some(3));
}

/// 不止一个头时，删旧版本留着它们共同的上一版本
#[tokio::test]
async fn pruning_keeps_the_common_parent_of_two_heads() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "relay", "https://relay.example", "sk-r"));
    t.ea.round().await.unwrap();
    t.eb.round().await.unwrap();
    let parent = versions(t.loc.path()).last().unwrap().clone();
    // 两台同时上传：两个头，依据同一个版本
    t.a.edit(|s| s.replace("    key: sk-r\n", "    key: sk-r\n    models: [m1]\n"));
    t.b.edit(|s| s.replace("    key: sk-r\n", "    key: sk-r\n    max_concurrent: 3\n"));
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
    let up_a = t.ea.round().await.unwrap().uploaded.unwrap();
    hidden.lock().unwrap().insert(format!("{up_a}.twconfig"));
    eb.round().await.unwrap();
    hidden.lock().unwrap().clear();
    assert!(versions(t.loc.path()).contains(&parent));
    // A 合并两个头之后：共同的上一版本还在，A 的合并没有冲突
    t.a.edit(|s| s.replace("    models: [m1]\n", "    models: [m1, m2]\n"));
    let r = t.ea.round().await.unwrap();
    assert!(r.conflicts.is_empty(), "{:?}", r.conflicts);
    assert_eq!(t.a.provider("relay").unwrap().max_concurrent, Some(3));
}

/// 云盘上两台几乎同时开启加密同步：B 那一刻还没收到 A 的 keyring 和第一个版本，于是新建，
/// 覆盖了 A 的 keyring。A 自己的版本这时解不开了：A 照两方比较合并、以新的 keyring 重新上传，
/// 删掉自己那个解不开的版本 —— 两台的配置都在
#[tokio::test]
async fn a_keyring_race_on_a_cloud_folder_loses_nothing() {
    tw_sync::keyring::set_work_factor_for_tests(4);
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = (Dev::new('a'), Dev::new('b'));
    a.edit(|s| add_provider(s, "one", "https://one.example", "sk-1"));
    b.edit(|s| add_provider(s, "two", "https://two.example", "sk-2"));
    let hidden: Arc<Mutex<BTreeSet<String>>> = Default::default();
    let mut ea = Engine::new(
        folder(loc.path()),
        a.clone(),
        a.dir.path(),
        settings(A_ID, "Mac mini", true, loc.path()),
    );
    let mut eb = Engine::new(
        Racy {
            inner: folder(loc.path()),
            hidden: hidden.clone(),
        },
        b.clone(),
        b.dir.path(),
        settings(B_ID, "MacBook Air", true, loc.path()),
    );
    assert!(ea.create().await.unwrap());
    hidden.lock().unwrap().extend([
        "keyring".to_string(),
        "0000000001-aaaaaaaa.twconfig".to_string(),
    ]);
    assert!(eb.create().await.unwrap(), "B 没看到 A 的，自己新建");
    hidden.lock().unwrap().clear();
    let r = ea.round().await.unwrap();
    assert!(r.uploaded.is_some());
    assert!(a.provider("one").is_some(), "A 自己的上游还在");
    assert!(a.provider("two").is_some());
    assert!(
        !versions(loc.path()).contains(&"0000000001-aaaaaaaa.twconfig".to_string()),
        "解不开的那个自己的版本删了"
    );
    eb.round().await.unwrap();
    assert!(b.provider("one").is_some());
    assert!(b.provider("two").is_some());
}

/// 两台几乎同时开启不加密的同步：两个不依据别的版本的第一个版本，按两方比较合并
#[tokio::test]
async fn two_first_versions_are_merged_two_way() {
    let loc = tempfile::tempdir().unwrap();
    let (a, b) = (Dev::new('a'), Dev::new('b'));
    a.edit(|s| add_provider(s, "one", "https://one.example", "sk-1"));
    b.edit(|s| add_provider(s, "two", "https://two.example", "sk-2"));
    let hidden: Arc<Mutex<BTreeSet<String>>> = Default::default();
    let mut ea = Engine::new(
        folder(loc.path()),
        a.clone(),
        a.dir.path(),
        settings(A_ID, "Mac mini", false, loc.path()),
    );
    let mut eb = Engine::new(
        Lagging {
            inner: folder(loc.path()),
            hidden: hidden.clone(),
        },
        b.clone(),
        b.dir.path(),
        settings(B_ID, "MacBook Air", false, loc.path()),
    );
    assert!(ea.create().await.unwrap());
    hidden
        .lock()
        .unwrap()
        .insert("0000000001-aaaaaaaa.twconfig".to_string());
    assert!(eb.create().await.unwrap());
    hidden.lock().unwrap().clear();
    let r = ea.round().await.unwrap();
    assert!(r.conflicts.is_empty(), "{:?}", r.conflicts);
    assert!(a.provider("two").is_some());
    eb.round().await.unwrap();
    assert!(b.provider("one").is_some());
}

/// 开启加密时 keyring 建好了、第一个版本没传上去：开启（口令对）接着传，加入也一样
#[tokio::test]
async fn a_location_with_only_a_keyring_can_be_turned_on() {
    tw_sync::keyring::set_work_factor_for_tests(4);
    for join in [false, true] {
        let loc = tempfile::tempdir().unwrap();
        let a = Dev::new('a');
        a.edit(|s| add_provider(s, "one", "https://one.example", "sk-1"));
        folder(loc.path())
            .create_exclusive("keyring", &Keyring::generate(0).seal(PASS))
            .await
            .unwrap();
        let mut ea = Engine::new(
            folder(loc.path()),
            a.clone(),
            a.dir.path(),
            settings(A_ID, "Mac mini", true, loc.path()),
        );
        if join {
            assert!(ea.join(None).await.unwrap().encrypted);
        } else {
            assert!(ea.create().await.unwrap());
        }
        assert_eq!(versions(loc.path()).len(), 1);
        let (b, bdir) = (Dev::new('b'), tempfile::tempdir().unwrap());
        let mut eb = Engine::new(
            folder(loc.path()),
            b.clone(),
            bdir.path(),
            settings(B_ID, "MacBook Air", true, loc.path()),
        );
        eb.join(None).await.unwrap();
        assert!(b.provider("one").is_some());
    }
}

// ─────────────────────────────────────────────── 读不出来的文件

/// 最新一层有一个坏文件（不是配置包）：马上跳过，计数；本机改动照常上传，别的设备照常收到
#[tokio::test]
async fn a_broken_file_on_top_does_not_stop_sync() {
    let mut t = bed(true).await;
    std::fs::write(
        t.loc.path().join("versions/9999999999-cccccccc.twconfig"),
        b"garbage",
    )
    .unwrap();
    t.a.edit(|s| add_provider(s, "one", "https://one.example", "sk-1"));
    let r = t.ea.round().await.unwrap();
    assert!(r.uploaded.is_some());
    assert_eq!(r.skipped, 1);
    let r = t.eb.round().await.unwrap();
    assert_eq!(r.skipped, 1);
    assert!(t.b.provider("one").is_some());
}

/// 最新一层有一个看上去没传完的版本：等着（不合并一半）；等了 30 分钟还读不出来就跳过
#[tokio::test]
async fn a_version_that_never_finishes_arriving_is_skipped_after_a_while() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "one", "https://one.example", "sk-1"));
    let up = t.ea.round().await.unwrap().uploaded.unwrap();
    // B 那边：最新的那个只到了一半
    let bytes = read_version(t.loc.path(), &up);
    let parked = tempfile::tempdir().unwrap();
    std::fs::write(parked.path().join("whole"), &bytes).unwrap();
    let seq: u64 = up[..10].parse().unwrap();
    let half = format!("{:010}-cccccccc.twconfig", seq + 1);
    std::fs::write(
        t.loc.path().join("versions").join(&half),
        &bytes[..bytes.len() / 2],
    )
    .unwrap();
    assert!(matches!(t.eb.round().await, Err(RoundError::NotArrived)));
    assert!(t.b.provider("one").is_none(), "不拿一半去合并");
    age_state(&t.eb, SKIP_AFTER_MS + 1000);
    let r = t.eb.round().await.unwrap();
    assert_eq!(r.skipped, 1);
    assert!(t.b.provider("one").is_some());
}

/// iCloud 还没下载的最新版本（只有旧式占位文件）：当作尚未到达，**不算读不出来**——过了
/// 30 分钟照样等（文件夹位置请求了下载）；一天还没到才跳过
#[tokio::test]
async fn an_icloud_placeholder_on_top_is_waited_for() {
    let mut t = bed(false).await;
    t.a.edit(|s| add_provider(s, "one", "https://one.example", "sk-1"));
    let up = t.ea.round().await.unwrap().uploaded.unwrap();
    let file = format!("{up}.twconfig");
    let real = t.loc.path().join("versions").join(&file);
    let parked = tempfile::tempdir().unwrap();
    std::fs::rename(&real, parked.path().join(&file)).unwrap();
    std::fs::write(
        t.loc
            .path()
            .join("versions")
            .join(format!(".{file}.icloud")),
        b"placeholder",
    )
    .unwrap();
    assert!(matches!(t.eb.round().await, Err(RoundError::NotArrived)));
    age_state(&t.eb, SKIP_AFTER_MS + 1000);
    assert!(
        matches!(t.eb.round().await, Err(RoundError::NotArrived)),
        "占位文件不按读不出来的 30 分钟跳过"
    );
    std::fs::rename(parked.path().join(&file), &real).unwrap();
    t.eb.round().await.unwrap();
    assert!(t.b.provider("one").is_some());
    assert!(t.eb.state().arriving.is_empty());
}

/// 一直没下载下来的占位文件：一天之后跳过，不再挡着这台设备上传
#[tokio::test]
async fn an_icloud_placeholder_that_never_downloads_is_skipped_after_a_day() {
    let mut t = bed(false).await;
    let up = t.ea.round().await.unwrap().uploaded;
    assert!(up.is_none());
    let head = versions(t.loc.path()).last().unwrap().clone();
    let seq: u64 = head[..10].parse().unwrap();
    std::fs::write(
        t.loc
            .path()
            .join("versions")
            .join(format!(".{:010}-cccccccc.twconfig.icloud", seq + 1)),
        b"placeholder",
    )
    .unwrap();
    t.b.edit(|s| add_provider(s, "fromb", "https://b.example", "sk-b"));
    assert!(matches!(t.eb.round().await, Err(RoundError::NotArrived)));
    age_state(&t.eb, tw_sync::engine::ARRIVING_SKIP_AFTER_MS + 1000);
    let r = t.eb.round().await.unwrap();
    assert_eq!(r.skipped, 1);
    assert!(r.uploaded.is_some());
}

// ─────────────────────────────────────────────── 太大的通用偏好

/// 同步位置上的通用偏好超出上限（别处写进来的，这台写不进）：偏好不应用、其余照常，报
/// `sync.prefs_too_large`，**不算失败**；之后上传的版本也不拿本机的偏好去换掉同步位置上的
#[tokio::test]
async fn oversized_preferences_are_left_out_and_the_rest_applies() {
    let mut t = bed(false).await;
    let big = serde_json::json!({ "notes": "x".repeat(70 * 1024) });
    assert!(tw_pack::disk::prefs_too_large(&big));
    let cfg = add_provider(&t.a.text(), "elsewhere", "https://e.example", "sk-e");
    forge_plain_version_with(t.loc.path(), cfg, Some(big.clone()));
    let r = t.eb.round().await.unwrap();
    let codes: Vec<&str> = r.notices.iter().map(|m| m.code.as_str()).collect();
    assert_eq!(codes, ["sync.prefs_too_large"]);
    assert!(t.b.provider("elsewhere").is_some(), "其余照常应用");
    assert!(r.uploaded.is_none(), "不拿本机的偏好去换");
    // 下一轮（走捷径）还说着
    let r = t.eb.round().await.unwrap();
    assert_eq!(r.notices.len(), 1);
    // B 改了别的：上传的版本里偏好还是同步位置上的那一份
    t.b.edit(|s| add_provider(s, "bnew", "https://bnew.example", "sk-bn"));
    let up = t.eb.round().await.unwrap().uploaded.unwrap();
    let p = tw_pack::read(&read_version(t.loc.path(), &up), tw_pack::Unseal::None).unwrap();
    assert_eq!(p.client_prefs, Some(big));
    assert_eq!(t.b.prefs(), None, "B 的偏好没动");
    // 同步位置上的偏好变回不大的：照常落地（B 的偏好一直没动过，不算 B 改了），提示没了
    let small = serde_json::json!({ "notes": "short" });
    forge_plain_version_with(t.loc.path(), t.b.text(), Some(small.clone()));
    let r = t.eb.round().await.unwrap();
    assert!(r.notices.is_empty(), "{:?}", r.notices);
    assert!(r.conflicts.is_empty(), "{:?}", r.conflicts);
    assert_eq!(t.b.prefs(), Some(small.clone()));
    t.ea.round().await.unwrap();
    assert_eq!(t.a.prefs(), Some(small));
}

// ─────────────────────────────────────────────── 空的同步位置

/// 文件夹位置的根目录还在、里面暂时是空的（网络盘没挂上时挂载点是个空目录）：不当作清空，
/// 挂回来之后照常同步
#[tokio::test]
async fn an_empty_mount_point_is_not_taken_for_a_purge() {
    let mut t = bed(false).await;
    let parked = tempfile::tempdir().unwrap();
    for name in ["versions", "devices"] {
        std::fs::rename(t.loc.path().join(name), parked.path().join(name)).unwrap();
    }
    for _ in 0..5 {
        assert!(matches!(t.ea.round().await, Err(RoundError::LooksEmpty)));
    }
    // 过了很久也一样：自己的设备记录不在只是没挂上的一种样子，结论要人下
    age_state(&t.ea, 60 * 60 * 1000);
    assert!(matches!(t.ea.round().await, Err(RoundError::Emptied)));
    assert!(t.ea.local().base().is_some(), "本机的同步状态不删");
    for name in ["versions", "devices"] {
        std::fs::rename(parked.path().join(name), t.loc.path().join(name)).unwrap();
    }
    t.a.edit(|s| add_provider(s, "back", "https://back.example", "sk-b"));
    assert!(t.ea.round().await.unwrap().uploaded.is_some());
    t.eb.round().await.unwrap();
    assert!(t.b.provider("back").is_some());
}

/// 停止同步并删除同步数据：只删认得出的同步文件，用户自己放进去的不动
#[tokio::test]
async fn purging_leaves_files_that_are_not_sync_data() {
    let mut t = bed(false).await;
    std::fs::write(t.loc.path().join("devices/notes.txt"), b"mine").unwrap();
    std::fs::write(t.loc.path().join("versions/readme.md"), b"mine").unwrap();
    t.ea.stop(true).await.unwrap();
    assert!(versions(t.loc.path()).iter().all(|n| n == "readme.md"));
    assert!(t.loc.path().join("devices/notes.txt").exists());
    assert!(t.loc.path().join("versions/readme.md").exists());
    assert!(!t.loc.path().join("devices/bbbbbbbb").exists());
}

// ─────────────────────────────────────────────── 测试用的存储

/// 云盘晚到：藏起来的文件列不出、读不到
struct Lagging {
    inner: AnyStore,
    hidden: Arc<Mutex<BTreeSet<String>>>,
}

impl Lagging {
    fn hides(&self, path: &str) -> bool {
        self.hidden
            .lock()
            .unwrap()
            .iter()
            .any(|h| path.ends_with(h.as_str()))
    }
}

impl Store for Lagging {
    async fn list(&self, dir: &str) -> Result<Vec<Entry>, StoreError> {
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
        if self.hides(path) {
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

/// 云盘上的「仅当不存在时创建」管不了别的设备：藏起来的文件当作不存在，照写不误
struct Racy {
    inner: AnyStore,
    hidden: Arc<Mutex<BTreeSet<String>>>,
}

impl Racy {
    fn hides(&self, path: &str) -> bool {
        self.hidden
            .lock()
            .unwrap()
            .iter()
            .any(|h| path.ends_with(h.as_str()))
    }
}

impl Store for Racy {
    async fn list(&self, dir: &str) -> Result<Vec<Entry>, StoreError> {
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
        if self.hides(path) {
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
        if self.hides(path) {
            self.inner.put(path, bytes).await?;
            return Ok(true);
        }
        self.inner.create_exclusive(path, bytes).await
    }
}
