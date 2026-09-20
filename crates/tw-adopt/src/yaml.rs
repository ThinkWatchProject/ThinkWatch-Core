//! YAML 的外科手术，靠 [`tw_yaml`] 的 span-to-patch。
//!
//! 覆盖面比 JSON 和 TOML 窄，而且是**故意**的：这里只支持顶层的扁平
//! 键。Aider 的 `.aider.conf.yml` 正好就是那个形状（`openai-api-base`
//! 这类），够用；Continue 那种「往 `models:` 列表里塞一个新条目」是
//! 结构性改动，不在这条路上（见 [`crate::clients`] 里它为什么只给
//! 指引）。
//!
//! **宁可少支持一个客户端，也不要写一个我们自己没把握的结构性改写。**
//! 这个文件会去改用户的配置，出错的代价不是「功能没做」而是「他的东西
//! 被弄坏了」。

use tw_yaml::{PatchError, Scalar, Step};

#[derive(Debug, thiserror::Error)]
pub enum YErr {
    #[error("{0}")]
    Patch(#[from] PatchError),
    #[error("only top-level fields are supported, and {0} is nested")]
    TooDeep(String),
}

fn only_top<'a>(path: &'a [&'a str]) -> Result<&'a str, YErr> {
    match path {
        [k] => Ok(k),
        _ => Err(YErr::TooDeep(path.join("."))),
    }
}

pub fn get(text: &str, path: &[&str]) -> Result<Option<String>, YErr> {
    let k = only_top(path)?;
    match tw_yaml::find(text, &[Step::Key(k.to_string())]) {
        Ok(f) => Ok(Some(f.value)),
        Err(PatchError::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 写一个顶层字段。已经有就原地替换，没有就**追加到文件末尾** ——
/// 不去猜该插在哪一行之间，那样只会打乱用户自己排的顺序。
pub fn set(text: &str, path: &[&str], value: &str) -> Result<String, YErr> {
    let k = only_top(path)?;
    let step = [Step::Key(k.to_string())];
    match tw_yaml::find(text, &step) {
        Ok(_) => Ok(tw_yaml::set(text, &step, &Scalar::s(value))?),
        Err(PatchError::NotFound { .. }) => {
            let mut out = text.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&format!("{k}: '{}'\n", value.replace('\'', "''")));
            Ok(out)
        }
        Err(e) => Err(e.into()),
    }
}

/// 删掉一个顶层字段：把它那一行整行拿掉。
///
/// 只处理「键和值在同一行」的情形 —— 多行值（块标量、嵌套映射）不碰，
/// 原样返回。我们自己写进去的永远是单行，所以还原时够用；碰上不是我们
/// 写的形状，宁可不动。
pub fn remove(text: &str, path: &[&str]) -> Result<String, YErr> {
    let k = only_top(path)?;
    let found = match tw_yaml::find(text, &[Step::Key(k.to_string())]) {
        Ok(f) => f,
        Err(PatchError::NotFound { .. }) => return Ok(text.to_string()),
        Err(e) => return Err(e.into()),
    };
    let line_start = text[..found.bytes.start]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let line_end = text[found.bytes.end..]
        .find('\n')
        .map(|i| found.bytes.end + i + 1)
        .unwrap_or(text.len());
    // 键必须就在这一行的开头（允许前导空白）—— 否则这是个嵌套或者
    // 多行的形状，不是我们写的那种
    if !text[line_start..found.bytes.start]
        .trim_start()
        .starts_with(k)
    {
        return Ok(text.to_string());
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..line_start]);
    out.push_str(&text[line_end..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_existing_field_is_replaced_in_place() {
        let src = "# 我的 aider 配置\nmodel: gpt-4o\nopenai-api-base: https://api.openai.com/v1\ndark-mode: true\n";
        let out = set(src, &["openai-api-base"], "http://127.0.0.1:8080/v1").unwrap();
        assert!(out.contains("# 我的 aider 配置"), "{out}");
        assert!(out.contains("dark-mode: true"), "{out}");
        assert!(out.contains("127.0.0.1:8080"), "{out}");
        assert!(!out.contains("api.openai.com"), "{out}");
    }

    #[test]
    fn a_missing_field_is_appended_rather_than_guessed_into_place() {
        let src = "model: gpt-4o\n";
        let out = set(src, &["openai-api-base"], "http://127.0.0.1:8080/v1").unwrap();
        assert_eq!(
            out,
            "model: gpt-4o\nopenai-api-base: 'http://127.0.0.1:8080/v1'\n"
        );
    }

    #[test]
    fn a_file_without_a_trailing_newline_still_gets_a_well_formed_line() {
        let out = set("model: gpt-4o", &["k"], "v").unwrap();
        assert_eq!(out, "model: gpt-4o\nk: 'v'\n");
    }

    #[test]
    fn removing_takes_the_whole_line_and_leaves_the_neighbours() {
        let src = "model: gpt-4o\nopenai-api-base: 'http://x'\ndark-mode: true\n";
        assert_eq!(
            remove(src, &["openai-api-base"]).unwrap(),
            "model: gpt-4o\ndark-mode: true\n"
        );
    }

    #[test]
    fn a_round_trip_puts_the_file_back_exactly() {
        let src = "# 注释\nmodel: gpt-4o\n";
        let with = set(src, &["openai-api-base"], "http://x").unwrap();
        assert_eq!(remove(&with, &["openai-api-base"]).unwrap(), src);
    }

    #[test]
    fn removing_something_absent_is_not_an_error() {
        let src = "model: gpt-4o\n";
        assert_eq!(remove(src, &["nope"]).unwrap(), src);
    }

    #[test]
    fn nested_paths_are_refused_out_loud_instead_of_half_done() {
        // Continue 的 models[].apiBase 走不了这条路。**说出来**，
        // 而不是写出一个我们没把握的结构。
        assert!(matches!(
            set("a: 1\n", &["a", "b"], "v"),
            Err(YErr::TooDeep(_))
        ));
        assert!(matches!(get("a: 1\n", &["a", "b"]), Err(YErr::TooDeep(_))));
    }

    #[test]
    fn multibyte_content_is_not_sliced_in_half() {
        // tw-yaml 的 marker 是按 char 数的，按它切 &str 会 panic。
        // 这个项目已经被这一类坑过三次。
        let src = "# 中文注释\n模型: 通义千问\nopenai-api-base: 'http://旧地址'\n";
        let out = set(src, &["openai-api-base"], "http://新地址").unwrap();
        assert!(out.contains("模型: 通义千问"), "{out}");
        assert!(out.contains("http://新地址"), "{out}");
        let back = remove(&out, &["openai-api-base"]).unwrap();
        assert_eq!(back, "# 中文注释\n模型: 通义千问\n");
    }

    #[test]
    fn a_value_with_a_quote_in_it_is_escaped() {
        let out = set("a: 1\n", &["k"], "it's").unwrap();
        assert_eq!(get(&out, &["k"]).unwrap(), Some("it's".to_string()));
    }
}
