//! 区域、地址，以及模型 id 怎么写进路径。
//!
//! Bedrock 的主机名是从区域拼出来的：推理走 `bedrock-runtime.{区域}.amazonaws.com`，
//! 列模型走控制面 `bedrock.{区域}.amazonaws.com`。所以**区域就是主机名的一部分**，
//! 校验区域就是在校验请求 —— 连同它带着的凭证 —— 会去哪一台主机：区域之外多一个
//! 字符，都可能把请求带到别处。

use std::fmt;

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

/// 不是 AWS 的区域代码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRegion;

impl fmt::Display for InvalidRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not an AWS region code such as us-east-1")
    }
}

impl std::error::Error for InvalidRegion {}

/// 一个 AWS 区域代码，比如 `us-east-1`、`us-gov-west-1`。
///
/// 只认「至少两个用 `-` 连起来的小写单词，再接 `-` 和数字」。更宽的写法都可能改掉
/// 拼出来的主机名：`us-east-1.evil.example`、`evil.example#` 拼进去就是另一台主机。
pub fn validate_region(region: &str) -> Result<(), InvalidRegion> {
    let valid = region.rsplit_once('-').is_some_and(|(name, number)| {
        !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
            && name.split('-').count() >= 2
            && name
                .split('-')
                .all(|word| !word.is_empty() && word.bytes().all(|b| b.is_ascii_lowercase()))
    });
    if valid { Ok(()) } else { Err(InvalidRegion) }
}

/// 这个区域的推理地址。调用方先校验过区域。
pub fn runtime_base(region: &str) -> String {
    format!("https://bedrock-runtime.{region}.amazonaws.com")
}

/// 这个区域的控制面，Bedrock 在这里列模型。调用方先校验过区域。
pub fn control_base(region: &str) -> String {
    format!("https://bedrock.{region}.amazonaws.com")
}

/// 标准推理地址里的区域。
///
/// 认 `https://bedrock-runtime.{区域}.amazonaws.com` 和它的 FIPS 版
/// `https://bedrock-runtime-fips.{区域}.amazonaws.com`，末尾的 `/` 可有可无，主机名
/// 不分大小写。带路径、查询、端口、用户信息的都不算：那是代理或者 VPC 端点，它的
/// 区域得另外说。
pub fn region_of(base_url: &str) -> Option<&str> {
    let scheme = "https://";
    let rest = base_url
        .get(..scheme.len())
        .filter(|s| s.eq_ignore_ascii_case(scheme))
        .map(|_| &base_url[scheme.len()..])?;
    let host = rest.strip_suffix('/').unwrap_or(rest);
    let lower = host.to_ascii_lowercase();
    let start = ["bedrock-runtime.", "bedrock-runtime-fips."]
        .iter()
        .find(|p| lower.starts_with(**p))?
        .len();
    let suffix = ".amazonaws.com";
    if !lower.ends_with(suffix) || lower.len() < start + suffix.len() {
        return None;
    }
    // 小写化不改变字节长度，所以按小写的那份量出来的边界在原串上一样合法
    let region = &host[start..host.len() - suffix.len()];
    validate_region(region).ok().map(|_| region)
}

