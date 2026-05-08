//! Local-network helpers for OTel sem-conv `source.address`.
//!
//! Picks the IPC's primary IPv4 address by enumerating active network
//! interfaces via `if-addrs` (`getifaddrs` / `GetAdaptersAddresses`).
//! Internet-free; works on segregated industrial networks that have
//! no default gateway. The DHCP-assigned IP shows up automatically
//! because the OS reports whichever IP the interface currently holds.
//!
//! Cached in a `OnceLock` so the (cheap) syscall happens once per
//! process, not per emitted record.

use std::net::IpAddr;
use std::sync::OnceLock;

static CACHED: OnceLock<String> = OnceLock::new();

/// Return the IPC's primary non-loopback IPv4 address, or
/// `127.0.0.1` if no other interface is available.
///
/// Selection rules:
/// 1. First non-loopback, non-link-local IPv4 wins (matches what
///    a user-facing config would call "the LAN IP" — DHCP-leased
///    or statically assigned).
/// 2. If only link-local (`169.254/16`) IPv4s exist, fall through
///    to one of those — better a real-but-link-local address than
///    a misleading loopback.
/// 3. If only loopback or no IPv4 at all, return `127.0.0.1`.
pub fn local_source_address() -> &'static str {
    CACHED.get_or_init(detect).as_str()
}

fn detect() -> String {
    let addrs = match if_addrs::get_if_addrs() {
        Ok(a) => a,
        Err(_) => return "127.0.0.1".to_string(),
    };

    let mut link_local: Option<IpAddr> = None;

    for iface in addrs {
        if iface.is_loopback() {
            continue;
        }
        let ip = iface.ip();
        let v4 = match ip {
            IpAddr::V4(v) => v,
            IpAddr::V6(_) => continue,
        };
        if v4.is_link_local() {
            // Stash the first link-local as a fallback but keep
            // looking for a routable address.
            if link_local.is_none() {
                link_local = Some(IpAddr::V4(v4));
            }
            continue;
        }
        if v4.is_unspecified() {
            continue;
        }
        return v4.to_string();
    }

    match link_local {
        Some(ip) => ip.to_string(),
        None => "127.0.0.1".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_some_address() {
        // Cannot assert a specific value (depends on host network),
        // but the function must always produce a parseable IPv4
        // string — never an empty / panic-on-cache scenario.
        let s = local_source_address();
        assert!(!s.is_empty());
        let _: std::net::Ipv4Addr = s.parse().expect("valid IPv4 string");
    }

    #[test]
    fn cache_is_stable() {
        let a = local_source_address();
        let b = local_source_address();
        assert_eq!(a, b);
    }
}
