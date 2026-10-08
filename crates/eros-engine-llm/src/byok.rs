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
                || (s[0] & 0xffc0) == 0xfec0 // fec0::/10 site-local
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

/// True when a strip rule `pattern` → `replacement` can never lengthen the
/// text it rewrites (spec §4.2). Rules run in sequence on each other's
/// output, so a growing rule would compound. `replacement` must hold no `$`
/// (nothing expands per match) and be no longer in bytes than the shortest
/// text `pattern` can match: a pattern that can match the empty string
/// admits only the empty replacement, and one that can match nothing admits
/// any. A pattern that does not parse admits none.
pub fn replacement_is_non_growing(pattern: &str, replacement: &str) -> bool {
    if replacement.contains('$') {
        return false;
    }
    regex_syntax::parse(pattern).is_ok_and(|hir| {
        hir.properties()
            .minimum_len()
            .is_none_or(|shortest| replacement.len() <= shortest)
    })
}

/// Bytes of a non-2xx response body a BYOK hop reads (spec §7.3).
pub(crate) const ERROR_BODY_MAX: usize = 64 * 1024;

/// Floor of a BYOK hop's streamed-body bound (spec §7.3).
const STREAM_BYTES_FLOOR: u64 = 1 << 20;

/// A BYOK hop's streamed body may carry `max(1 MiB, max_tokens × 1 KiB)`
/// bytes (spec §7.3). The bound also caps everything derived from the body:
/// the accumulated reply, the regex scrubber's held text, the persisted row.
pub(crate) fn stream_byte_limit(max_tokens: u32) -> u64 {
    (u64::from(max_tokens) * 1024).max(STREAM_BYTES_FLOOR)
}

