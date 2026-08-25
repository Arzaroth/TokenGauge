//! The object-store transport: any S3-compatible bucket.
//!
//! SigV4 is signed here rather than pulled in with `aws-sdk-s3`, which would
//! drag tokio and dozens of crates into a binary with no async runtime in order
//! to sign three verbs. `reqwest::blocking` is already linked for the provider
//! fetchers, and `hmac` pairs with the `sha2` already used for the device id.
//!
//! Tested against S3, R2, B2's S3 endpoint, MinIO and Garage by using
//! path-style addressing, which all of them accept.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::transport::{PeerEntry, Transport, is_object_name};
use crate::SyncS3Config;

const SERVICE: &str = "s3";
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub struct S3Transport {
    endpoint: String,
    bucket: String,
    prefix: String,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    client: reqwest::blocking::Client,
}

impl std::fmt::Debug for S3Transport {
    /// Hand-written so credentials cannot reach a log through a debug dump.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Transport")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

impl S3Transport {
    pub fn new(config: &SyncS3Config, timeout: Duration) -> Result<Self> {
        let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        let access_key_id = pick(&config.access_key_id, env("AWS_ACCESS_KEY_ID"))
            .context("no S3 access key; set [sync.s3] access_key_id or AWS_ACCESS_KEY_ID")?;
        let secret_access_key = pick(&config.secret_access_key, env("AWS_SECRET_ACCESS_KEY"))
            .context(
                "no S3 secret key; set [sync.s3] secret_access_key or AWS_SECRET_ACCESS_KEY",
            )?;
        if config.endpoint.trim().is_empty() {
            bail!("[sync.s3] endpoint is not set");
        }
        if config.bucket.trim().is_empty() {
            bail!("[sync.s3] bucket is not set");
        }

        Ok(Self {
            endpoint: config.endpoint.trim().trim_end_matches('/').to_string(),
            bucket: config.bucket.trim().to_string(),
            prefix: normalise_prefix(&config.prefix),
            region: pick(&config.region, env("AWS_REGION")).unwrap_or_else(|| "auto".to_string()),
            access_key_id,
            secret_access_key,
            session_token: env("AWS_SESSION_TOKEN"),
            client: reqwest::blocking::Client::builder()
                .timeout(timeout)
                .build()
                .context("could not build an HTTP client for the S3 transport")?,
        })
    }

    fn key_for(&self, name: &str) -> String {
        format!("{}v1/{name}", self.prefix)
    }

    fn host(&self) -> Result<String> {
        let url = reqwest::Url::parse(&self.endpoint)
            .with_context(|| format!("[sync.s3] endpoint is not a URL: {}", self.endpoint))?;
        let host = url.host_str().context("[sync.s3] endpoint has no host")?;
        Ok(match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        })
    }

    /// Path-style: every S3-compatible implementation accepts it, and
    /// virtual-host style needs per-provider DNS rules.
    fn path_for(&self, key: Option<&str>) -> String {
        match key {
            Some(key) => format!("/{}/{}", self.bucket, key),
            None => format!("/{}", self.bucket),
        }
    }

