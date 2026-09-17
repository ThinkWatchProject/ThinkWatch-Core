//! 把 token 端点换发的新 refresh token 写回 config.yaml。
//!
//! # 为什么这件事在控制面
//!
//! 数据面不写文件（那条：维护和观测绝不跑在转发路径上）。它只把
//! 新值往通道里一放就继续转发。真正动文件的是这里 —— 因为**只有这里
//! 有 `ConfigManager`**，而写用户的配置文件需要它那一整套：写之前存
//! 历史、乐观并发对版本、原子写加 0600、以及记指纹防回环。
//!
//! # 写失败不是小事
//!
//! **服务器换发新 refresh token 的那一刻，旧的已经在服务端作废了。**
//! 写不进去的话，用户的 config.yaml 从这一秒起就是坏的 —— 只是症状要
//! 等到下一次重启才出现。所以失败要重试，重试完还不行就一直挂着说。
//!
//! 我们**不把新 token 显示出来让用户手抄**：那等于把一份凭据印在界面上
//! （还会进截图、进 issue）。真的写不进去时，正确的出路是修好文件权限
//! —— 修好之后下一次轮换会自动写进去 —— 或者重新走一次授权。

use std::time::Duration;

use tw_config::history::Origin;
use tw_gateway::oauth::Rotated;

/// 通道容量。
///
/// **1 就够，但不能是 0**：同一个 provider 的轮换是串行的（换 token
/// 本身是串行的），而不同 provider 同时轮换是可能的。留 8 个位置，
/// 满了那一条会被明确报出来 —— 和 body 那条路「满了就静默丢」不同，
/// 这里丢掉的是一份还没落盘的凭据。
pub const CHANNEL_CAP: usize = 8;

/// 重试几次。
///
/// 写失败最常见的原因是**用户正在编辑器里改这个文件**（乐观并发对不上
/// 版本）。那种冲突是秒级的，等一下再试就好；等三次还不行的话，多半是
/// 权限或者磁盘的问题，那不是等能解决的。
const TRIES: usize = 3;
const GAP: Duration = Duration::from_millis(400);

/// 起一个任务收轮换事件并写回文件。**控制面起来之后调一次。**
pub fn spawn(state: crate::ControlState) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Rotated>(CHANNEL_CAP);
    state.gateway.set_rotation_sink(tx);
    tokio::spawn(async move {
        while let Some(r) = rx.recv().await {
            let (ok, detail) = persist(&state, &r).await;
            state.gateway.report_rotation(&r.provider, ok, &detail);
        }
    });
}

/// 真正写那一次。返回（成没成，人话）。
async fn persist(state: &crate::ControlState, r: &Rotated) -> (bool, String) {
    let mut last = String::new();
    for attempt in 1..=TRIES {
        match once(state, r).await {
            Ok(version) => {
                return (
                    true,
                    format!("已写回 {}（版本 {version}）", state.cfg.path().display()),
                );
            }
            Err(why) => {
                last = why;
                if attempt < TRIES {
                    tokio::time::sleep(GAP).await;
                }
            }
        }
    }
    (
        false,
        format!(
            "{last}。请在重启前处理：检查该文件的权限，以及是否被其他程序占用。问题解决后，下一次轮换会自动写回；如仍无法写回，请重新授权以获取新凭据"
        ),
    )
}

async fn once(state: &crate::ControlState, r: &Rotated) -> Result<String, String> {
    let cur = state
        .cfg
        .current()
        .map_err(|e| format!("无法读取 config.yaml：{e}"))?;
    // 打完补丁的文本自己会先被解析一遍（`patch_oauth_refresh` 里），
    // 所以到这儿的一定是一份能加载的配置
    let patched = tw_config::patch_oauth_refresh(&cur.text, &r.provider, &r.refresh)
        .map_err(|e| e.to_string())?;
    if patched == cur.text {
        // 已经是这个值了 —— 比如重启之后又收到一次同样的轮换。
        // **不写**：一次没有内容变化的写会白白多一条历史和一次重载
        return Ok(cur.version());
    }
    state
        .cfg
        .write(&patched, Some(&cur.version()), Origin::Rotation)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {

    const CFG: &str = r#"version: 1
# 用户自己写的注释，一个字都不该动
listen:
  gateway:
    bind: loopback
    port: 8788
clients:
  - name: claude-code
    key: tw-testkey
providers:
  - name: 官方
    base_url: https://api.anthropic.com
    key: sk-ant-literal
  - name: oauth-家
    base_url: https://api.example.com
    oauth:
      refresh: rt-OLD-ONE     # 这个会被换掉
      endpoint: https://auth.example.com/token
      refresh_before: 5m
"#;

    #[test]
    fn the_patch_touches_exactly_one_scalar() {
        // **用户的注释、排版、字段顺序一个字都不能动** —— 这是一次
        // 用户没要求的写入，它必须只动它该动的那一处
        let out = tw_config::patch_oauth_refresh(CFG, "oauth-家", "rt-NEW-ONE").unwrap();
        assert!(out.contains("rt-NEW-ONE"), "{out}");
        assert!(!out.contains("rt-OLD-ONE"), "{out}");
        assert!(out.contains("# 用户自己写的注释，一个字都不该动"), "{out}");
        assert!(out.contains("key: sk-ant-literal"), "另一家被动了：{out}");
        // 逐行比：只有一行不同
        let diff: Vec<_> = CFG
            .lines()
            .zip(out.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(diff.len(), 1, "动了不止一行：{diff:?}");
        assert_eq!(CFG.lines().count(), out.lines().count(), "行数变了");
    }

    #[test]
    fn a_provider_that_is_no_longer_in_the_config_is_reported_not_guessed() {
        // 轮换在飞的时候用户把这家删了。**不猜位置** —— 在一个装着
        // 明文密钥的文件里猜结构是不能接受的
        let e = tw_config::patch_oauth_refresh(CFG, "已经删掉了", "rt-NEW").unwrap_err();
        assert!(e.to_string().contains("已不存在上游"), "{e}");
    }

    #[test]
    fn a_provider_without_oauth_is_refused_rather_than_rewritten() {
        let e = tw_config::patch_oauth_refresh(CFG, "官方", "rt-NEW").unwrap_err();
        assert!(e.to_string().contains("无法定位"), "{e}");
    }

    #[test]
    fn a_token_with_yaml_metacharacters_survives_the_round_trip() {
        // 真实的 refresh token 里有 `/`、`+`、`=`，也可能以 `*` 或 `&`
        // 开头 —— 后两个在 YAML 里是别名和锚点。**引号是 tw_yaml 的事，
        // 但这条测试要在这儿**：写坏了的后果是配置整个读不回来
        for t in [
            "1//0gLd+xyz/abc=",
            "*starts-with-star",
            "&anchor-looking",
            "yes",
            "含中文的token",
            "with: colon and #hash",
        ] {
            let out = tw_config::patch_oauth_refresh(CFG, "oauth-家", t)
                .unwrap_or_else(|e| panic!("{t} 写不进去：{e}"));
            let re: tw_config::Config = serde_yaml_ng::from_str(&out).unwrap();
            let got = re
                .providers
                .iter()
                .find(|p| p.name == "oauth-家")
                .and_then(|p| p.oauth.as_ref())
                .unwrap();
            assert_eq!(got.refresh, t, "读回来不是原值：{out}");
        }
    }
}
