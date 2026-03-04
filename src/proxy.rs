use std::net::IpAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::Request;
use axum::response::{IntoResponse, Response};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use crate::api::AppState;
use crate::error::DenError;

/// Validate that an IP is safe to proxy to (container IP, not host/metadata/loopback).
fn is_safe_proxy_target(ip: &str) -> bool {
    let addr: IpAddr = match ip.parse() {
        Ok(a) => a,
        Err(_) => return false,
    };

    match addr {
        IpAddr::V4(v4) => {
            // Block loopback (127.x.x.x)
            if v4.is_loopback() {
                return false;
            }
            // Block link-local / cloud metadata (169.254.x.x)
            if v4.is_link_local() {
                return false;
            }
            let octets = v4.octets();
            // Block host-only ranges that aren't Docker bridge
            // Docker bridge default is 172.17.0.0/16
            // Docker custom networks use 172.18-31.0.0/16
            // Allow only 172.16-31.x.x (Docker range) and 192.168.x.x
            match octets[0] {
                172 if (16..=31).contains(&octets[1]) => true,
                192 if octets[1] == 168 => true,
                _ => false,
            }
        }
        IpAddr::V6(_) => false, // Docker containers don't typically use IPv6 for bridge
    }
}

/// Reverse proxy handler: forwards requests to container_ip:port.
pub async fn proxy_handler(
    State(state): State<Arc<AppState>>,
    Path((sandbox_id, port, rest)): Path<(String, u16, String)>,
    req: Request<Body>,
) -> Result<Response, DenError> {
    let container_id = {
        let entry = state
            .pool
            .sandboxes
            .get(&sandbox_id)
            .ok_or_else(|| DenError::SandboxNotFound {
                id: sandbox_id.clone(),
            })?;
        entry.value().container_id.clone()
    };

    let ip = state.docker.get_container_ip(&container_id).await?;

    if !is_safe_proxy_target(&ip) {
        return Err(DenError::Proxy(format!(
            "container IP {ip} is not in an allowed range"
        )));
    }

    let uri = format!("http://{ip}:{port}/{rest}");

    let client = Client::builder(TokioExecutor::new()).build_http();

    let (mut parts, body) = req.into_parts();
    parts.uri = uri
        .parse()
        .map_err(|e| DenError::Proxy(format!("bad URI: {e}")))?;

    let proxied_req = Request::from_parts(parts, body);

    let response = client
        .request(proxied_req)
        .await
        .map_err(|e| DenError::Proxy(format!("upstream: {e}")))?;

    Ok(response.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_docker_bridge_ips() {
        assert!(is_safe_proxy_target("172.17.0.2"));
        assert!(is_safe_proxy_target("172.18.0.5"));
        assert!(is_safe_proxy_target("172.31.255.255"));
        assert!(is_safe_proxy_target("192.168.1.100"));
    }

    #[test]
    fn blocks_loopback() {
        assert!(!is_safe_proxy_target("127.0.0.1"));
        assert!(!is_safe_proxy_target("127.0.0.2"));
    }

    #[test]
    fn blocks_link_local_metadata() {
        assert!(!is_safe_proxy_target("169.254.169.254")); // cloud metadata
        assert!(!is_safe_proxy_target("169.254.1.1"));
    }

    #[test]
    fn blocks_public_ips() {
        assert!(!is_safe_proxy_target("8.8.8.8"));
        assert!(!is_safe_proxy_target("10.0.0.1"));
        assert!(!is_safe_proxy_target("1.1.1.1"));
    }

    #[test]
    fn blocks_ipv6() {
        assert!(!is_safe_proxy_target("::1"));
        assert!(!is_safe_proxy_target("fe80::1"));
    }

    #[test]
    fn blocks_invalid_input() {
        assert!(!is_safe_proxy_target("not-an-ip"));
        assert!(!is_safe_proxy_target(""));
    }

    #[test]
    fn blocks_outside_docker_range() {
        assert!(!is_safe_proxy_target("172.15.0.1")); // below 172.16
        assert!(!is_safe_proxy_target("172.32.0.1")); // above 172.31
        assert!(!is_safe_proxy_target("192.167.1.1")); // not 192.168
    }
}
