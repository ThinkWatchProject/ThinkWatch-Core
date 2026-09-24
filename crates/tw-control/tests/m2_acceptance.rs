//! M2 的验收标准，逐条可执行。
//!
//! > UI 改一个 base_url，配置文件里只有那一行变了，注释格式原样保留；
//! > 手改文件 UI 三秒内反映，写个语法错误进去旧配置继续服务且定位到
//! > 出错行；CLI 和 UI 改同一字段效果一致；改坏能一键回滚；
//! > property test 全绿。
//!
//! 「手改文件三秒内反映」和「旧配置继续服务」在 `sync.rs` 里；
//! property test 在 `tw-yaml`。这里是其余三条。

use std::sync::Arc;
use std::time::Duration;

use tw_config::history::Origin;
use tw_control::ConfigManager;

/// 一份长得像真配置的文件：注释、空行、中文、行尾注释、单引号都有。
const REAL: &str = r#"# ThinkWatch 配置
version: 1

listen:
  gateway:
    port: 8788        # 换端口记得同步改客户端
  control:
    key: c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00

clients:
  - name: claude-code
    key: tw-abc123

providers:
  # 这家走代理
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-ant-xxx

  - name: 中转
    base_url: 'https://relay.example.com'   # 尾注释
    key: sk-relay
"#;

fn setup() -> (tempfile::TempDir, Arc<ConfigManager>) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("config.yaml");
    std::fs::write(&p, REAL).unwrap();
    let cfg: tw_config::Config = serde_yaml_ng::from_str(REAL).unwrap();
    let gw = tw_gateway::AppState::new(cfg).unwrap();
    let bus = gw.bus.clone();
    (d, Arc::new(ConfigManager::new(p, gw, bus)))
}

fn patch(path: &str, value: &str) -> Vec<tw_api::PatchOp> {
    vec![tw_api::PatchOp::Replace {
        path: path.into(),
        value: tw_api::PatchValue::Str(value.into()),
    }]
}

#[tokio::test]
async fn one_changing_a_base_url_touches_exactly_one_line() {
    let (_d, mgr) = setup();
    mgr.patch(
        &patch("/providers/中转/base_url", "https://relay2.example.com"),
        None,
        Origin::Ui,
    )
    .await
    .unwrap();

    let after = std::fs::read_to_string(mgr.path()).unwrap();
    let changed: Vec<_> = REAL
        .lines()
        .zip(after.lines())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .collect();
    assert_eq!(changed.len(), 1, "变了不止一行：{changed:?}");
    assert_eq!(REAL.lines().count(), after.lines().count(), "行数变了");

    // 注释、空行、引号风格全都在
    for keep in [
        "# ThinkWatch 配置",
        "# 换端口记得同步改客户端",
        "# 这家走代理",
        "# 尾注释",
    ] {
        assert!(after.contains(keep), "丢了 {keep}");
    }
    assert!(
        after.contains("'https://relay2.example.com'"),
        "单引号风格没保住：{after}"
    );
    assert_eq!(
        REAL.matches("\n\n").count(),
        after.matches("\n\n").count(),
        "空行被动过"
    );
}

#[tokio::test]
async fn two_the_cli_and_the_ui_produce_the_same_bytes() {
    // **两套实现就是两套行为。**这条测的是它们真的走同一段代码 ——
    // 一边用 API 改，一边用 CLI 那条路改，字节必须一样。
    let (_d1, mgr) = setup();
    mgr.patch(
        &patch("/providers/官方/base_url", "https://x.example.com"),
        None,
        Origin::Ui,
    )
    .await
    .unwrap();
    let via_api = std::fs::read_to_string(mgr.path()).unwrap();

    // CLI 那条路：resolve_path + tw_yaml::set，不经过控制面
    let steps = tw_control::resolve_path(REAL, "/providers/官方/base_url").unwrap();
    let via_cli = tw_yaml::set(REAL, &steps, &tw_yaml::Scalar::s("https://x.example.com")).unwrap();

    assert_eq!(via_api, via_cli, "CLI 和界面改出来的文件不一样");
}

