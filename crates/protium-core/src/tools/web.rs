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
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default = "default_search_results")]
    max_results: usize,
}

fn default_search_results() -> usize {
    5
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
    let max_bytes = args.max_bytes.unwrap_or(max_bytes).min(max_bytes);
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

pub async fn search(
    value: &Value,
    max_bytes: usize,
    allow_private: bool,
) -> Result<String, ToolError> {
    let args: SearchArgs = serde_json::from_value(value.clone())?;
    let query = args.query.trim();
    if query.is_empty() {
        return Err(ToolError::Execution("web_search query is empty".into()));
    }
    let max_results = args.max_results.clamp(1, 10);
    let output_limit = max_bytes.min(64 * 1024);
    let mut url = Url::parse("https://html.duckduckgo.com/html/").map_err(execution_error)?;
    url.query_pairs_mut().append_pair("q", query);
    let fetched = fetch(
        &json!({"url": url.as_str(), "method": "GET", "max_bytes": output_limit}),
        output_limit,
        allow_private,
    )
    .await?;
    Ok(limit_search_output(
        query,
        &fetched,
        max_results,
        output_limit,
    ))
}

fn limit_search_output(query: &str, fetched: &str, max_results: usize, max_bytes: usize) -> String {
    let mut output = format!("Search query: {query}\nSearch provider: DuckDuckGo HTML\n\n");
    let mut results = 0usize;
    for block in fetched.split("\n\n") {
        let is_result =
            block.contains("](") || block.contains("http://") || block.contains("https://");
        if !is_result || block.starts_with("URL:") {
            continue;
        }
        if results == max_results {
            break;
        }
        if output.len() + block.len() + 2 > max_bytes {
            output.push_str("[search output truncated]\n");
            break;
        }
        output.push_str(block.trim());
        output.push_str("\n\n");
        results += 1;
    }
    if results == 0 {
        let remaining = max_bytes.saturating_sub(output.len());
        let fallback = bounded_utf8(fetched, remaining);
        output.push_str(fallback);
    }
    output
}

fn bounded_utf8(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
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
        let literal_ip = host.parse::<IpAddr>().ok();
        for address in &addresses {
            // Clash and similar transparent DNS proxies synthesize 198.18/15
            // answers. Permit that range only for a hostname; literal private
            // or benchmark IP URLs remain blocked.
            let proxy_fake_ip = literal_ip.is_none() && is_dns_proxy_fake_ip(address.ip());
            if !is_public(address.ip()) && !proxy_fake_ip {
                return Err(ToolError::Security(format!(
                    "private or local address is blocked: {}",
                    address.ip()
                )));
            }
        }
    }
    Ok(addresses)
}

fn is_dns_proxy_fake_ip(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(ip) if matches!(ip.octets(), [198, 18..=19, _, _]))
}

pub(crate) async fn validate_public_url(value: &str, allow_private: bool) -> Result<(), ToolError> {
    let url = Url::parse(value).map_err(execution_error)?;
    validate_target(&url, allow_private).await.map(|_| ())
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
        assert!(is_dns_proxy_fake_ip(IpAddr::V4(Ipv4Addr::new(
            198, 18, 1, 1
        ))));
        assert!(!is_dns_proxy_fake_ip(IpAddr::V4(Ipv4Addr::new(
            192, 168, 1, 1
        ))));
    }

    #[test]
    fn search_output_is_bounded_by_result_count() {
        let fetched = "URL: https://example.test\n\n[first](https://one.test)\n\n[second](https://two.test)\n\n[third](https://three.test)";
        let output = limit_search_output("rust", fetched, 2, 4096);
        assert!(output.contains("one.test"));
        assert!(output.contains("two.test"));
        assert!(!output.contains("three.test"));
    }

    #[test]
    fn literal_fake_ip_is_not_public() {
        assert!(!is_public(IpAddr::V4(Ipv4Addr::new(198, 18, 1, 1))));
    }

    #[tokio::test]
    #[ignore = "requires public network access"]
    async fn public_search_smoke_test() {
        let output = search(
            &json!({"query": "Rust Tokio", "max_results": 3}),
            64 * 1024,
            false,
        )
        .await
        .expect("public search should succeed");
        assert!(output.contains("Search query: Rust Tokio"));
        assert!(output.to_ascii_lowercase().contains("tokio"));
        assert!(output.len() <= 64 * 1024);
    }
}