    fn signed(
        &self,
        method: &str,
        path: &str,
        query: &str,
        payload_sha256: &str,
        now: &str,
    ) -> Result<BTreeMap<String, String>> {
        let mut headers = BTreeMap::new();
        headers.insert("host".to_string(), self.host()?);
        headers.insert(
            "x-amz-content-sha256".to_string(),
            payload_sha256.to_string(),
        );
        headers.insert("x-amz-date".to_string(), now.to_string());
        if let Some(token) = &self.session_token {
            headers.insert("x-amz-security-token".to_string(), token.clone());
        }

        let authorization = authorization(
            &Credentials {
                access_key_id: &self.access_key_id,
                secret_access_key: &self.secret_access_key,
                region: &self.region,
            },
            method,
            path,
            query,
            &headers,
            payload_sha256,
            now,
        );
        headers.insert("authorization".to_string(), authorization);
        Ok(headers)
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &str,
        payload: Option<&[u8]>,
        extra: &[(&str, String)],
    ) -> Result<reqwest::blocking::Response> {
        let now = timestamp();
        let payload_sha256 = match payload {
            Some(body) => hex(&Sha256::digest(body)),
            None => EMPTY_SHA256.to_string(),
        };
        let headers = self.signed(method.as_str(), path, query, &payload_sha256, &now)?;

        let url = if query.is_empty() {
            format!("{}{path}", self.endpoint)
        } else {
            format!("{}{path}?{query}", self.endpoint)
        };
        let mut request = self.client.request(method, &url);
        for (name, value) in &headers {
            // reqwest sets Host itself, and signing it twice is a mismatch.
            if name != "host" {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        for (name, value) in extra {
            request = request.header(*name, value.as_str());
        }
        if let Some(body) = payload {
            request = request.body(body.to_vec());
        }
        request.send().context("the S3 request did not complete")
    }
}

fn pick(configured: &str, from_env: Option<String>) -> Option<String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    from_env
}

fn normalise_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

impl Transport for S3Transport {
    fn describe(&self) -> String {
        format!("s3:{}/{}/{}v1", self.endpoint, self.bucket, self.prefix)
    }

    fn put(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path_for(Some(&self.key_for(name)));
        let response = self.request(reqwest::Method::PUT, &path, "", Some(bytes), &[])?;
        expect_ok(response, "write").map(|_| ())
    }

    fn list(&self) -> Result<Vec<PeerEntry>> {
        let mut found = Vec::new();
        let mut token: Option<String> = None;
        let path = self.path_for(None);

        loop {
            let mut params = vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), format!("{}v1/", self.prefix)),
            ];
            if let Some(token) = &token {
                params.push(("continuation-token".to_string(), token.clone()));
            }
            params.sort();
            let query = params
                .iter()
                .map(|(k, v)| format!("{}={}", uri_encode(k, true), uri_encode(v, true)))
                .collect::<Vec<_>>()
                .join("&");

            let response = self.request(reqwest::Method::GET, &path, &query, None, &[])?;
            let body = expect_ok(response, "list")?
                .text()
                .context("the bucket listing was not readable")?;
            let (entries, next) = parse_listing(&body);
            found.extend(entries);
            match next {
                Some(next) => token = Some(next),
                None => break,
            }
        }

        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found)
    }

    fn get(&self, entry: &PeerEntry, known_version: Option<&str>) -> Result<Option<Vec<u8>>> {
        let path = self.path_for(Some(&self.key_for(&entry.name)));
        let extra: Vec<(&str, String)> = known_version
            .map(|version| vec![("if-none-match", version.to_string())])
            .unwrap_or_default();
        let response = self.request(reqwest::Method::GET, &path, "", None, &extra)?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        let body = expect_ok(response, "read")?
            .bytes()
            .context("the object body was not readable")?;
        Ok(Some(body.to_vec()))
    }

    fn delete(&self, name: &str) -> Result<()> {
        let path = self.path_for(Some(&self.key_for(name)));
        let response = self.request(reqwest::Method::DELETE, &path, "", None, &[])?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        expect_ok(response, "delete").map(|_| ())
    }
}

fn expect_ok(
    response: reqwest::blocking::Response,
    what: &str,
) -> Result<reqwest::blocking::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    // The body carries S3's own <Code> and <Message>, which say far more than
    // the status alone - AccessDenied against NoSuchBucket, for one.
    let detail = response.text().unwrap_or_default();
    let code = between(&detail, "<Code>", "</Code>").unwrap_or_default();
    let message = between(&detail, "<Message>", "</Message>").unwrap_or_default();
    if code.is_empty() {
        bail!("could not {what}: HTTP {status}");
    }
    bail!("could not {what}: {code} ({status}) {message}");
}

