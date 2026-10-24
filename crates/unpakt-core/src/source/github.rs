use serde::Deserialize;

use super::SourcePlugin;
use crate::error::{Error, Result};
use crate::http::Http;
use crate::model::{Asset, Release, SourceCfg, SourceKind};

pub struct GithubSource;

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    size: Option<u64>,
}

enum Shape {
    NotGithub,
    /// `https://github.com/<owner>/<repo>[.git][/]`.
    Repo(String, String),
    /// A release asset link, which is a plain download.
    AssetDownload,
    /// Any other github.com page.
    Unsupported,
}

fn classify(input: &str) -> Shape {
    let Ok(url) = ::url::Url::parse(input) else {
        return Shape::NotGithub;
    };
    let github_host = matches!(url.host_str(), Some("github.com" | "www.github.com"));
    if !matches!(url.scheme(), "http" | "https") || !github_host {
        return Shape::NotGithub;
    }
    let segments: Vec<&str> = url.path_segments().map_or(Vec::new(), |s| s.collect());
    let (owner, repo) = match segments.as_slice() {
        [owner, repo, ..] if !owner.is_empty() => {
            (*owner, repo.strip_suffix(".git").unwrap_or(repo))
        }
        _ => return Shape::Unsupported,
    };
    if repo.is_empty() {
        return Shape::Unsupported;
    }
    match &segments[2..] {
        [] | [""] => Shape::Repo(owner.to_string(), repo.to_string()),
        ["releases", "download", ..] => Shape::AssetDownload,
        _ => Shape::Unsupported,
    }
}

/// Fails for github.com pages that are neither a repo nor an asset download.
pub(super) fn check_supported(input: &str) -> Result<()> {
    match classify(input) {
        Shape::Unsupported => Err(Error::Refused(
            "unsupported GitHub URL; use https://github.com/<owner>/<repo> to track the latest release"
                .into(),
        )),
        _ => Ok(()),
    }
}

fn parse(input: &str) -> Option<(String, String)> {
    match classify(input) {
        Shape::Repo(owner, repo) => Some((owner, repo)),
        _ => None,
    }
}

impl SourcePlugin for GithubSource {
    fn kind(&self) -> SourceKind {
        SourceKind::Github
    }

    fn matches(&self, input: &str) -> bool {
        parse(input).is_some()
    }

    fn cfg(&self, input: &str) -> SourceCfg {
        let location = match parse(input) {
            Some((owner, repo)) => format!("https://github.com/{owner}/{repo}"),
            None => input.to_string(),
        };
        SourceCfg {
            kind: SourceKind::Github,
            location,
            pattern: None,
        }
    }

    fn latest(&self, cfg: &SourceCfg, http: &Http) -> Result<Release> {
        let (owner, repo) = parse(&cfg.location)
            .ok_or_else(|| Error::Parse(format!("{} is not a GitHub repo", cfg.location)))?;
        let url = format!("{}/repos/{owner}/{repo}/releases/latest", http.github_api());
        let release: ApiRelease = http.get_json(&url)?;
        Ok(Release {
            marker: Some(release.tag_name.clone()),
            version: Some(release.tag_name),
            assets: release
                .assets
                .into_iter()
                .map(|a| Asset {
                    name: a.name,
                    url: a.browser_download_url,
                    size: a.size,
                })
                .collect(),
        })
    }
}
