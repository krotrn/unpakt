//! Where packages come from: a local file, a direct URL or a GitHub repo.

mod github;
mod local;
mod url;

use crate::error::{Error, Result};
use crate::http::Http;
use crate::model::{Release, SourceCfg, SourceKind};

pub trait SourcePlugin: Sync {
    fn kind(&self) -> SourceKind;
    fn matches(&self, input: &str) -> bool;
    fn cfg(&self, input: &str) -> SourceCfg;
    fn latest(&self, cfg: &SourceCfg, http: &Http) -> Result<Release>;
}

static GITHUB: github::GithubSource = github::GithubSource;
static DIRECT_URL: url::UrlSource = url::UrlSource;
static LOCAL_FILE: local::LocalSource = local::LocalSource;

/// Picks the source for `input`, trying GitHub, then direct URL, then local file.
pub fn resolve(input: &str) -> Result<(&'static dyn SourcePlugin, SourceCfg)> {
    github::check_supported(input)?;
    let plugins: [&'static dyn SourcePlugin; 3] = [&GITHUB, &DIRECT_URL, &LOCAL_FILE];
    plugins
        .into_iter()
        .find(|p| p.matches(input))
        .map(|p| (p, p.cfg(input)))
        .ok_or_else(|| {
            Error::NotFound(format!(
                "{input} is not an existing file, a GitHub repo or an http(s) URL"
            ))
        })
}

pub fn for_kind(kind: SourceKind) -> &'static dyn SourcePlugin {
    match kind {
        SourceKind::Github => &GITHUB,
        SourceKind::DirectUrl => &DIRECT_URL,
        SourceKind::LocalFile => &LOCAL_FILE,
    }
}