/// At most `cap` bytes of `resp`'s body, decoded as lossy UTF-8.
pub(crate) async fn read_capped(mut resp: reqwest::Response, cap: usize) -> String {
    let mut buf = Vec::new();
    while buf.len() < cap {
        match resp.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk[..chunk.len().min(cap - buf.len())]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Passes `s` through until more than `limit` bytes have arrived, then
/// yields one error and ends. The error names the bound only, never the URL
/// or the key; the SSE layer turns it into a `transport` gateway error.
pub(crate) fn byte_capped<S, B>(
    s: S,
    limit: u64,
) -> impl futures_util::Stream<Item = Result<B, std::io::Error>>
where
    S: futures_util::Stream<Item = Result<B, std::io::Error>>,
    B: AsRef<[u8]>,
{
    use futures_util::StreamExt as _;
    s.scan((0u64, false), move |(total, tripped), item| {
        let out = if *tripped {
            None
        } else {
            if let Ok(bytes) = &item {
                *total += bytes.as_ref().len() as u64;
            }
            if *total > limit {
                *tripped = true;
                Some(Err(std::io::Error::other(format!(
                    "byok: response exceeded {limit} bytes"
                ))))
            } else {
                Some(item)
            }
        };
        futures_util::future::ready(out)
    })
}

/// What a redacted credential reads as in provider text.
const REDACTED: &str = "<redacted>";

/// Header and query values shorter than this are left alone: replacing a
/// two-letter value would mangle ordinary text. The key is replaced at any
/// length.
const MIN_REDACT_LEN: usize = 4;

/// Every credential of one BYOK hop as it may appear in provider text (spec
/// §8.2): the key, each header value, the URL, and each value in the URL's
/// query string, raw and decoded. Longest first, so a URL is replaced whole
/// before a value inside it. No `Debug`: it holds the secrets it hides.
pub(crate) struct Redactor(Vec<String>);

impl Redactor {
    pub(crate) fn new(
        api_key: &str,
        headers: Option<&reqwest::header::HeaderMap>,
        url: &str,
    ) -> Self {
        let mut found = vec![api_key.to_string(), url.to_string()];
        found.extend(
            headers
                .into_iter()
                .flat_map(|h| h.values())
                .filter_map(|v| std::str::from_utf8(v.as_bytes()).ok())
                .map(str::to_string),
        );
        if let Ok(parsed) = reqwest::Url::parse(url) {
            found.push(parsed.as_str().to_string());
            if let Some(query) = parsed.query() {
                found.extend(
                    query
                        .split('&')
                        .map(|pair| pair.split_once('=').map_or(pair, |(_, v)| v))
                        .map(str::to_string),
                );
            }
            found.extend(parsed.query_pairs().map(|(_, v)| v.into_owned()));
        }
        found.retain(|s| s.len() >= MIN_REDACT_LEN);
        if !api_key.is_empty() {
            found.push(api_key.to_string());
        }
        found.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        found.dedup();
        Self(found)
    }

    /// `text` with every credential replaced by `<redacted>`.
    pub(crate) fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in &self.0 {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
        out
    }

    /// A parsed error body with every credential replaced, in each field. Run
    /// after parsing as well as before it: a JSON-escaped credential (`sk\/x`)
    /// only reads verbatim once the envelope is decoded.
    pub(crate) fn apply_body(
        &self,
        body: crate::openrouter::ParsedErrorBody,
    ) -> crate::openrouter::ParsedErrorBody {
        let apply = |s: Option<String>| s.map(|s| self.apply(&s));
        crate::openrouter::ParsedErrorBody {
            code: apply(body.code),
            error_type: apply(body.error_type),
            provider_code: apply(body.provider_code),
            message: self.apply(&body.message),
        }
    }
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
            "fec0::1",
            "feff::1",
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

    #[test]
    fn a_replacement_may_not_outgrow_the_shortest_match() {
        for (pattern, replacement) in [
            (r"\s*\[note:[^\]]*\]\s*$", ""),
            ("ab", "x"),
            ("ab", "xy"),
            ("(?:)", ""),
            ("你", "abc"),
            // Matches nothing, so it rewrites nothing.
            ("[a&&b]", "anything"),
        ] {
            assert!(
                replacement_is_non_growing(pattern, replacement),
                "{pattern} → {replacement}"
            );
        }
        for (pattern, replacement) in [
            ("(?:)", "x"),
            (r"\s*", " "),
            ("x", "yy"),
            ("你", "abcd"),
            ("abcdef", "$0"),
            ("(abcdef)", "$1"),
            ("abcdef", "$$"),
            ("(", ""),
        ] {
            assert!(
                !replacement_is_non_growing(pattern, replacement),
                "{pattern} → {replacement}"
            );
        }
    }

    #[test]
    fn redactor_replaces_every_credential_longest_first() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-org", "hdr-SECRET".parse().unwrap());
        headers.insert("x-tiny", "abc".parse().unwrap());
        let url = "https://api.example.com/v1/chat?key=Q%2BSECRET&flag";
        let r = Redactor::new("sk-KEY-1", Some(&headers), url);
        assert_eq!(
            r.apply(&format!(
                "{url} sk-KEY-1 hdr-SECRET Q%2BSECRET Q+SECRET flag abc"
            )),
            "<redacted> <redacted> <redacted> <redacted> <redacted> <redacted> abc"
        );
    }

    #[test]
    fn redactor_replaces_a_short_key() {
        let r = Redactor::new("k1", None, "https://api.example.com/v1/chat");
        assert_eq!(
            r.apply("Incorrect API key: k1"),
            "Incorrect API key: <redacted>"
        );
    }

    #[tokio::test]
    async fn byte_capped_ends_with_one_error_past_its_limit() {
        use futures_util::StreamExt;
        let chunks =
            futures_util::stream::iter((0..10).map(|_| Ok::<_, std::io::Error>(vec![0u8; 100])));
        let out: Vec<_> = byte_capped(chunks, 250).collect().await;
        assert_eq!(out.len(), 3, "two chunks fit, the third trips the bound");
        assert!(out[..2].iter().all(Result::is_ok));
        assert_eq!(
            out[2].as_ref().unwrap_err().to_string(),
            "byok: response exceeded 250 bytes"
        );
    }

    #[tokio::test]
    async fn read_capped_stops_at_its_cap() {
        use wiremock::{matchers::any, Mock, MockServer, ResponseTemplate};
        let mock = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(500).set_body_string("e".repeat(200 * 1024)))
            .mount(&mock)
            .await;
        let resp = reqwest::get(mock.uri()).await.unwrap();
        assert_eq!(
            read_capped(resp, ERROR_BODY_MAX).await.len(),
            ERROR_BODY_MAX
        );
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
