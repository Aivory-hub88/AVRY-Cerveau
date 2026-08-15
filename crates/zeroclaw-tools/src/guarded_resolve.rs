//! Guarded fetcher — SSRF-safe HTTPS transport for tenant-supplied MCP
//! server URLs (ADR-006 Part B). Mirrors `guarded_fetch.py` in avry-backend
//! 1:1 on the deny-list logic; the two are independent implementations by
//! design (ADR-006 §B3: the runtime call must not add a Postgres/AES
//! round-trip into avry-backend on every tool call), but must stay
//! behaviorally identical since they enforce the same security boundary at
//! two different points in the pipeline — registration-time verification
//! (Python) and every runtime call (this module).
//!
//! The whole Aivory stack is colocated on one VPS via loopback ports. A
//! tenant-registered URL resolving to `127.0.0.1` is the single most
//! dangerous payload this architecture can receive — it would let a
//! "verified" tenant MCP server probe Cerveau's own webhook or any other
//! loopback-bound internal service from inside Aivory's own trust boundary.
//!
//! Controls (ADR-006 §B4), enforced identically at every call:
//!
//! 1. `https://` only — the caller (B2/B3 wiring) must reject non-https
//!    URLs before ever reaching this module; this module has no HTTP
//!    fallback to accidentally use.
//! 2. DNS resolved via [`GuardedResolver`]; every resolved address is
//!    validated against [`is_ip_denied`] before the address is handed back
//!    to reqwest to connect to. If ANY resolved answer is denied, resolution
//!    fails for the whole hostname — not just that one address — since a
//!    client can reconnect on any answer a multi-A-record host returns.
//! 3. reqwest connects to exactly the address [`GuardedResolver::resolve`]
//!    returned — there is no separate "validate, then let something else
//!    resolve again to connect" step, which is what closes the
//!    DNS-rebinding TOCTOU gap. Host/SNI still come from the original
//!    hostname (reqwest's normal behavior; the resolver only supplies the
//!    connect address).
//! 4. `redirect::Policy::none()` — the client returned by
//!    [`build_guarded_client`] never auto-follows a redirect. A caller that
//!    wants to follow one MUST re-issue the request for the `Location`
//!    target through this same client (or a fresh call to
//!    `build_guarded_client`) rather than any other HTTP path, so the new
//!    target goes through steps 1-3 again from scratch.
//! 5. Every call re-resolves and re-validates — nothing here caches a host
//!    as "safe forever" from a past check.
//! 6. [`read_capped_body`] enforces a response size cap via a streaming
//!    byte-counter, not `Content-Length` (a tenant server can lie about or
//!    omit it).
//! 7. [`build_guarded_client`] takes explicit connect/total timeouts — the
//!    caller is expected to pass short bounds for registration-time
//!    verification and a larger (but still bounded) budget for runtime
//!    tool calls, never unbounded.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Response size cap shared by every guarded fetch — matches
/// `guarded_fetch.py`'s `MAX_RESPONSE_BYTES`.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// stdlib `Ipv4Addr::is_private()` (RFC1918) does not cover RFC6598
/// "Shared Address Space" for carrier-grade NAT — checked explicitly here,
/// same reasoning as `guarded_fetch.py`'s `_CGNAT_RANGE`.
fn is_cgnat(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0b1100_0000) == 0b0100_0000 // 100.64.0.0/10
}

/// Class E / other reserved space (240.0.0.0/4) — also not covered by
/// stable `Ipv4Addr` methods (`is_reserved()` is nightly-only as of this
/// toolchain).
fn is_reserved_v4(ip: Ipv4Addr) -> bool {
    ip.octets()[0] >= 240
}

fn is_ipv4_denied(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private() // RFC1918: 10/8, 172.16/12, 192.168/16
        || ip.is_link_local() // 169.254.0.0/16 — AWS/GCP/Azure's
        // 169.254.169.254 *and* Tencent Cloud's own 169.254.0.23 metadata
        // endpoint (this VPS is Tencent)
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || is_cgnat(ip)
        || is_reserved_v4(ip)
}

fn is_ipv6_denied(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_ipv4_denied(mapped);
    }
    ip.is_loopback()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.is_unique_local() // fc00::/7 (IPv6 private-use)
        || ip.is_unicast_link_local() // fe80::/10
}

