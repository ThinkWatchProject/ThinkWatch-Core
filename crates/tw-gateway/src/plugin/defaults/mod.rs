//! 随 core 一起发的插件（默认插件）。
//!
//! 它们和用户自己装的插件**在同一张单子上**：没有单独的分组、没有特别的标记，**装上时
//! 一律停用**，用户像对别的插件一样自己打开。源码就是这个目录里的 `<id>.js`，编进二进制
//! —— 远程 core 也带着它们。
//!
//! 这里只有清单。什么时候装上、什么时候换成新版、用户删了或改了之后怎么办，是管理面的
//! 事（`tw_control::plugins::defaults`），记在插件目录的 `.defaults.json` 里。
//!
//! **加一个就在 [`ALL`] 里加一行。id 一经发出就不再改**：用户删掉的默认插件按 id 记着，
//! 改了 id 等于又塞给他一个删过的插件。
//!
//! **manifest 里给人看的字（名字、说明、设置项的标签）一律写英文**：桌面端按插件 id 和
//! 设置项的键换成界面的语言，表里没有的照这里的英文显示。
//!
//! **manifest 写成 core 改写它时的样子**（`tw_plugin::literal::write`）：出错时怎么办、范围、
//! 设置的值都在文件里，用户在界面上改一个设置，文件里只有那一行变。
//!
//! **装上它们不起运行时**：它们装上时都停用着，而沙箱一起来就是几 MB 常驻内存。显示要的
//! 名字、权限、范围和设置，都从 `manifests.json` 里读 —— 那是测试照真的
//! 沙箱把每一个编一遍生成的（[`manifest`]）。**改了哪个 `.js` 就重新生成一次**：
//! `UPDATE_DEFAULT_MANIFESTS=1 cargo test -p tw-gateway --lib plugin::defaults`，不然测试
//! 不过；生成的那一份对不上源码时，管理面退回到真的编一遍。

/// 全部默认插件：(id, 源码)。**第一次装上时按这个顺序排进配置**
pub const ALL: &[(&str, &str)] = &[
    ("reply-language", include_str!("reply-language.js")),
    ("wsl-paths", include_str!("wsl-paths.js")),
];

/// 每个默认插件预先算好的 manifest：id → `{ sha256, manifest }`，`sha256` 是生成时那份
/// 源码的
const MANIFESTS: &str = include_str!("manifests.json");