/// 写进路径的模型 id 里要转义的字符。
///
/// ARN 形式的模型 id（`arn:aws:bedrock:…:application-inference-profile/abc`）里有
/// `/`，不转义的话 `/model/{id}/converse` 就多出一段路径，AWS 按路径找不到操作。
/// 会改变 URL 结构的另外几个字符（`?`、`#`、`%`、空格和控制字符）一并转义。
///
/// `:` 不转：每个模型 id 都带它（`…-v1:0`），AWS 的文档和示例也都原样写。签名覆盖的
/// 是实际发出去的那个写法，写法本身不影响签名对不对得上。
const MODEL_ID: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// 发往推理地址的完整 URL。
///
/// `base` 是推理地址（[`runtime_base`]，或者配置里写的代理地址），末尾的 `/` 会去掉。
/// `path` 是 `tw_dialect` 写好的 `/model/{id}/{操作}`，模型 id 在这里转义，见
/// [`MODEL_ID`]。认不出这个形状的路径原样接上。
pub fn runtime_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let shaped = path
        .strip_prefix("/model/")
        .and_then(|rest| rest.rsplit_once('/'));
    match shaped {
        Some((id, action)) if !id.is_empty() => {
            format!(
                "{base}/model/{}/{action}",
                utf8_percent_encode(id, MODEL_ID)
            )
        }
        _ => format!("{base}{path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_codes_are_accepted() {
        for region in [
            "us-east-1",
            "eu-central-2",
            "ap-southeast-7",
            "us-gov-west-1",
        ] {
            assert!(validate_region(region).is_ok(), "{region}");
        }
    }

    #[test]
    fn anything_that_could_change_the_host_is_refused() {
        for bad in [
            "",
            "us-east",
            "useast1",
            "US-EAST-1",
            "us--east-1",
            "-us-east-1",
            "us-east-1.evil.example",
            "evil.example#",
            "evil.example/x?-1",
            "https://bedrock-runtime.us-east-1.amazonaws.com",
        ] {
            assert!(validate_region(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn both_hosts_are_built_from_the_region() {
        assert_eq!(
            runtime_base("eu-west-1"),
            "https://bedrock-runtime.eu-west-1.amazonaws.com"
        );
        assert_eq!(
            control_base("eu-west-1"),
            "https://bedrock.eu-west-1.amazonaws.com"
        );
    }

    #[test]
    fn the_region_is_read_back_from_a_standard_runtime_address() {
        for (url, region) in [
            (
                "https://bedrock-runtime.us-east-1.amazonaws.com",
                "us-east-1",
            ),
            (
                "https://bedrock-runtime.us-east-1.amazonaws.com/",
                "us-east-1",
            ),
            (
                "https://bedrock-runtime-fips.us-gov-west-1.amazonaws.com",
                "us-gov-west-1",
            ),
            (
                "HTTPS://Bedrock-Runtime.eu-west-3.AmazonAWS.com",
                "eu-west-3",
            ),
        ] {
            assert_eq!(region_of(url), Some(region), "{url}");
        }
    }

    #[test]
    fn anything_else_has_no_region_of_its_own() {
        for url in [
            // 不是 https
            "http://bedrock-runtime.us-east-1.amazonaws.com",
            // 控制面不是推理地址
            "https://bedrock.us-east-1.amazonaws.com",
            // 路径、端口、用户信息：是代理或者别的什么
            "https://bedrock-runtime.us-east-1.amazonaws.com/v1",
            "https://bedrock-runtime.us-east-1.amazonaws.com:443",
            "https://user@bedrock-runtime.us-east-1.amazonaws.com",
            // 主机名里借壳
            "https://bedrock-runtime.us-east-1.amazonaws.com.evil.example",
            "https://bedrock-runtime.evil.example.amazonaws.com",
            // VPC 端点
            "https://vpce-0a1b.bedrock-runtime.us-east-1.vpce.amazonaws.com",
            "https://bedrock-runtime..amazonaws.com",
            "",
        ] {
            assert_eq!(region_of(url), None, "{url}");
        }
    }

    #[test]
    fn a_plain_model_id_goes_into_the_path_as_it_is() {
        assert_eq!(
            runtime_url(
                "https://bedrock-runtime.us-east-1.amazonaws.com/",
                "/model/us.anthropic.claude-sonnet-4-5-20250929-v1:0/converse-stream"
            ),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/\
             us.anthropic.claude-sonnet-4-5-20250929-v1:0/converse-stream"
        );
    }

    #[test]
    fn the_slash_of_an_arn_is_encoded_so_the_path_keeps_its_shape() {
        assert_eq!(
            runtime_url(
                "https://bedrock-runtime.us-east-2.amazonaws.com",
                "/model/arn:aws:bedrock:us-east-2:123456789012:application-inference-profile/a1b2c3/converse"
            ),
            "https://bedrock-runtime.us-east-2.amazonaws.com/model/\
             arn:aws:bedrock:us-east-2:123456789012:application-inference-profile%2Fa1b2c3/converse"
        );
    }

    #[test]
    fn characters_that_would_change_the_url_are_encoded_too() {
        assert_eq!(
            runtime_url("https://h", "/model/a?b#c%d e/converse"),
            "https://h/model/a%3Fb%23c%25d%20e/converse"
        );
    }

    #[test]
    fn a_path_of_another_shape_is_appended_as_it_is() {
        assert_eq!(
            runtime_url("https://h/", "/foundation-models"),
            "https://h/foundation-models"
        );
    }
}
