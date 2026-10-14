//! Outbound routing for the two things that need the network: model weights and updates.
//!
//! A GUI launch from Finder carries none of the terminal's `HTTPS_PROXY`, and a direct
//! connection to GitHub or the HF CDN is not reliable here — so the proxy is a setting, and
//! we only use it if it actually accepts a connection. That probe costs one failed local
//! TCP connect when no proxy is running, instead of a request timeout on every fetch.

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::{Error, Result};

/// How long to wait for a local proxy to answer. Anything slower than this is not worth
/// routing through: the direct path would already have started.
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// `host:port` out of `http://host:port[/path]`. Only the authority matters for a probe.
pub fn proxy_authority(proxy: &str) -> Option<(String, u16)> {
    let without_scheme = proxy
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(proxy);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()?
        // credentials belong to the request, not the socket
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(without_scheme.split(['/', '?', '#']).next()?);
    if authority.is_empty() {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => (h, p.parse::<u16>().ok()?),
        // no port written out: plain `host` is not a usable proxy either
        _ => return None,
    };
    Some((host.to_string(), port))
}

/// True when something is listening at the proxy address.
pub fn proxy_alive(proxy: &str) -> bool {
    let Some((host, port)) = proxy_authority(proxy) else {
        return false;
    };
    let Ok(addr) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    addr.into_iter()
        .any(|a| TcpStream::connect_timeout(&a, PROBE_TIMEOUT).is_ok())
}

/// The proxy to route through, or `None` for a direct connection.
pub fn pick_proxy(configured: Option<&str>) -> Option<String> {
    let p = configured.map(str::trim).filter(|s| !s.is_empty())?;
    proxy_alive(p).then(|| p.to_string())
}

/// A blocking client for downloads, with the same routing decision the updater makes.
pub fn download_client(user_agent: &str, proxy: Option<&str>) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        .user_agent(user_agent)
        .connect_timeout(Duration::from_secs(20))
        // a stalled CDN must not freeze the download thread forever
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_nodelay(true);
    if let Some(p) = pick_proxy(proxy) {
        builder = builder.proxy(
            reqwest::Proxy::all(&p).map_err(|e| Error::Download(format!("代理地址无效：{e}")))?,
        );
    }
    builder.build().map_err(|e| Error::Download(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_authority_out_of_a_proxy_url() {
        assert_eq!(
            proxy_authority("http://127.0.0.1:7890"),
            Some(("127.0.0.1".into(), 7890))
        );
        assert_eq!(
            proxy_authority("http://user:pw@proxy.local:8080/with/path"),
            Some(("proxy.local".into(), 8080))
        );
        assert_eq!(proxy_authority("http://127.0.0.1"), None, "a port is required");
        assert_eq!(proxy_authority(""), None);
        assert_eq!(proxy_authority("http://host:notaport"), None);
    }

    /// Nothing should be listening on this port; if the probe said yes we would route every
    /// download into a dead proxy and the user would only see slow, failing fetches.
    #[test]
    fn an_unreachable_proxy_is_not_picked() {
        assert!(!proxy_alive("http://127.0.0.1:1"));
        assert_eq!(pick_proxy(Some("http://127.0.0.1:1")), None);
    }

    #[test]
    fn blank_or_absent_proxy_means_direct() {
        assert_eq!(pick_proxy(None), None);
        assert_eq!(pick_proxy(Some("   ")), None);
    }

    #[test]
    fn a_listening_proxy_is_picked() {
        let _listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = _listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        assert!(proxy_alive(&url), "{url} should be reachable");
        assert_eq!(pick_proxy(Some(&url)).as_deref(), Some(url.as_str()));
    }
}