/// 这个默认插件发出去的那一版的 manifest（预先算好的，见模块说明）。**和现在这份源码
/// 对不上**（改了 `.js` 没重新生成）、不是默认插件，都是 None
pub fn manifest(id: &str) -> Option<crate::plugin::Manifest> {
    static PARSED: std::sync::OnceLock<serde_json::Map<String, serde_json::Value>> =
        std::sync::OnceLock::new();
    let all = PARSED.get_or_init(|| serde_json::from_str(MANIFESTS).unwrap_or_default());
    let (_, source) = ALL.iter().find(|(x, _)| *x == id)?;
    let entry = all.get(id)?;
    if entry["sha256"].as_str()? != crate::plugin::load::sha256_hex(source.as_bytes()) {
        return None;
    }
    crate::plugin::manifests::from_json(&entry["manifest"])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 清单就是约定的那几个，一个不多、一个不少
    #[test]
    fn the_list_is_the_agreed_set() {
        let ids: Vec<&str> = ALL.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, ["reply-language", "wsl-paths"]);
    }

    /// 每个 id 都装得进配置：写法对、不重复
    #[test]
    fn every_id_can_be_a_plugin_id() {
        let mut seen = std::collections::HashSet::new();
        for (id, _) in ALL {
            assert!(tw_config::plugins::valid_id(id), "{id}");
            assert!(seen.insert(*id), "{id} is listed twice");
        }
    }

    /// 预先算好的 manifest 就是真的沙箱编出来的那一份。改了 `.js`：
    /// `UPDATE_DEFAULT_MANIFESTS=1 cargo test -p tw-gateway --lib plugin::defaults`
    #[test]
    fn the_precomputed_manifests_are_what_the_sandbox_reads() {
        let engine = crate::plugin::default_engine();
        let mut want = serde_json::Map::new();
        let mut compiled = Vec::new();
        for (id, source) in ALL {
            let host = engine
                .load(source.as_bytes())
                .unwrap_or_else(|e| panic!("{id} does not load: {}", e.msg()));
            want.insert(
                id.to_string(),
                serde_json::json!({
                    "sha256": crate::plugin::load::sha256_hex(source.as_bytes()),
                    "manifest": crate::plugin::manifests::to_json(host.manifest()),
                }),
            );
            compiled.push((*id, host.manifest().clone()));
        }
        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::Value::Object(want)).unwrap()
        );
        if std::env::var_os("UPDATE_DEFAULT_MANIFESTS").is_some() {
            std::fs::write(
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/src/plugin/defaults/manifests.json"
                ),
                &text,
            )
            .unwrap();
            return;
        }
        assert!(
            MANIFESTS == text,
            "src/plugin/defaults/manifests.json is out of date: run \
             UPDATE_DEFAULT_MANIFESTS=1 cargo test -p tw-gateway --lib plugin::defaults"
        );
        for (id, m) in compiled {
            assert_eq!(manifest(id), Some(m), "{id}");
        }
        assert_eq!(manifest("not-a-default"), None);
    }

    /// 给人看的那几样是英文（见模块说明）：名字、说明、每个设置项的标签都写了，一个汉字
    /// 都没有。查的是装上和显示时用的那一份（预先算好的 manifest）
    #[test]
    fn what_a_default_shows_is_written_in_english() {
        let cjk = |s: &str| {
            s.chars().any(|c| {
                matches!(c as u32,
                    0x3000..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff
                        | 0xff00..=0xffef)
            })
        };
        for (id, _) in ALL {
            let m = manifest(id).unwrap_or_else(|| panic!("{id} has no precomputed manifest"));
            let mut shown = vec![("name", m.name.clone())];
            shown.extend(m.description.clone().map(|d| ("description", d)));
            shown.extend(m.settings.iter().map(|s| ("label", s.label.clone())));
            for (what, text) in shown {
                assert!(!text.trim().is_empty(), "{id}: the {what} is empty");
                assert!(!cjk(&text), "{id}: the {what} is not in English: {text:?}");
            }
        }
    }

    /// manifest 写成 core 改写它时的样子：改一个设置，文件里只有那一行变
    #[test]
    fn every_manifest_is_written_the_way_core_writes_it() {
        for (id, source) in ALL {
            let lit = tw_plugin::literal::find(source).unwrap_or_else(|e| panic!("{id}: {e}"));
            let canonical = tw_plugin::literal::replace(source, &lit, &lit.data);
            assert!(
                canonical == *source,
                "{id}: the manifest is not written the way core writes it:\n{canonical}"
            );
        }
        let (_, src) = ALL.iter().find(|(id, _)| *id == "reply-language").unwrap();
        let out = crate::plugin::source::rewrite(
            src,
            tw_api::OnError::Reject,
            &tw_api::PluginScope::default(),
            &std::collections::BTreeMap::from([(
                "language".to_string(),
                tw_api::SettingValue::String("English".into()),
            )]),
        )
        .unwrap();
        assert_eq!(src.lines().count(), out.lines().count(), "{out}");
        let changed: Vec<_> = src
            .lines()
            .zip(out.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert!(changed[0].1.contains("value: \"English\""), "{changed:?}");
    }

    /// 每一个都在真的沙箱里编得成：装不上的默认插件只会在日志里留一行
    #[test]
    fn every_default_compiles_in_the_real_sandbox() {
        let engine = crate::plugin::default_engine();
        for (id, source) in ALL {
            assert!(source.len() <= crate::plugin::MAX_SOURCE, "{id}");
            if let Err(e) = engine.load(source.as_bytes()) {
                panic!("{id} does not load: {}", e.msg());
            }
        }
    }
}
