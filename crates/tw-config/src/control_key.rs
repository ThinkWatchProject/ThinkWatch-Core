//! 控制面的钥匙在配置原文里的几件事：补上、换掉、打码、把打码的换回来。
//!
//! **都是最小替换**（tw-yaml）：只动 `listen.control.key` 那一个标量，用户的
//! 注释和排版原样留着。钥匙是 core 替用户写进去的一行，不该顺手把整份
//! 文件按 serde 的样子重排一遍。

use std::path::Path;

use tw_api::control::{ControlKey, KEY_HEX_LEN, KEY_MASK, KEY_PATH};
use tw_yaml::{PatchError, Scalar, Step};

use crate::store::{self, StoreError};

fn at() -> Vec<Step> {
    KEY_PATH.iter().map(|k| Step::key(*k)).collect()
}

/// 原文里写着的那个值，**不管它是不是一把能用的钥匙**。没写是 `None`。
pub fn raw_in(text: &str) -> Result<Option<String>, PatchError> {
    match tw_yaml::find(text, &at()) {
        Ok(f) => Ok(Some(f.value)),
        Err(PatchError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// 没有钥匙就补一把，**只加这一行**。返回改过的原文；已经有了是 `None`。
///
/// 写着一把坏钥匙（短了、不是十六进制）**不替换**：那是用户自己写的，悄悄
/// 换掉等于让他手里那一把失效。校验会说它不对，`twcore control-key --rotate`
/// 换新的。
pub fn ensure(text: &str) -> Result<Option<String>, PatchError> {
    if raw_in(text)?.is_some() {
        return Ok(None);
    }
    let key = crate::generate_control_key();
    tw_yaml::insert(text, &at(), &Scalar::s(key.to_hex())).map(Some)
}

/// 换一把新的钥匙。没有就补上。
pub fn rotate(text: &str) -> Result<(String, ControlKey), PatchError> {
    let key = crate::generate_control_key();
    let out = tw_yaml::insert(text, &at(), &Scalar::s(key.to_hex()))?;
    Ok((out, key))
}

/// 发给界面、写进历史之前，钥匙换成 [`KEY_MASK`]。
///
/// **读不成 YAML 也要打码**：写坏的配置照样会被界面拿去显示（让用户改），
/// 而那时按路径找不到那一行。退一步把原文里每一段正好 64 个十六进制字符
/// 的连续串都换掉 —— 宁可多打一处，不能漏掉钥匙。
pub fn mask(text: &str) -> String {
    match tw_yaml::find(text, &at()) {
        Ok(f) if !f.aliased => match tw_yaml::set(text, &at(), &Scalar::s(KEY_MASK)) {
            Ok(out) => out,
            Err(_) => mask_hex_runs(text),
        },
        Err(PatchError::NotFound(_)) => text.to_string(),
        _ => mask_hex_runs(text),
    }
}

/// 原文里每一段正好 [`KEY_HEX_LEN`] 个十六进制字符的连续串，换成打码。
///
/// 也给报错时摘出来的那一行用：一行读不成 YAML，按路径找不到钥匙。
pub(crate) fn mask_hex_runs(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut last = 0;
    while i < b.len() {
        if !b[i].is_ascii_hexdigit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_hexdigit() {
            i += 1;
        }
        if i - start == KEY_HEX_LEN {
            out.push_str(&text[last..start]);
            out.push_str(KEY_MASK);
            last = i;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// 界面整份写回来的原文里，钥匙那一处是打码的：换回 `real` 那一把。
///
/// **打码就是「钥匙不动」**：界面从来拿不到真钥匙，它写回来的只能是打码。
/// 换回来之后这份原文的钥匙就是现在那一把，写入才过得了「控制面不能改钥匙」
/// 那一关。没有 `real` 时原样返回 —— 校验会说它不是一把钥匙。
pub fn unmask(text: &str, real: Option<&str>) -> Result<String, PatchError> {
    if raw_in(text)?.as_deref() != Some(KEY_MASK) {
        return Ok(text.to_string());
    }
    match real {
        Some(real) => tw_yaml::set(text, &at(), &Scalar::s(real)),
        None => Ok(text.to_string()),
    }
}

/// `serve` 起控制面之前：配置文件里没有钥匙就补一把。返回改没改。
///
/// **先写钥匙、再监听**：桌面端看到 socket 出现就会来读钥匙。文件不存在
/// 不在这里管 —— 那是生成初始配置那一步的事，那份配置自带钥匙。
pub fn ensure_file(path: &Path) -> Result<bool, EnsureError> {
    let cur = store::read(path)?;
    match ensure(&cur.text)? {
        None => Ok(false),
        Some(next) => {
            store::write_if_unchanged(path, &cur.fingerprint, &next)?;
            Ok(true)
        }
    }
}

/// 换钥匙并写回文件。`twcore control-key --rotate` 走这条。
///
/// **写之前校验**：换完钥匙的配置得是一份能用的配置，否则写下去等于把
/// 一份坏配置交给正在跑的 core（它会拒绝、继续用旧钥匙），而命令行却说
/// 换好了。
pub fn rotate_file(path: &Path) -> Result<ControlKey, EnsureError> {
    let cur = store::read(path)?;
    let (next, key) = rotate(&cur.text)?;
    crate::try_parse(&next).map_err(|r| EnsureError::Rejected(Box::new(r)))?;
    let _ = crate::history::snapshot(path, &cur.text, crate::history::Origin::Cli);
    store::write_if_unchanged(path, &cur.fingerprint, &next)?;
    let _ = crate::history::snapshot(path, &next, crate::history::Origin::Cli);
    Ok(key)
}

#[derive(Debug, thiserror::Error)]
pub enum EnsureError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Patch(#[from] PatchError),
    #[error("{0}")]
    Rejected(Box<crate::Rejected>),
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn key_of(text: &str) -> Option<String> {
        raw_in(text).unwrap()
    }

    /// 旧配置没有钥匙：只多出钥匙这几行，注释和其余的一个字节不动。
    #[test]
    fn a_key_is_added_without_touching_anything_else() {
        let before = "version: 1 # 别动我\n# 上游\nproviders: []\nclients:\n  - { name: default, key: tw-abc }\n";
        let after = ensure(before).unwrap().expect("没有钥匙就该补上");
        let k = key_of(&after).unwrap();
        assert!(ControlKey::parse(&k).is_ok(), "{after}");
        // 去掉新加的那几行，剩下的就是原文
        let kept: String = after
            .lines()
            .filter(|l| !(l.starts_with("listen:") || l.contains("control:") || l.contains(&k)))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(kept, before, "{after}");
        crate::try_parse(&after).unwrap_or_else(|e| panic!("{e}\n{after}"));
        // 第二次什么都不做
        assert_eq!(ensure(&after).unwrap(), None);
    }

    /// 已经有 `listen.gateway` 的：钥匙加在同一个 `listen` 下面。
    #[test]
    fn a_key_joins_an_existing_listen_section() {
        let before = "version: 1\nlisten:\n  gateway:\n    port: 9000 # 自己改的\nclients:\n  - { name: default, key: tw-abc }\n";
        let after = ensure(before).unwrap().unwrap();
        assert!(after.contains("    port: 9000 # 自己改的\n"), "{after}");
        let cfg = crate::try_parse(&after).unwrap_or_else(|e| panic!("{e}\n{after}"));
        assert_eq!(cfg.listen.gateway.port, 9000);
        assert!(cfg.listen.control.key().is_some());
    }

    /// 写坏了的钥匙不替换：那是用户的，校验会说它不对。
    #[test]
    fn a_bad_key_is_left_for_the_user_to_fix() {
        let before = "version: 1\nlisten:\n  control:\n    key: short\n";
        assert_eq!(ensure(before).unwrap(), None);
    }

    #[test]
    fn rotation_replaces_only_the_value() {
        let before = format!("version: 1\nlisten:\n  control:\n    key: \"{HEX}\" # 钥匙\n");
        let (after, key) = rotate(&before).unwrap();
        assert_ne!(key.to_hex(), HEX);
        assert_eq!(
            after,
            before.replace(HEX, &key.to_hex()),
            "引号和注释都该留着"
        );
    }

    /// 打码前后一样长：界面按光标位置问「这是哪一段」，偏移要对得上。
    #[test]
    fn masking_keeps_the_length_and_the_quotes() {
        for text in [
            format!("version: 1\nlisten:\n  control:\n    key: {HEX}\nclients: []\n"),
            format!("version: 1\nlisten:\n  control:\n    key: \"{HEX}\"\nclients: []\n"),
            format!("version: 1\nlisten: {{ control: {{ key: '{HEX}' }} }}\n"),
        ] {
            let m = mask(&text);
            assert!(!m.contains(HEX), "{m}");
            assert!(m.contains(KEY_MASK), "{m}");
            assert_eq!(m.len(), text.len(), "{m}");
            assert_eq!(key_of(&m).as_deref(), Some(KEY_MASK));
        }
        // 没有钥匙就原样
        assert_eq!(mask("version: 1\n"), "version: 1\n");
    }

    /// 写坏了的 YAML 找不到那一行，钥匙也不能漏出去。
    #[test]
    fn masking_still_works_when_the_yaml_is_broken() {
        let text = format!("version: 1\nlisten:\n  control:\n    key: {HEX}\n  - oops: [\n");
        let m = mask(&text);
        assert!(!m.contains(HEX), "{m}");
        assert!(m.contains(KEY_MASK), "{m}");
        // 别的十六进制串（比如 65 位的）不动
        let other = "a".repeat(65);
        assert_eq!(mask_hex_runs(&other), other);
    }

    #[test]
    fn a_masked_key_written_back_becomes_the_real_one_again() {
        let current = format!("version: 1\nlisten:\n  control:\n    key: {HEX}\n");
        let edited = mask(&current).replace("version: 1", "version: 1 # 改了一处");
        let back = unmask(&edited, Some(HEX)).unwrap();
        assert_eq!(key_of(&back).as_deref(), Some(HEX));
        assert!(back.contains("# 改了一处"));
        // 不是打码的就不碰：换了钥匙、删了钥匙，都原样交给后面那一关
        let other = current.replace(HEX, &"1".repeat(64));
        assert_eq!(unmask(&other, Some(HEX)).unwrap(), other);
        assert_eq!(unmask("version: 1\n", Some(HEX)).unwrap(), "version: 1\n");
        // 不知道真钥匙是哪把：原样交出去，校验会说打码的那串不是钥匙
        assert_eq!(unmask(&edited, None).unwrap(), edited);
    }

    #[test]
    fn a_file_without_a_key_gets_one_and_a_second_pass_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        std::fs::write(
            &p,
            "version: 1\nclients:\n  - { name: default, key: tw-abc }\n",
        )
        .unwrap();
        assert!(ensure_file(&p).unwrap());
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(crate::try_parse(&text).is_ok(), "{text}");
        assert!(!ensure_file(&p).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), text);

        let before = key_of(&text).unwrap();
        let k = rotate_file(&p).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert_eq!(key_of(&after).unwrap(), k.to_hex());
        assert_ne!(k.to_hex(), before);
    }
}