#[tokio::test]
async fn two_b_a_path_by_index_and_a_path_by_name_mean_the_same_thing() {
    // 名字是给人写的，下标是给机器写的。两者必须指向同一个位置，否则
    // 「界面上改的」和「脚本里改的」会是两个东西。
    let by_name = tw_control::resolve_path(REAL, "/providers/中转/key").unwrap();
    let by_index = tw_control::resolve_path(REAL, "/providers/1/key").unwrap();
    assert_eq!(by_name, by_index);
}

#[tokio::test]
async fn three_a_bad_edit_can_be_undone_in_one_step() {
    let (_d, mgr) = setup();
    let good = mgr.current().unwrap().version();

    mgr.patch(
        &patch("/providers/官方/base_url", "https://坏的"),
        None,
        Origin::Ui,
    )
    .await
    .unwrap();
    assert!(
        std::fs::read_to_string(mgr.path())
            .unwrap()
            .contains("坏的")
    );

    mgr.rollback(&good).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(mgr.path()).unwrap(),
        REAL,
        "回滚没有精确还原"
    );
}

#[tokio::test]
async fn three_b_a_rollback_can_itself_be_rolled_back() {
    // 「回滚之后发现回错了」是回滚最常见的用法之一。
    let (_d, mgr) = setup();
    let v0 = mgr.current().unwrap().version();
    mgr.patch(&patch("/providers/官方/key", "sk-new"), None, Origin::Ui)
        .await
        .unwrap();
    let v1 = mgr.current().unwrap().version();

    mgr.rollback(&v0).await.unwrap();
    assert!(
        !std::fs::read_to_string(mgr.path())
            .unwrap()
            .contains("sk-new")
    );

    mgr.rollback(&v1).await.unwrap();
    assert!(
        std::fs::read_to_string(mgr.path())
            .unwrap()
            .contains("sk-new")
    );
}

#[tokio::test]
async fn all_the_ops_in_one_patch_land_together_or_not_at_all() {
    // 一次 patch 改三个字段却分三次写盘，中间任何一次失败都会留下一份
    // 半改的配置 —— **而那份配置是合法的，所以没有任何人会发现**。
    let (_d, mgr) = setup();
    let mut ops = patch("/providers/官方/base_url", "https://a");
    ops.extend(patch("/providers/不存在的/key", "x"));
    let e = mgr.patch(&ops, None, Origin::Ui).await.unwrap_err();
    assert!(e.to_string().contains("不存在的"), "{e}");
    assert_eq!(
        std::fs::read_to_string(mgr.path()).unwrap(),
        REAL,
        "第一个 op 已经写进去了 —— 留下了一份半改的配置"
    );
}

#[tokio::test]
async fn a_patch_that_changes_several_fields_at_once_changes_exactly_those_lines() {
    let (_d, mgr) = setup();
    let mut ops = patch("/providers/官方/base_url", "https://a.example.com");
    ops.extend(patch("/providers/中转/key", "sk-new"));
    ops.push(tw_api::PatchOp::Replace {
        path: "/listen/gateway/port".into(),
        value: tw_api::PatchValue::Int(9999),
    });
    mgr.patch(&ops, None, Origin::Ui).await.unwrap();

    let after = std::fs::read_to_string(mgr.path()).unwrap();
    let changed = REAL
        .lines()
        .zip(after.lines())
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(changed, 3, "改了三个字段，动了 {changed} 行");
    assert!(after.contains("# 换端口记得同步改客户端"), "行尾注释没了");
}

#[tokio::test]
async fn the_reload_after_a_patch_is_the_new_config_not_the_old_one() {
    // 写对了文件但没换进去，用户会以为「保存了没生效」—— 而那是这一层
    // 最容易漏的一步，因为文件看起来完全正确。
    let (_d, mgr) = setup();
    mgr.patch(
        &patch("/providers/官方/base_url", "https://changed.example.com"),
        None,
        Origin::Ui,
    )
    .await
    .unwrap();
    // 等一下让文件监听那条路也走完（它应该什么都不做）
    tokio::time::sleep(Duration::from_millis(400)).await;
    let live = mgr.current().unwrap();
    assert!(live.text.contains("changed.example.com"));
}