/// Returns true if `ip` must never be connected to by a guarded fetch.
/// Unparseable/unknown never happens here (the type is already `IpAddr`),
/// so unlike the Python sibling there's no "unparseable -> deny" branch —
/// the type system already rules that case out.
pub fn is_ip_denied(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_ipv4_denied(v4),
        IpAddr::V6(v6) => is_ipv6_denied(v6),
    }
}

#[derive(Debug)]
struct GuardedResolveError(String);

impl std::fmt::Display for GuardedResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GuardedResolveError {}

/// `reqwest::dns::Resolve` implementation that performs real DNS
/// resolution, validates every answer, and — critically — hands back only
/// the exact validated addresses reqwest will connect to. There is no
/// separate resolve-then-reresolve step anywhere in the request path, which
/// is what makes this immune to DNS rebinding (a hostname that answers
/// differently on a second lookup can't matter, because there is no second
/// lookup for the same connection).
#[derive(Debug, Default)]
pub struct GuardedResolver;

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move { resolve_and_validate(&host).await })
    }
}

async fn resolve_and_validate(
    host: &str,
) -> Result<Addrs, Box<dyn std::error::Error + Send + Sync>> {
    // Port 0 here is a placeholder — reqwest overrides it with the real
    // request port per the `Resolve` trait's documented contract ("port 0
    // will be replaced by the conventional port... explicit port in the
    // URL will override").
    let lookup = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|e| GuardedResolveError(format!("DNS resolution failed for '{host}': {e}")))?;

    let addrs: Vec<SocketAddr> = lookup.collect();
    if addrs.is_empty() {
        return Err(Box::new(GuardedResolveError(format!(
            "DNS resolution returned no addresses for '{host}'"
        ))));
    }

    if addrs.iter().any(|a| is_ip_denied(a.ip())) {
        // Any denied address among the answers is a full reject, not just
        // skip-that-one — a multi-answer host that resolves to both a
        // public IP and 127.0.0.1 is exactly the DNS-rebinding shape this
        // guard exists to close, and a client can reconnect on any answer.
        return Err(Box::new(GuardedResolveError(format!(
            "'{host}' resolves to a disallowed address"
        ))));
    }

    let boxed: Addrs = Box::new(addrs.into_iter());
    Ok(boxed)
}

/// Builds a `reqwest::Client` that routes all DNS resolution through
/// [`GuardedResolver`] and never auto-follows redirects (§B4 item 4). The
/// caller supplies both a connect timeout and a total per-request timeout —
/// short bounds (~3s / ~10s) for registration-time verification, a larger
/// but still-bounded budget for runtime tool calls (~20-30s), per ADR-006
/// §B4 item 7. Every call builds a fresh client rather than sharing one
/// across differently-trusted callers, so a misconfigured timeout in one
/// caller can never leak into another's budget.
pub fn build_guarded_client(
    connect_timeout: Duration,
    total_timeout: Duration,
) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(GuardedResolver))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(connect_timeout)
        .timeout(total_timeout)
        .build()
}

/// Streams `resp`'s body, aborting as soon as more than `max_bytes` have
/// been read rather than draining the whole thing first — a malicious or
/// misbehaving tenant server can't force a large read by omitting or lying
/// about `Content-Length`.
pub async fn read_capped_body(
    resp: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, anyhow::Error> {
    use futures_util::StreamExt;

    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        if buf.len() > max_bytes {
            anyhow::bail!("response exceeded {max_bytes}-byte cap");
        }
    }
    Ok(buf)
}

/// Convenience wrapper: fetch a URL (already validated as https:// by the
/// caller) through a fresh guarded client, capping response size and using
/// the given timeouts. Returns the raw response for the caller to inspect
/// `status()`/headers — including 3xx, since redirects are never followed
/// automatically (see module docs, item 4).
pub async fn guarded_get(
    url: &str,
    connect_timeout: Duration,
    total_timeout: Duration,
) -> anyhow::Result<(reqwest::StatusCode, reqwest::header::HeaderMap, Vec<u8>)> {
    let client = build_guarded_client(connect_timeout, total_timeout)?;
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = read_capped_body(resp, MAX_RESPONSE_BYTES).await?;
    Ok((status, headers, body))
}

// Type aliases kept private; exposed only so the module compiles standalone
// without pulling extra pub surface callers don't need yet (B2/B3 wire
// their own higher-level calls on top of `guarded_get`/`build_guarded_client`).
#[allow(dead_code)]
type _UnusedFutureAlias = Pin<Box<dyn Future<Output = ()> + Send>>;

