// SPDX-License-Identifier: AGPL-3.0-only
//! Bring-your-own-key transport (spec
//! docs/superpowers/specs/2026-10-08-byok-chat-design.md §7–§8): the audit
//! label, the address guard, the guarded HTTP client every BYOK hop posts
//! through, and the size-limited compile for an end user's regex rules.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

/// The provider label every BYOK hop is recorded under, whatever name the
/// end user gave their provider (spec §8.1).
pub const AUDIT_LABEL: &str = "byok";

/// Compiled-size cap for an end user's `output_regex` pattern, applied to
/// both the NFA and the lazy DFA (spec §4.2).
pub const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// `<id>@byok` for a BYOK slug `<id>@<name>` — the form every audit record
/// and log line uses for a BYOK hop.
pub fn audit_slug(slug: &str) -> String {
    let id = crate::provider::bare_model_id(slug);
    format!("{}@{AUDIT_LABEL}", crate::provider::escape_model_id(&id))
}

/// False for every address a BYOK hop must not reach (spec §7.2).
/// IPv4-mapped and NAT64 IPv6 addresses are judged by the IPv4 inside.
pub fn address_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_allowed(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_allowed(v4);
            }
            let s = v6.segments();
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
                let o = v6.octets();
                return v4_allowed(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
            }
            !(v6.is_unspecified()
                || v6.is_loopback()
                || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || (s[0] & 0xff00) == 0xff00) // ff00::/8 multicast
        }
    }
}

fn v4_allowed(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (b & 0xc0) == 64) // 100.64.0.0/10
        || (a == 169 && b == 254)
        || (a == 172 && (b & 0xf0) == 16) // 172.16.0.0/12
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 168)
        || (a == 198 && (b & 0xfe) == 18) // 198.18.0.0/15
        || a >= 224) // multicast, reserved, broadcast
}

/// Resolves through the system resolver, then drops every refused address;
/// an answer with nothing left fails the lookup. reqwest connects only to
/// what this returns, so a rebinding answer cannot redirect the connection.
struct GuardedResolver;

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let addrs: Vec<SocketAddr> = found.filter(|a| address_allowed(a.ip())).collect();
            if addrs.is_empty() {
                return Err::<reqwest::dns::Addrs, Box<dyn std::error::Error + Send + Sync>>(
                    "byok: destination address not allowed".into(),
                );
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The client every BYOK hop posts through: redirects off, proxies off (a
/// proxy would resolve the host itself), and — unless
/// `allow_private_network` — https only behind the address guard.
pub fn build_http(allow_private_network: bool) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(crate::openrouter::CONNECT_TIMEOUT)
        .pool_idle_timeout(crate::openrouter::POOL_IDLE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if !allow_private_network {
        builder = builder
            .https_only(true)
            .dns_resolver(Arc::new(GuardedResolver));
    }
    builder
        .build()
        .expect("reqwest client build never fails with static config")
}

/// Compile an end user's `output_regex` pattern under [`REGEX_SIZE_LIMIT`].
pub fn compile_pattern(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn refuses_every_reserved_range() {
        for s in [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.8",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.255",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fd00::1",
            "fdaa::2",
            "fe80::1",
            "ff02::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b::7f00:1",
        ] {
            assert!(!address_allowed(ip(s)), "{s} must be refused");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for s in [
            "1.1.1.1",
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.0.1.1",
            "198.17.255.255",
            "198.20.0.1",
            "223.255.255.255",
            "2606:4700:4700::1111",
            "::ffff:1.1.1.1",
            "64:ff9b::101:101",
        ] {
            assert!(address_allowed(ip(s)), "{s} must pass");
        }
    }

    #[test]
    fn audit_slug_replaces_the_provider_name() {
        assert_eq!(audit_slug("gpt-x@mine"), "gpt-x@byok");
        assert_eq!(audit_slug("weird\\@vendor/m@mine"), "weird\\@vendor/m@byok");
    }

    #[test]
    fn compile_pattern_refuses_an_oversized_program() {
        assert!(compile_pattern(r"\s*\[note:[^\]]*\]\s*$").is_ok());
        assert!(compile_pattern(r"(?:\w{1000}){1000}").is_err());
    }

    #[tokio::test]
    async fn guarded_client_refuses_a_loopback_host() {
        let err = build_http(false)
            .post("https://localhost:9/v1/chat/completions")
            .send()
            .await
            .expect_err("the guard must refuse localhost");
        let mut chain = String::new();
        let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(&err);
        while let Some(e) = cur {
            chain.push_str(&e.to_string());
            chain.push('\n');
            cur = e.source();
        }
        assert!(
            chain.contains("destination address not allowed"),
            "error chain: {chain}"
        );
    }

    #[tokio::test]
    async fn guarded_client_refuses_plain_http() {
        let err = build_http(false)
            .post("http://example.com/v1/chat/completions")
            .send()
            .await
            .expect_err("https only");
        assert!(err.is_builder(), "{err}");
    }
}
