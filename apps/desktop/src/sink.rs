//! Where the snapshot goes: one object PUT into an S3-compatible bucket.
//!
//! The upload is blocking and runs on its own thread. Nothing here may surface
//! an error dialog; failures come back as values and the publisher backs off.

use std::time::Duration;

use crate::config::SinkConfig;
use crate::sigv4;

pub struct UploadError {
    pub message: String,
    /// True when retrying soon is pointless (bad credentials, wrong bucket); the
    /// publisher then backs off much harder instead of hammering the endpoint.
    pub fatal: bool,
    /// True when the host name could not be resolved. That is worth a fast retry
    /// rather than a long backoff: the endpoint is usually fine, the resolver is
    /// not, and a three minute silence is enough to look offline to a reader.
    pub dns: bool,
}

impl UploadError {
    fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fatal: false,
            dns: false,
        }
    }
    fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fatal: true,
            dns: false,
        }
    }
    fn dns(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fatal: false,
            dns: true,
        }
    }
}

/// Name resolution failures arrive as opaque OS strings, in whatever language the
/// system uses, so both the errno and the usual phrasings have to be recognised.
fn looks_like_dns_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "11001",
        "11002",
        "dns",
        "resolve",
        "lookup",
        "不知道这样的主机",
        "找不到主机",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub struct Agent {
    http: ureq::Agent,
}

