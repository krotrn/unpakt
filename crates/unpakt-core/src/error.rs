use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("refused: {0}")]
    Refused(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error(
        "conflict: `{command}` would be shadowed by {}; use --force or choose another command",
        by.display()
    )]
    Shadowed { command: String, by: PathBuf },
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("http error: {0}")]
    Http(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("interrupted")]
    Interrupted,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Attaches the path an I/O operation was applied to.
pub trait IoResultExt<T> {
    fn at(self, path: impl AsRef<Path>) -> Result<T>;
}

impl<T> IoResultExt<T> for std::io::Result<T> {
    fn at(self, path: impl AsRef<Path>) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.as_ref().to_path_buf(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_attaches_path() {
        let err = std::fs::read("/definitely/not/here")
            .at("/definitely/not/here")
            .unwrap_err();
        assert!(
            matches!(err, Error::Io { ref path, .. } if path == Path::new("/definitely/not/here"))
        );
        assert!(
            err.to_string()
                .starts_with("io error at /definitely/not/here")
        );
    }
}
