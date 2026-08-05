use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use futures_util::StreamExt;
use reqwest::{Method, StatusCode, header::LOCATION};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::lookup_host;
use url::Url;

use super::ToolError;

const MAX_REDIRECTS: usize = 5;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    url: String,
    #[serde(default = "default_method")]
    method: String,
}

fn default_method() -> String {
    "GET".into()
}

pub async fn fetch(
    value: &Value,
    max_bytes: usize,
    allow_private: bool,
) -> Result<String, ToolError> {
    let args: FetchArgs = serde_json::from_value(value.clone())?;
    let method = match args.method.as_str() {
        "GET" => Method::GET,
        "HEAD" => Method::HEAD,
        _ => {
            return Err(ToolError::Execution(
                "web_fetch supports only GET and HEAD".into(),
            ));
        }
    };
    let mut url = Url::parse(&args.url).map_err(execution_error)?;

    for redirect_count in 0..=MAX_REDIRECTS {
        let addresses = validate_target(&url, allow_private).await?;
        let host = url
            .host_str()
            .ok_or_else(|| ToolError::Security("URL has no host".into()))?;
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none());
        if host.parse::<IpAddr>().is_err() {
            for address in addresses {
                builder = builder.resolve(host, address);
            }
        }
        let client = builder.build().map_err(execution_error)?;
        let response = client
            .request(method.clone(), url.clone())
            .header("user-agent", "1H-Agent/0.1")
            .send()
            .await
            .map_err(execution_error)?;

        if response.status().is_redirection() {
            if redirect_count == MAX_REDIRECTS {
                return Err(ToolError::Execution("too many redirects".into()));
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| ToolError::Execution("redirect has no valid Location".into()))?;
            url = url.join(location).map_err(execution_error)?;
            continue;
        }

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        if method == Method::HEAD {
            return Ok(json!({
                "url": url.as_str(),
                "status": status.as_u16(),
                "content_type": content_type,
            })
            .to_string());
        }
        if status == StatusCode::NO_CONTENT {
            return Ok(String::new());
        }

        let mut body = Vec::with_capacity(max_bytes.min(64 * 1024));
        let mut stream = response.bytes_stream();
        let mut truncated = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(execution_error)?;
            let remaining = max_bytes.saturating_sub(body.len());
            if chunk.len() > remaining {
                body.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }

        let text = if content_type.contains("text/html") {
            html2text::from_read(body.as_slice(), 100)
                .map_err(|error| ToolError::Execution(error.to_string()))?
        } else if content_type.starts_with("text/")
            || content_type.contains("json")
            || content_type.contains("xml")
        {
            String::from_utf8_lossy(&body).into_owned()
        } else {
            return Ok(json!({
                "url": url.as_str(),
                "status": status.as_u16(),
                "content_type": content_type,
                "bytes": body.len(),
                "message": "binary body omitted"
            })
            .to_string());
        };
        return Ok(format!(
            "URL: {url}\nStatus: {}\nContent-Type: {content_type}\nTruncated: {truncated}\n\n{text}",
            status.as_u16()
        ));
    }
    Err(ToolError::Execution("redirect handling failed".into()))
}

async fn validate_target(url: &Url, allow_private: bool) -> Result<Vec<SocketAddr>, ToolError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError::Security(
            "only HTTP and HTTPS URLs are allowed".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ToolError::Security(
            "credentials in URLs are not allowed".into(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| ToolError::Security("URL has no host".into()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| ToolError::Security("URL has no valid port".into()))?;
    let addresses: Vec<SocketAddr> = lookup_host((host, port))
        .await
        .map_err(execution_error)?
        .collect();
    if addresses.is_empty() {
        return Err(ToolError::Execution("host did not resolve".into()));
    }
    if !allow_private {
        for address in &addresses {
            if !is_public(address.ip()) {
                return Err(ToolError::Security(format!(
                    "private or local address is blocked: {}",
                    address.ip()
                )));
            }
        }
    }
    Ok(addresses)
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && (segments[0] & 0xfe00) != 0xfc00
                && (segments[0] & 0xffc0) != 0xfe80
                && (segments[0] & 0xffc0) != 0xfec0
                && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
                && ip
                    .to_ipv4_mapped()
                    .is_none_or(|mapped| is_public(IpAddr::V4(mapped)))
        }
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _d] = ip.octets();
    !matches!(
        (a, b, c),
        (0, _, _)
            | (10, _, _)
            | (100, 64..=127, _)
            | (127, _, _)
            | (169, 254, _)
            | (172, 16..=31, _)
            | (192, 0, 0)
            | (192, 0, 2)
            | (192, 168, _)
            | (198, 18..=19, _)
            | (198, 51, 100)
            | (203, 0, 113)
            | (224..=255, _, _)
    )
}

fn execution_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Execution(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn blocks_non_public_addresses() {
        assert!(!is_public(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!is_public(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert!(!is_public(IpAddr::V4(Ipv4Addr::new(100, 64, 1, 1))));
        assert!(!is_public(IpAddr::V4(Ipv4Addr::new(240, 1, 2, 3))));
        assert!(!is_public(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_public(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }
}