impl Agent {
    pub fn new(proxy: Option<&str>) -> Self {
        let mut builder = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(10)))
            .user_agent(concat!("now-doing/", env!("CARGO_PKG_VERSION")));

        if let Some(url) = proxy.map(str::trim).filter(|s| !s.is_empty()) {
            match ureq::Proxy::new(url) {
                Ok(p) => builder = builder.proxy(Some(p)),
                Err(e) => log::warn!("ignoring bad proxy {url:?}: {e}"),
            }
        }

        // The default connector plus our own resolver: the system one fails on
        // api.cloudflare.com in this network, which would break the purge in a way
        // that is invisible from the outside.
        Self {
            http: ureq::Agent::with_parts(
                builder.build(),
                ureq::unversioned::transport::DefaultConnector::new(),
                crate::dnsfallback::FallbackResolver,
            ),
        }
    }

    /// PUT the JSON body at the configured object key. Returns the HTTP status on
    /// success so the caller can distinguish "stored" from "stored but odd".
    pub fn put_json(&self, sink: &SinkConfig, body: &str) -> Result<u16, UploadError> {
        let (host, base_path) = split_endpoint(&sink.endpoint)
            .ok_or_else(|| UploadError::fatal(format!("bad endpoint {:?}", sink.endpoint)))?;
        if sink.bucket.trim().is_empty() {
            return Err(UploadError::fatal("bucket is empty"));
        }
        if sink.access_key_id.trim().is_empty() || sink.secret_access_key.trim().is_empty() {
            return Err(UploadError::fatal("credentials are not configured"));
        }

        let key = sink.object_key.trim().trim_start_matches('/');
        let path = format!("{base_path}/{}/{}", sink.bucket.trim(), key);
        // The signature covers the percent-encoded path, so the request line has to
        // carry the same encoding or a key with a space comes back as a 403.
        let url = format!("https://{host}{}", sigv4::encode_path(&path));

        let amz_date = {
            let now = crate::util::unix_now();
            let (y, mo, d) = crate::util::civil_from_days(now.div_euclid(86_400));
            let rem = now.rem_euclid(86_400);
            format!(
                "{y:04}{mo:02}{d:02}T{:02}{:02}{:02}Z",
                rem / 3600,
                (rem % 3600) / 60,
                rem % 60
            )
        };
        let payload_hash = sigv4::sha256_hex(body.as_bytes());
        let authorization = sigv4::authorization(&sigv4::SigningInput {
            access_key: sink.access_key_id.trim(),
            secret_key: sink.secret_access_key.trim(),
            region: "auto",
            host: &host,
            path: &path,
            amz_date: &amz_date,
            payload_hash: &payload_hash,
            content_type: "application/json",
        });

        let result = self
            .http
            .put(&url)
            .header("content-type", "application/json")
            .header("x-amz-content-sha256", &payload_hash)
            .header("x-amz-date", &amz_date)
            .header("authorization", &authorization)
            .send(body);

        match result {
            Ok(res) => {
                let status = res.status().as_u16();
                if (200..300).contains(&status) {
                    Ok(status)
                } else {
                    Err(UploadError::retryable(format!("HTTP {status}")))
                }
            }
            Err(ureq::Error::StatusCode(code)) => {
                // 401/403/404 mean the credentials, bucket or key are wrong; retrying
                // cannot fix that, so climb the backoff ladder instead of hammering.
                if code == 401 || code == 403 || code == 404 {
                    Err(UploadError::fatal(format!("HTTP {code}")))
                } else {
                    Err(UploadError::retryable(format!("HTTP {code}")))
                }
            }
            Err(e) => {
                let message = e.to_string();
                if looks_like_dns_failure(&message) {
                    Err(UploadError::dns(message))
                } else {
                    Err(UploadError::retryable(message))
                }
            }
        }
    }

    /// Ask Cloudflare to drop the cached copy of the public URL.
    ///
    /// Best effort by design: the object already carries a short edge TTL, so a
    /// failed purge only costs a few seconds of staleness. Never retried.
    pub fn purge(&self, sink: &SinkConfig) -> Result<(), String> {
        if sink.purge_token.trim().is_empty() || sink.zone_id.trim().is_empty() {
            return Ok(());
        }
        if sink.public_url.trim().is_empty() {
            return Ok(());
        }

        let url = format!(
            "https://api.cloudflare.com/client/v4/zones/{}/purge_cache",
            sink.zone_id.trim()
        );
        let body = serde_json::json!({ "files": [sink.public_url.trim()] }).to_string();

        match self
            .http
            .post(&url)
            .header(
                "authorization",
                format!("Bearer {}", sink.purge_token.trim()),
            )
            .header("content-type", "application/json")
            .send(&body)
        {
            Ok(res) if res.status().is_success() => Ok(()),
            Ok(res) => Err(format!("purge HTTP {}", res.status().as_u16())),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Split `https://host/optional/base` into the connect host and the path prefix.
fn split_endpoint(endpoint: &str) -> Option<(String, String)> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))?;
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{}", p.trim_matches('/'))),
        None => (rest, String::new()),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_splits_into_host_and_prefix() {
        assert_eq!(
            split_endpoint("https://acc.r2.cloudflarestorage.com"),
            Some(("acc.r2.cloudflarestorage.com".into(), String::new()))
        );
        assert_eq!(
            split_endpoint("https://acc.r2.cloudflarestorage.com/"),
            Some(("acc.r2.cloudflarestorage.com".into(), String::new()))
        );
        assert_eq!(
            split_endpoint("https://acc.r2.cloudflarestorage.com/prefix"),
            Some(("acc.r2.cloudflarestorage.com".into(), "/prefix".into()))
        );
        assert_eq!(split_endpoint("ftp://nope"), None);
        assert_eq!(split_endpoint(""), None);
    }

    #[test]
    fn dns_failures_are_told_apart_from_generic_ones() {
        assert!(looks_like_dns_failure(
            "io: 不知道这样的主机。 (os error 11001)"
        ));
        assert!(looks_like_dns_failure(
            "failed to lookup address information"
        ));
        assert!(!looks_like_dns_failure("timeout: connect"));
        assert!(!looks_like_dns_failure("HTTP 503"));
    }

    #[test]
    fn path_is_bucket_plus_key() {
        let sink = SinkConfig {
            endpoint: "https://acc.r2.cloudflarestorage.com".into(),
            bucket: "now-doing".into(),
            object_key: "presence.json".into(),
            ..SinkConfig::default()
        };
        let (host, base) = split_endpoint(&sink.endpoint).unwrap();
        assert_eq!(host, "acc.r2.cloudflarestorage.com");
        let path = format!("{base}/{}/{}", sink.bucket, sink.object_key);
        assert_eq!(path, "/now-doing/presence.json");
    }
}
