use std::path::Path;

use super::SourcePlugin;
use crate::error::{IoResultExt, Result};
use crate::http::Http;
use crate::model::{Asset, Release, SourceCfg, SourceKind};

pub struct LocalSource;

impl SourcePlugin for LocalSource {
    fn kind(&self) -> SourceKind {
        SourceKind::LocalFile
    }

    fn matches(&self, input: &str) -> bool {
        Path::new(input).is_file()
    }

    fn cfg(&self, input: &str) -> SourceCfg {
        let path = Path::new(input);
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        SourceCfg {
            kind: SourceKind::LocalFile,
            location: canonical.to_string_lossy().into_owned(),
            pattern: None,
        }
    }

    fn latest(&self, cfg: &SourceCfg, _http: &Http) -> Result<Release> {
        let path = Path::new(&cfg.location);
        let size = std::fs::metadata(path).at(path)?.len();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Release {
            version: None,
            marker: None,
            assets: vec![Asset {
                name,
                url: cfg.location.clone(),
                size: Some(size),
            }],
        })
    }
}
