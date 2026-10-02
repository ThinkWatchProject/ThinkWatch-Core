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

/// 全部默认插件：(id, 源码)。**第一次装上时按这个顺序排进配置**
pub const ALL: &[(&str, &str)] = &[
    ("reply-language", include_str!("reply-language.js")),
    ("current-date", include_str!("current-date.js")),
    ("term-unify", include_str!("term-unify.js")),
    ("reply-redact", include_str!("reply-redact.js")),
    ("wsl-paths", include_str!("wsl-paths.js")),
    ("deepseek-flags", include_str!("deepseek-flags.js")),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// 清单就是约定的那几个，一个不多、一个不少
    #[test]
    fn the_list_is_the_agreed_set() {
        let ids: Vec<&str> = ALL.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            [
                "reply-language",
                "current-date",
                "term-unify",
                "reply-redact",
                "wsl-paths",
                "deepseek-flags",
            ]
        );
    }

    /// 每个 id 都装得进配置：写法对、不是控制面占用的词、不重复
    #[test]
    fn every_id_can_be_a_plugin_id() {
        let mut seen = std::collections::HashSet::new();
        for (id, _) in ALL {
            assert!(tw_config::plugins::valid_id(id), "{id}");
            assert!(!tw_config::plugins::RESERVED_IDS.contains(id), "{id}");
            assert!(seen.insert(*id), "{id} is listed twice");
        }
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
