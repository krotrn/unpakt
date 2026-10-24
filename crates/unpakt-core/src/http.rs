//! Blocking HTTP client: JSON from the release API and streamed downloads.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use percent_encoding::percent_decode_str;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, ETAG, HeaderValue, LAST_MODIFIED};
use sha2::{Digest, Sha256};
use url::Url;

use crate::cancel::Cancel;
use crate::error::{Error, IoResultExt, Result};

const GITHUB_API: &str = "https://api.github.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for one whole request, body included (the blocking client has no per-read timeout).
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct Http {
    client: Client,
    github_api: String,
    token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Download {
    pub path: PathBuf,
    pub sha256: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

fn http_err(url: &str, e: impl std::fmt::Display) -> Error {
    Error::Http(format!("{url}: {e}"))
}

fn header(resp: &Response, name: reqwest::header::HeaderName) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// The decoded last path segment of `url`, if it is safe to use as a file name.
fn file_name_of(url: &Url) -> Result<String> {
    let raw = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or("");
    let name = percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| Error::Refused(format!("{url}: file name is not valid UTF-8")))?;
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(Error::Refused(format!(
            "{url}: unsafe or missing file name `{name}`"
        )));
    }
    Ok(name.into_owned())
}

/// The `Authorization` value for `token`.
fn bearer(token: &str) -> Result<HeaderValue> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| Error::Http("the token is not a valid header value".into()))?;
    value.set_sensitive(true);
    Ok(value)
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

impl Http {
    /// Builds a client; no request is made. The token, if any, is sent only
    /// to the host and port of `github_api`. Cross-host redirects are
    /// followed by reqwest, which drops the `Authorization` header when the
    /// host changes.
    pub fn new(github_api: &str, token: Option<String>) -> Result<Http> {
        let client = Client::builder()
            .user_agent(concat!("unpakt/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .build()
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(Http {
            client,
            github_api: github_api.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// The default API, with the token taken from `GITHUB_TOKEN`, else `GH_TOKEN`.
    pub fn from_env() -> Result<Http> {
        let token = ["GITHUB_TOKEN", "GH_TOKEN"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .find(|v| !v.is_empty());
        Http::new(GITHUB_API, token)
    }

    pub fn github_api(&self) -> &str {
        &self.github_api
    }

    pub fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let parsed = Url::parse(url).map_err(|e| http_err(url, e))?;
        let mut req = self
            .client
            .get(parsed.clone())
            .header("Accept", "application/vnd.github+json");
        if let (Some(token), Ok(api)) = (&self.token, Url::parse(&self.github_api))
            && same_origin(&parsed, &api)
        {
            req = req.header(AUTHORIZATION, bearer(token)?);
        }
        let resp = req.send().map_err(|e| http_err(url, e))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(http_err(url, format!("status {status}")));
        }
        resp.json().map_err(|e| http_err(url, e))
    }

    /// The ETag of `url`, else its Last-Modified, from a HEAD request. A
    /// non-success status (many hosts reject HEAD) yields `None`.
    pub fn head_marker(&self, url: &str) -> Result<Option<String>> {
        let resp = self.client.head(url).send().map_err(|e| http_err(url, e))?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        Ok(header(&resp, ETAG).or_else(|| header(&resp, LAST_MODIFIED)))
    }

    /// Streams `url` into `dest_dir/<last path segment>`, replacing any file
    /// already there. The token is never sent.
    pub fn download(&self, url: &str, dest_dir: &Path) -> Result<Download> {
        self.download_cancellable(url, dest_dir, &Cancel::new())
    }

    /// Like [`Http::download`], stopping with `Interrupted` once `cancel` is set.
    pub fn download_cancellable(
        &self,
        url: &str,
        dest_dir: &Path,
        cancel: &Cancel,
    ) -> Result<Download> {
        cancel.check()?;
        let parsed = Url::parse(url).map_err(|e| http_err(url, e))?;
        let name = file_name_of(&parsed)?;
        let mut resp = self
            .client
            .get(parsed)
            .send()
            .map_err(|e| http_err(url, e))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(http_err(url, format!("status {status}")));
        }
        let etag = header(&resp, ETAG);
        let last_modified = header(&resp, LAST_MODIFIED);
        let expected = resp.content_length();

        std::fs::create_dir_all(dest_dir).at(dest_dir)?;
        // Removed on drop unless persisted, so every error path cleans up.
        let mut part = tempfile::Builder::new()
            .prefix(".part-")
            .tempfile_in(dest_dir)
            .at(dest_dir)?;
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        let mut buf = [0u8; 64 * 1024];
        loop {
            cancel.check()?;
            let n = resp.read(&mut buf).map_err(|e| http_err(url, e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            part.write_all(&buf[..n]).at(part.path())?;
            total += n as u64;
        }
        if let Some(expected) = expected
            && expected != total
        {
            return Err(http_err(
                url,
                format!("incomplete download: got {total} of {expected} bytes"),
            ));
        }
        part.flush().at(part.path())?;

        let path = dest_dir.join(name);
        part.persist(&path).map_err(|e| Error::Io {
            path: path.clone(),
            source: e.error,
        })?;
        Ok(Download {
            path,
            sha256: hex::encode(hasher.finalize()),
            etag,
            last_modified,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_header_is_sensitive() {
        let value = bearer("tok").unwrap();
        assert!(value.is_sensitive());
        assert_eq!(value.as_bytes(), b"Bearer tok");
        assert!(bearer("bad\nvalue").is_err());
    }

    #[test]
    fn same_origin_compares_host_and_port() {
        let a = Url::parse("http://127.0.0.1:1/x").unwrap();
        assert!(same_origin(
            &a,
            &Url::parse("http://127.0.0.1:1/y").unwrap()
        ));
        assert!(!same_origin(
            &a,
            &Url::parse("http://127.0.0.1:2/y").unwrap()
        ));
        assert!(!same_origin(
            &a,
            &Url::parse("http://localhost:1/y").unwrap()
        ));
    }
}