/// `ListObjectsV2` is a fixed, well-known shape, so it is scanned rather than
/// parsed with an XML crate. Anything that is not one of our object names is
/// ignored, which is also what keeps a shared bucket usable.
fn parse_listing(xml: &str) -> (Vec<PeerEntry>, Option<String>) {
    let mut entries = Vec::new();
    for chunk in xml.split("<Contents>").skip(1) {
        let Some(block) = chunk.split("</Contents>").next() else {
            continue;
        };
        let Some(key) = between(block, "<Key>", "</Key>") else {
            continue;
        };
        let name = key.rsplit('/').next().unwrap_or(&key).to_string();
        if !is_object_name(&name) {
            continue;
        }
        entries.push(PeerEntry {
            name,
            version: between(block, "<ETag>", "</ETag>")
                .map(|tag| tag.replace("&quot;", "\"").trim_matches('"').to_string())
                .unwrap_or_default(),
            size: between(block, "<Size>", "</Size>")
                .and_then(|size| size.parse().ok())
                .unwrap_or(0),
        });
    }

    let truncated = between(xml, "<IsTruncated>", "</IsTruncated>")
        .map(|value| value.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let next = truncated
        .then(|| between(xml, "<NextContinuationToken>", "</NextContinuationToken>"))
        .flatten();
    (entries, next)
}

fn between(text: &str, open: &str, close: &str) -> Option<String> {
    let start = text.find(open)? + open.len();
    let rest = &text[start..];
    let end = rest.find(close)?;
    Some(rest[..end].to_string())
}

// ---------------------------------------------------------------------------
// SigV4
// ---------------------------------------------------------------------------

pub(crate) struct Credentials<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub region: &'a str,
}

fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

pub(crate) fn canonical_request(
    method: &str,
    path: &str,
    query: &str,
    headers: &BTreeMap<String, String>,
    payload_sha256: &str,
) -> String {
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{}:{}\n", name.to_lowercase(), value.trim()))
        .collect();
    let signed_headers = headers
        .keys()
        .map(|name| name.to_lowercase())
        .collect::<Vec<_>>()
        .join(";");
    format!(
        "{method}\n{}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_sha256}",
        uri_encode(path, false)
    )
}

pub(crate) fn authorization(
    credentials: &Credentials<'_>,
    method: &str,
    path: &str,
    query: &str,
    headers: &BTreeMap<String, String>,
    payload_sha256: &str,
    now: &str,
) -> String {
    let date = &now[..8];
    let scope = format!("{date}/{}/{SERVICE}/aws4_request", credentials.region);
    let canonical = canonical_request(method, path, query, headers, payload_sha256);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{now}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );

    let signing_key = signing_key(credentials.secret_access_key, date, credentials.region);
    let signature = hex(&hmac(&signing_key, to_sign.as_bytes()));
    let signed_headers = headers
        .keys()
        .map(|name| name.to_lowercase())
        .collect::<Vec<_>>()
        .join(";");

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    )
}

