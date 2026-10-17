use super::SourcePlugin;
use crate::error::{Error, Result};
use crate::http::Http;
use crate::model::{Asset, Release, SourceCfg, SourceKind};
use crate::naming::parse_file_name;

pub struct UrlSource;

fn parse(input: &str) -> Option<::url::Url> {
    ::url::Url::parse(input)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
}

impl SourcePlugin for UrlSource {
    fn kind(&self) -> SourceKind {
        SourceKind::DirectUrl
    }

    fn matches(&self, input: &str) -> bool {
        parse(input).is_some()
    }

    fn cfg(&self, input: &str) -> SourceCfg {
        SourceCfg {
            kind: SourceKind::DirectUrl,
            location: input.to_string(),
            pattern: None,
        }
    }

    fn latest(&self, cfg: &SourceCfg, http: &Http) -> Result<Release> {
        let url = parse(&cfg.location)
            .ok_or_else(|| Error::Parse(format!("{} is not an http(s) URL", cfg.location)))?;
        let name = url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .unwrap_or("")
            .to_string();
        let version = parse_file_name(&name).1;
        let marker = http.head_marker(&cfg.location)?;
        Ok(Release {
            version,
            marker,
            assets: vec![Asset {
                name,
                url: cfg.location.clone(),
                size: None,
            }],
        })
    }
}
