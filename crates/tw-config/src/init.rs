//! 首次运行时生成一份最小合法配置。
//!
//! **整文件生成，不走 §3.8 的最小替换** —— 那是两套机制：这里是从无到
//! 有，那里是改一个字节而保住其余全部。

use crate::{Client, Config, GatewayListen, Listen, Provider, SCHEMA_VERSION};

/// 生成一把网关密钥。
///
/// `tw-` 前缀是刻意的：用户在客户端配置文件里看到它时，一眼就知道这不是
/// 某个上游的真 API key，不会误以为自己的密钥泄漏了（§5.4）。
pub fn generate_key() -> String {
    // base32 的字母表：去掉了 0/O、1/I/l 这些抄错的字符。用户是要把这
    // 串东西读出来、抄进另一个配置文件的。
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let body: String = (0..24)
        .map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char)
        .collect();
    format!("tw-{body}")
}

/// 一份能通过校验、但还没有上游的骨架配置。
///
/// 注意它**不是**合法配置 —— `providers` 是空的，`validate` 会拒。这是
/// 有意的：首次运行的第二步就是引导用户填第一个 provider（§7.6），在那
/// 之前配置本就不该被当成可用。
pub fn generate_initial() -> Config {
    Config {
        version: SCHEMA_VERSION,
        listen: Listen {
            gateway: GatewayListen::default(),
        },
        clients: vec![Client {
            name: "default".to_string(),
            key: generate_key(),
        }],
        providers: Vec::new(),
    }
}

/// 带一个 provider 的完整初始配置，用于引导流程结束时落盘。
pub fn generate_with_provider(name: &str, base_url: &str, key: &str) -> Config {
    let mut cfg = generate_initial();
    cfg.providers.push(Provider {
        name: name.to_string(),
        base_url: base_url.to_string(),
        key: key.to_string(),
        // 猜得出来就不写进文件 —— 少一行是一行（§0.6）。猜不出来也不写：
        // 让 `twcore check` 在这里说「未知」，比在配置里落一个我们编的
        // 默认值好，后者会让用户以为是他自己选的。
        protocol: None,
    });
    cfg
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
        // 而且它**不**通过校验 —— 引导流程还没走完，配置本就不该可用。
        assert!(crate::validate::validate(&c).is_err());
    }

    #[test]
    fn a_config_with_one_provider_is_immediately_usable() {
        let c = generate_with_provider("relay", "https://api.example.com", "sk-1");
        assert!(crate::validate::validate(&c).is_ok());
    }

    #[test]
    fn the_generated_config_round_trips_through_yaml() {
        // 生成出来的东西必须自己能读回去，否则首次运行就废了。
        let c = generate_with_provider("relay", "https://api.example.com", "sk-1");
        let text = serde_yaml_ng::to_string(&c).unwrap();
        let back: Config = serde_yaml_ng::from_str(&text).unwrap();
        assert_eq!(back.clients[0].key, c.clients[0].key);
        assert_eq!(back.providers[0].base_url, "https://api.example.com");
    }
}
