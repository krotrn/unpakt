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

/// `(owner, repo)` of `https://github.com/<owner>/<repo>[.git][/...]`.
fn parse(input: &str) -> Option<(String, String)> {
    let url = ::url::Url::parse(input).ok()?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") {
        return None;
    }
    let mut segments = url.path_segments()?;
    let owner = segments.next().filter(|s| !s.is_empty())?;
    let repo = segments.next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
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
