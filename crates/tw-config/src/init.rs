//! 首次运行时生成一份最小合法配置。
//!
//! **整文件生成，不走最小替换** —— 那是两套机制：这里是从无到
//! 有，那里是改一个字节而保住其余全部。

use crate::{Client, Config, ControlListen, Listen};

/// 生成一把网关密钥。
///
/// `tw-` 前缀是刻意的：用户在客户端配置文件里看到它时，一眼就知道这不是
/// 某个上游的真 API key，不会误以为自己的密钥泄漏了。
pub fn generate_key() -> String {
    // base32 的字母表：去掉了 0/O、1/I/l 这些抄错的字符。用户是要把这
    // 串东西读出来、抄进另一个配置文件的。
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let body: String = (0..24)
        .map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char)
        .collect();
    format!("tw-{body}")
}

/// 生成一把控制面的钥匙：32 个随机字节。
///
/// **`serve` 首次运行、`serve` 给旧配置补钥匙、`init`、`control-key --rotate`
/// 都用它**，不各生成各的。
pub fn generate_control_key() -> tw_api::control::ControlKey {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    tw_api::control::ControlKey::from_bytes(bytes)
}

/// 一份还没有上游的骨架配置。
///
/// 它**是合法的**（见 `validate` 里那段注释）：core 要能带着它起来，
/// 控制面要能工作，引导流程才有地方跑。数据面会在收到请求时说清楚
/// 「还没配上游」—— 那是「不能转发」，不是「配置错了」。
pub fn generate_initial() -> Config {
    Config {
        clients: vec![Client {
            name: "default".to_string(),
            key: generate_key(),
            ..Default::default()
        }],
        listen: Listen {
            control: ControlListen {
                key: Some(generate_control_key().to_hex()),
            },
            ..Default::default()
        },
        // 其余全是默认值：没有 provider、没有代理、没有规则。
        // 层 0（不写规则也能跑）就是这份配置的形状。
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_carry_the_tw_prefix() {
        assert!(generate_key().starts_with("tw-"));
    }

    #[test]
    fn keys_avoid_characters_people_transcribe_wrong() {
        // 用户要把这串东西抄进另一个配置文件，0/O 和 1/l/I 会害人。
        let k = generate_key();
        for bad in ['0', 'o', 'O', '1', 'l', 'I'] {
            assert!(!k[3..].contains(bad), "密钥里出现了易混字符 {bad}：{k}");
        }
    }

    #[test]
    fn keys_are_not_repeated() {
        let a = generate_key();
        let b = generate_key();
        assert_ne!(a, b);
    }

    #[test]
    fn the_skeleton_has_a_key_but_no_upstream() {
        let c = generate_initial();
        assert_eq!(c.clients.len(), 1);
        assert!(c.providers.is_empty());
        // 而且它是合法的 —— core 要能带着它起来，否则 UI 连「你还没配
        // 上游」都说不出口。
        assert!(crate::validate::validate(&c).is_ok());
    }

    /// 骨架配置里有钥匙，而网关那一段仍然是默认值、不写进文件。
    #[test]
    fn the_skeleton_carries_a_control_key_and_nothing_else_under_listen() {
        let c = generate_initial();
        assert!(c.listen.control.key().is_some());
        let text = serde_yaml_ng::to_string(&c).unwrap();
        assert!(text.contains("control:"), "{text}");
        assert!(
            !text.contains("gateway"),
            "默认的网关监听被写进文件了：{text}"
        );
        assert_ne!(
            generate_initial().listen.control.key,
            c.listen.control.key,
            "每份配置的钥匙都该是新生成的"
        );
    }

    #[test]
    fn the_generated_config_round_trips_through_yaml() {
        // 生成出来的东西必须自己能读回去，否则首次运行就废了。
        let c = generate_initial();
        let text = serde_yaml_ng::to_string(&c).unwrap();
        let back: Config = serde_yaml_ng::from_str(&text).unwrap();
        assert_eq!(back.clients[0].key, c.clients[0].key);
        assert!(crate::validate::validate(&back).is_ok());
    }
}