pub(crate) fn signing_key(secret: &str, date: &str, region: &str) -> Vec<u8> {
    let mut key = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    key = hmac(&key, region.as_bytes());
    key = hmac(&key, SERVICE.as_bytes());
    hmac(&key, b"aws4_request")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac takes any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// `encode_slash` is false for a path (segments are encoded, separators are
/// not) and true for a query value.
pub(crate) fn uri_encode(text: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's own worked example for a GET with a Range header. If this matches,
    /// the canonical request, the string to sign and the key derivation are all
    /// right at once.
    #[test]
    fn signing_matches_the_published_aws_example() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "host".to_string(),
            "examplebucket.s3.amazonaws.com".to_string(),
        );
        headers.insert("range".to_string(), "bytes=0-9".to_string());
        headers.insert("x-amz-content-sha256".to_string(), EMPTY_SHA256.to_string());
        headers.insert("x-amz-date".to_string(), "20130524T000000Z".to_string());

        let authorization = authorization(
            &Credentials {
                access_key_id: "AKIAIOSFODNN7EXAMPLE",
                secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                region: "us-east-1",
            },
            "GET",
            "/test.txt",
            "",
            &headers,
            EMPTY_SHA256,
            "20130524T000000Z",
        );

        assert!(
            authorization.ends_with(
                "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            ),
            "{authorization}"
        );
        assert!(
            authorization
                .contains("Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request")
        );
        assert!(authorization.contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date"));
    }

    #[test]
    fn the_canonical_request_has_the_shape_the_spec_asks_for() {
        let mut headers = BTreeMap::new();
        headers.insert("host".to_string(), "example.com".to_string());
        headers.insert("x-amz-date".to_string(), "20260825T120000Z".to_string());

        assert_eq!(
            canonical_request("PUT", "/bucket/v1/a b", "list-type=2", &headers, "HASH"),
            "PUT\n/bucket/v1/a%20b\nlist-type=2\nhost:example.com\nx-amz-date:20260825T120000Z\n\nhost;x-amz-date\nHASH"
        );
    }

    #[test]
    fn a_path_keeps_its_separators_and_a_query_value_does_not() {
        assert_eq!(
            uri_encode("/bucket/v1/x.tgsync", false),
            "/bucket/v1/x.tgsync"
        );
        assert_eq!(uri_encode("fleet/v1/", true), "fleet%2Fv1%2F");
        assert_eq!(uri_encode("a+b c", true), "a%2Bb%20c");
    }

    #[test]
    fn a_prefix_is_normalised_however_it_was_written() {
        assert_eq!(normalise_prefix(""), "");
        assert_eq!(normalise_prefix("  "), "");
        assert_eq!(normalise_prefix("fleet"), "fleet/");
        assert_eq!(normalise_prefix("/fleet/"), "fleet/");
    }

    #[test]
    fn a_listing_yields_our_objects_and_ignores_everything_else() {
        let name = "0123456789abcdef0123456789abcdef.tgsync";
        let xml = format!(
            r#"<?xml version="1.0"?><ListBucketResult>
              <IsTruncated>true</IsTruncated>
              <NextContinuationToken>token-2</NextContinuationToken>
              <Contents><Key>fleet/v1/{name}</Key><ETag>&quot;abc123&quot;</ETag><Size>2243</Size></Contents>
              <Contents><Key>fleet/v1/notes.txt</Key><ETag>&quot;zzz&quot;</ETag><Size>10</Size></Contents>
            </ListBucketResult>"#
        );

        let (entries, next) = parse_listing(&xml);
        assert_eq!(entries.len(), 1, "somebody else's file must be ignored");
        assert_eq!(entries[0].name, name);
        assert_eq!(entries[0].version, "abc123");
        assert_eq!(entries[0].size, 2243);
        assert_eq!(next.as_deref(), Some("token-2"));
    }

    #[test]
    fn a_finished_listing_asks_for_no_more_pages() {
        let xml = "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>";
        let (entries, next) = parse_listing(xml);
        assert!(entries.is_empty());
        assert_eq!(next, None);
    }

    #[test]
    fn s3s_own_error_code_reaches_the_user() {
        let body = "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>";
        assert_eq!(
            between(body, "<Code>", "</Code>").as_deref(),
            Some("AccessDenied")
        );
    }

    #[test]
    fn credentials_come_from_the_config_before_the_environment() {
        let configured = SyncS3Config {
            endpoint: "https://example.r2.cloudflarestorage.com".into(),
            region: "auto".into(),
            bucket: "tokengauge".into(),
            prefix: "fleet".into(),
            access_key_id: "from-config".into(),
            secret_access_key: "shhh".into(),
        };
        let transport = S3Transport::new(&configured, Duration::from_secs(5)).expect("build");

        assert_eq!(transport.access_key_id, "from-config");
        assert_eq!(transport.key_for("x.tgsync"), "fleet/v1/x.tgsync");
        assert_eq!(
            transport.path_for(Some("fleet/v1/x")),
            "/tokengauge/fleet/v1/x"
        );
        assert_eq!(
            transport.host().expect("host"),
            "example.r2.cloudflarestorage.com"
        );
        assert!(transport.describe().starts_with("s3:"));
    }

    #[test]
    fn an_endpoint_or_bucket_that_is_missing_says_which() {
        let mut config = SyncS3Config {
            access_key_id: "k".into(),
            secret_access_key: "s".into(),
            ..Default::default()
        };
        let error = S3Transport::new(&config, Duration::from_secs(5)).unwrap_err();
        assert!(format!("{error}").contains("endpoint"), "{error}");

        config.endpoint = "https://example.com".into();
        let error = S3Transport::new(&config, Duration::from_secs(5)).unwrap_err();
        assert!(format!("{error}").contains("bucket"), "{error}");
    }
}
