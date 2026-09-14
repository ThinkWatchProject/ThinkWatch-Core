//! 双向同步端到端：两个写入方，一份文件。
//!
//! 这些测试盯的是**回环、冲突、以及坏配置进来时旧的还在不在**。前两个
//! 的失败模式都很隐蔽：回环表现为 CPU 莫名其妙地转，冲突表现为「我改的
//! 东西不见了」 ——后者是这个项目最不能犯的错（那批 cc-switch issue）。

use std::sync::Arc;
use std::time::Duration;

use tw_config::history::Origin;
use tw_control::{ApplyError, ConfigManager};

const BASE: &str = "version: 1\n# 别动我这句注释\nclients:\n  - name: c\n    key: tw-k\n";

fn setup() -> (tempfile::TempDir, Arc<ConfigManager>, tw_observe::EventBus) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, BASE).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(BASE).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    (d, Arc::new(ConfigManager::new(p, gw, bus.clone())), bus)
}

async fn next_config_event(
    rx: &mut tokio::sync::broadcast::Receiver<tw_api::Event>,
) -> tw_api::Event {
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("3 秒内没等到配置事件")
            .unwrap();
        if matches!(
            ev,
            tw_api::Event::ConfigReloaded { .. } | tw_api::Event::ConfigRejected { .. }
        ) {
            return ev;
        }
    }
}

#[tokio::test]
async fn an_external_edit_is_picked_up_and_takes_effect() {
    let (_d, mgr, bus) = setup();
    let mut rx = bus.subscribe();
    let _w = tw_control::spawn_watcher(mgr.clone()).unwrap();

    let next = format!("{BASE}providers:\n  - name: a\n    base_url: https://x\n    key: k\n");
    std::fs::write(mgr.path(), &next).unwrap();

    match next_config_event(&mut rx).await {
        tw_api::Event::ConfigReloaded { origin, .. } => assert_eq!(origin, "外部编辑"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn our_own_write_does_not_come_back_as_an_external_edit() {
    // **这是回环的判据。**没有它，我们写一次文件，监听响一次，我们再
    // 重载一次 —— 严重时是个循环，而它的表现只是 CPU 莫名其妙地转。
    let (_d, mgr, bus) = setup();
    let mut rx = bus.subscribe();
    let _w = tw_control::spawn_watcher(mgr.clone()).unwrap();

    let next = format!("{BASE}providers: []\n");
    mgr.write(&next, None, Origin::Ui).await.unwrap();

    // 第一个事件是我们自己那次写
    match next_config_event(&mut rx).await {
        tw_api::Event::ConfigReloaded { origin, .. } => assert_eq!(origin, "界面"),
        other => panic!("{other:?}"),
    }
    // 之后不该再冒出一个「外部编辑」
    let stray = tokio::time::timeout(Duration::from_millis(1200), async {
        loop {
            if let Ok(tw_api::Event::ConfigReloaded { origin, .. }) = rx.recv().await
                && origin == "外部编辑"
            {
                return;
            }
        }
    })
    .await;
    assert!(stray.is_err(), "自己写的又被当成外部改动重载了一遍");
}

#[tokio::test]
async fn a_broken_file_keeps_the_old_config_serving_and_says_where() {
    // 桌面工具不能因为一个笔误就断线。
    let (_d, mgr, bus) = setup();
    let mut rx = bus.subscribe();
    let _w = tw_control::spawn_watcher(mgr.clone()).unwrap();

    std::fs::write(
        mgr.path(),
        "version: 1\nclients:\n  - name: c\n    kye: tw-k\n",
    )
    .unwrap();
    match next_config_event(&mut rx).await {
        tw_api::Event::ConfigRejected {
            stage,
            line,
            message,
            ..
        } => {
            assert_eq!(stage, "字段");
            assert_eq!(line, Some(4), "得指到那一行");
            assert!(message.contains("kye"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn writing_on_a_stale_version_is_refused_with_both_versions() {
    // 乐观并发：**对不上就是 409，不是覆盖。**
    let (_d, mgr, _bus) = setup();
    let stale = mgr.current().unwrap().version();
    // 别人（或者你自己在编辑器里）先改了
    std::fs::write(mgr.path(), format!("{BASE}providers: []\n")).unwrap();

    let e = mgr
        .write("version: 1\nclients: []\n", Some(&stale), Origin::Ui)
        .await
        .unwrap_err();
    assert!(matches!(e, ApplyError::Stale { .. }), "{e:?}");
    assert!(e.to_string().contains("刷新"), "{e}");
    // 而且真的没写
    assert!(
        std::fs::read_to_string(mgr.path())
            .unwrap()
            .contains("providers: []"),
        "手改被覆盖了"
    );
}

#[tokio::test]
async fn a_write_that_would_not_parse_never_reaches_the_disk() {
    // **写完才发现读不回来，那份坏配置已经在盘上了** —— 而用户下一次
    // 启动会撞上它。
    let (_d, mgr, _bus) = setup();
    let e = mgr
        .write(
            "version: 1\nclients:\n  - name: c\n    kye: x\n",
            None,
            Origin::Ui,
        )
        .await
        .unwrap_err();
    assert!(matches!(e, ApplyError::Rejected(_)), "{e:?}");
    assert_eq!(std::fs::read_to_string(mgr.path()).unwrap(), BASE);
}

#[tokio::test]
async fn a_rollback_goes_through_the_same_door_as_everything_else() {
    // 三条路各写一遍，就会有两条忘了存历史、一条忘了防回环。
    let (_d, mgr, _bus) = setup();
    let v0 = mgr.current().unwrap().version();
    mgr.write(&format!("{BASE}providers: []\n"), None, Origin::Ui)
        .await
        .unwrap();
    assert!(
        std::fs::read_to_string(mgr.path())
            .unwrap()
            .contains("providers")
    );

    mgr.rollback(&v0).await.unwrap();
    let back = std::fs::read_to_string(mgr.path()).unwrap();
    assert_eq!(back, BASE);
    assert!(back.contains("别动我这句注释"), "注释没回来");
}

#[tokio::test]
async fn every_write_leaves_the_previous_version_in_history() {
    // 「回滚」这个动作要的是「回到我动它之前」。
    let (_d, mgr, _bus) = setup();
    mgr.write(&format!("{BASE}providers: []\n"), None, Origin::Ui)
        .await
        .unwrap();
    let hist = tw_config::history::list(mgr.path()).unwrap();
    let texts: Vec<String> = hist
        .iter()
        .map(|v| tw_config::history::read(v).unwrap())
        .collect();
    assert!(
        texts.contains(&BASE.to_string()),
        "改之前那一版没进历史：{texts:?}"
    );
}

#[tokio::test]
async fn an_external_edit_also_lands_in_history_so_it_can_be_undone() {
    // 你在编辑器里改坏了配置，也该能一键回到上一版 —— 而「上一版」只有
    // 我们记着才存在。
    let (_d, mgr, _bus) = setup();
    let next = format!("{BASE}providers: []\n");
    std::fs::write(mgr.path(), &next).unwrap();
    mgr.reload_from_disk()
        .await
        .unwrap()
        .expect("该被当成外部改动");
    let hist = tw_config::history::list(mgr.path()).unwrap();
    assert!(
        hist.iter().any(|v| v.origin == Origin::External),
        "外部改动没进历史"
    );
}
