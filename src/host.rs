//! Protection against DNS rebinding.
//!
//! A website can point its own domain at a LAN address, after which the browser treats this
//! server as part of that website and lets the site's scripts read from it and upload to it.
//! Turning away requests whose `Host` isn't a name this server could legitimately be reached by
//! stops that. Rebinding needs a public domain the attacker controls, so IP addresses,
//! `localhost` and LAN host names are all safe to accept without being told about them.

use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::uri::Authority;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Suffixes of names that only resolve on the local network, so can't belong to an attacker.
const LAN_SUFFIXES: &[&str] = &[".local", ".lan", ".home.arpa", ".internal", ".localhost"];

/// Host names accepted on top of the built-in ones, given with `--allow-host`.
pub struct AllowedHosts(Vec<String>);

impl AllowedHosts {
    pub fn new(hosts: Vec<String>) -> Self {
        Self(hosts.iter().map(|h| normalise(h)).collect())
    }

    /// Whether `host` (a `Host` header value, with or without a port) is acceptable.
    fn allows(&self, host: &str) -> bool {
        let Ok(authority) = host.parse::<Authority>() else {
            return false;
        };
        let name = normalise(authority.host());
        let name = name.trim_start_matches('[').trim_end_matches(']');
        name.parse::<IpAddr>().is_ok()
            || name == "localhost"
            || !name.contains('.')
            || LAN_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
            || self.0.iter().any(|allowed| allowed == name)
    }
}

/// Lowercases a host name and drops the trailing dot of a fully qualified name.
fn normalise(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Middleware that turns away requests addressed to a host name that isn't allowed.
///
/// Requests without a host at all are let through: browsers always send one, so they can't be
/// part of a rebinding attack, and some command-line tools leave it out.
pub async fn require(
    State(allowed): State<Arc<AllowedHosts>>,
    req: Request,
    next: Next,
) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .map(|h| h.to_str().unwrap_or_default())
        .or_else(|| req.uri().authority().map(Authority::as_str));
    match host {
        Some(host) if !allowed.allows(host) => (
            StatusCode::MISDIRECTED_REQUEST,
            format!(
                "upserve doesn't answer to {host}; start it with --allow-host to allow that name"
            ),
        )
            .into_response(),
        _ => next.run(req).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_addresses_and_lan_names() {
        let allowed = AllowedHosts::new(vec![]);
        for host in [
            "192.168.1.20:8000",
            "10.0.0.1",
            "[::1]:8000",
            "[fe80::1]",
            "localhost:8000",
            "mypc:8000",
            "MyPC.",
            "mypc.local",
            "nas.lan:8000",
            "mypc.home.arpa",
            "box.internal",
            "app.localhost",
        ] {
            assert!(allowed.allows(host), "{host} should be allowed");
        }
    }

    #[test]
    fn rejects_other_domains() {
        let allowed = AllowedHosts::new(vec!["Files.Example.com.".into()]);
        for host in [
            "attacker.example",
            "attacker.example:8000",
            "192.168.1.20.attacker.example",
            "local.attacker.example",
            "attacker.example.",
            "",
            "not a host",
        ] {
            assert!(!allowed.allows(host), "{host:?} should be rejected");
        }
        assert!(allowed.allows("files.example.com:8000"));
    }
}