#[cfg(test)]
mod tests {
    use super::*;

    // ── Deny-list matrix — mirrors tests/test_guarded_fetch.py 1:1 ────────

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn denies_loopback() {
        assert!(is_ip_denied(ip("127.0.0.1")));
        assert!(is_ip_denied(ip("127.5.5.5")));
        assert!(is_ip_denied(ip("::1")));
    }

    #[test]
    fn denies_rfc1918() {
        assert!(is_ip_denied(ip("10.0.0.5")));
        assert!(is_ip_denied(ip("172.16.0.1")));
        assert!(is_ip_denied(ip("172.31.255.255")));
        assert!(is_ip_denied(ip("192.168.1.1")));
    }

    #[test]
    fn denies_link_local_including_both_cloud_metadata_ips() {
        assert!(is_ip_denied(ip("169.254.169.254"))); // AWS/GCP/Azure
        assert!(is_ip_denied(ip("169.254.0.23"))); // Tencent Cloud (this VPS)
        assert!(is_ip_denied(ip("fe80::1")));
    }

    #[test]
    fn denies_unspecified_and_multicast_and_reserved() {
        assert!(is_ip_denied(ip("0.0.0.0")));
        assert!(is_ip_denied(ip("::")));
        assert!(is_ip_denied(ip("224.0.0.1")));
        assert!(is_ip_denied(ip("ff02::1")));
        assert!(is_ip_denied(ip("240.0.0.1")));
    }

    #[test]
    fn denies_ipv4_mapped_ipv6_of_denied_addresses() {
        assert!(is_ip_denied(ip("::ffff:127.0.0.1")));
        assert!(is_ip_denied(ip("::ffff:10.1.2.3")));
        assert!(is_ip_denied(ip("::ffff:169.254.169.254")));
    }

    #[test]
    fn denies_carrier_grade_nat() {
        assert!(is_ip_denied(ip("100.64.0.1")));
        assert!(is_ip_denied(ip("100.127.255.255")));
        assert!(!is_ip_denied(ip("100.63.255.255"))); // just outside the range
        assert!(!is_ip_denied(ip("100.128.0.0"))); // just outside the range
    }

    #[test]
    fn denies_ipv6_unique_local() {
        assert!(is_ip_denied(ip("fc00::1")));
        assert!(is_ip_denied(ip("fd12:3456::1")));
    }

    #[test]
    fn allows_public_addresses() {
        assert!(!is_ip_denied(ip("8.8.8.8")));
        assert!(!is_ip_denied(ip("1.1.1.1")));
        assert!(!is_ip_denied(ip("2001:4860:4860::8888")));
    }

    // ── GuardedResolver — real (loopback) DNS resolution, no network ──────

    #[tokio::test]
    async fn resolver_rejects_ip_literal_loopback_hostname() {
        let resolver = GuardedResolver;
        let name: Name = "127.0.0.1".parse().unwrap();
        let result = resolver.resolve(name).await;
        assert!(result.is_err(), "loopback literal must be rejected");
    }

    #[tokio::test]
    async fn resolver_rejects_ip_literal_metadata_hostname() {
        let resolver = GuardedResolver;
        let name: Name = "169.254.169.254".parse().unwrap();
        let result = resolver.resolve(name).await;
        assert!(
            result.is_err(),
            "cloud metadata IP literal must be rejected"
        );
    }

    #[tokio::test]
    async fn resolver_rejects_ip_literal_tencent_metadata_hostname() {
        let resolver = GuardedResolver;
        let name: Name = "169.254.0.23".parse().unwrap();
        let result = resolver.resolve(name).await;
        assert!(
            result.is_err(),
            "Tencent metadata IP literal must be rejected"
        );
    }

    #[tokio::test]
    async fn resolver_accepts_ip_literal_public_hostname() {
        let resolver = GuardedResolver;
        let name: Name = "8.8.8.8".parse().unwrap();
        let result = resolver.resolve(name).await;
        assert!(result.is_ok(), "public IP literal must resolve");
    }

    // ── build_guarded_client — construction sanity ─────────────────────────

    #[test]
    fn guarded_client_builds_successfully() {
        let client = build_guarded_client(Duration::from_secs(3), Duration::from_secs(10));
        assert!(client.is_ok());
    }
}
