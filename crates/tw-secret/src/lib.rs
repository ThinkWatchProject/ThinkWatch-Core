//! 密钥的三件事：从环境变量取、打码给人看、以及不要把它们写进日志。
//!
//! 设计立场见 DESIGN.md §3.2：**密钥就明文写在 config.yaml 里**，不做
//! keychain、不做主密钥、不做信封加密。`${ENV}` 是给「不想让密钥落到
//! 文件里」的人准备的逃生口，不是默认路径。

use std::collections::HashMap;

pub mod exec;
pub mod mask;

pub use exec::{ExecError, run_exec};
pub use mask::{mask_secret, redact_url};

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("环境变量 {0} 未设置")]
    MissingEnv(String),
    #[error("第 {pos} 个字符处的 ${{...}} 没有闭合")]
    Unterminated { pos: usize },
    #[error("空的变量名：${{}}")]
    EmptyName,
}

/// 把 `${VAR}` 展开成环境变量的值。
///
/// 三条刻意的选择：
///
/// 1. **只认 `${VAR}`，不认裸 `$VAR`。** 裸形式会把 base_url 里的 `$`
///    当成变量起点，而中转站的 URL 里出现 `$` 并不稀奇。
/// 2. **`$${...}` 是转义**，展开成字面量 `${...}`。没有这条，一个真的
///    想写 `${FOO}` 的人就没有出路。
/// 3. **变量不存在是错误，不是空串。** 展开成空串会让请求带着空 key
///    发出去，然后收到一个 401 —— 排查时要花很久才想到是这里。
pub fn expand(input: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, SecretError> {
    if !input.contains('$') {
        return Ok(input.to_string());
    }
    let mut out = String::with_capacity(input.len());
    let b = input.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' {
            let start = i;
            while i < b.len() && b[i] != b'$' {
                i += 1;
            }
            out.push_str(&input[start..i]);
            continue;
        }
        // `$${` → 字面量 `${`
        if b.get(i + 1) == Some(&b'$') && b.get(i + 2) == Some(&b'{') {
            out.push_str("${");
            i += 3;
            continue;
        }
        if b.get(i + 1) != Some(&b'{') {
            out.push('$');
            i += 1;
            continue;
        }
        let name_start = i + 2;
        let Some(rel) = input[name_start..].find('}') else {
            return Err(SecretError::Unterminated { pos: i });
        };
        let name = &input[name_start..name_start + rel];
        if name.is_empty() {
            return Err(SecretError::EmptyName);
        }
        match env(name) {
            Some(v) => out.push_str(&v),
            None => return Err(SecretError::MissingEnv(name.to_string())),
        }
        i = name_start + rel + 1;
    }
    Ok(out)
}

/// 用进程自己的环境展开。
pub fn expand_from_env(input: &str) -> Result<String, SecretError> {
    expand(input, &|k| std::env::var(k).ok())
}

/// 用一张给定的表展开。测试用，也给「不想读进程环境」的调用方用。
pub fn expand_with(input: &str, table: &HashMap<String, String>) -> Result<String, SecretError> {
    expand(input, &|k| table.get(k).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn expands_a_variable() {
        let e = t(&[("K", "sk-123")]);
        assert_eq!(expand_with("${K}", &e).unwrap(), "sk-123");
        assert_eq!(expand_with("Bearer ${K}!", &e).unwrap(), "Bearer sk-123!");
    }

    #[test]
    fn leaves_bare_dollar_alone() {
        // 中转站的 URL 里出现 $ 并不稀奇，不该被当成变量。
        let e = t(&[("K", "v")]);
        assert_eq!(expand_with("a$b", &e).unwrap(), "a$b");
        assert_eq!(expand_with("$K", &e).unwrap(), "$K");
        assert_eq!(expand_with("trailing$", &e).unwrap(), "trailing$");
    }

    #[test]
    fn double_dollar_escapes() {
        assert_eq!(expand_with("$${K}", &t(&[])).unwrap(), "${K}");
    }

    #[test]
    fn missing_variable_is_an_error_not_an_empty_string() {
        // 展成空串会让请求带着空 key 发出去，然后收到 401 —— 那条排查
        // 路径很长，而这里报错是立刻的。
        let err = expand_with("${NOPE}", &t(&[])).unwrap_err();
        assert!(matches!(err, SecretError::MissingEnv(ref n) if n == "NOPE"));
    }

    #[test]
    fn unterminated_and_empty_are_errors() {
        assert!(matches!(
            expand_with("${K", &t(&[])).unwrap_err(),
            SecretError::Unterminated { .. }
        ));
        assert!(matches!(
            expand_with("${}", &t(&[])).unwrap_err(),
            SecretError::EmptyName
        ));
    }

    #[test]
    fn no_dollar_is_a_fast_path_and_identity() {
        assert_eq!(
            expand_with("https://api.example.com", &t(&[])).unwrap(),
            "https://api.example.com"
        );
    }

    #[test]
    fn multibyte_input_does_not_panic() {
        // 按字节切串是这个项目栽过两次的地方（DESIGN.md §9.7）。
        let e = t(&[("K", "值")]);
        assert_eq!(expand_with("前${K}后", &e).unwrap(), "前值后");
    }
}
