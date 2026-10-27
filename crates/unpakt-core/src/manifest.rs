use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::{Error, IoResultExt, Result};
use crate::model::{ExecKind, FormatKind, SourceCfg, SourceKind};

const SCHEMA_VERSION: i64 = 1;

const SCHEMA_V1: &str = "
CREATE TABLE apps (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    format TEXT NOT NULL,
    backend TEXT NOT NULL,
    source_kind TEXT NOT NULL,
    source_location TEXT NOT NULL,
    source_pattern TEXT,
    current TEXT NOT NULL,
    desktop INTEGER NOT NULL,
    command TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE versions (
    app_id TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    version TEXT,
    marker TEXT,
    entry TEXT NOT NULL,
    exec_kind TEXT NOT NULL,
    seq INTEGER NOT NULL,
    installed_at INTEGER NOT NULL,
    PRIMARY KEY (app_id, label)
);
CREATE TABLE options (
    app_id TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (app_id, key)
);
CREATE TABLE files (
    app_id TEXT NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    PRIMARY KEY (app_id, path)
);
";

const APP_COLUMNS: &str = "id, name, format, backend, source_kind, source_location, \
                           source_pattern, current, desktop, command";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRecord {
    pub id: String,
    pub name: String,
    pub format: FormatKind,
    pub backend: String,
    pub source: SourceCfg,
    pub current: String,
    pub desktop: bool,
    pub command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRecord {
    pub label: String,
    pub version: Option<String>,
    pub marker: Option<String>,
    pub entry: PathBuf,
    pub exec_kind: ExecKind,
    pub installed_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Desktop,
    Icon,
    Bin,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Desktop => "desktop",
            FileKind::Icon => "icon",
            FileKind::Bin => "bin",
        }
    }
}

impl FromStr for FileKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "desktop" => Ok(FileKind::Desktop),
            "icon" => Ok(FileKind::Icon),
            "bin" => Ok(FileKind::Bin),
            other => Err(Error::Parse(format!("unknown file kind: {other}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub path: PathBuf,
    pub kind: FileKind,
}

pub struct Manifest {
    conn: Connection,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::Parse(format!("path is not valid UTF-8: {}", path.display())))
}

fn parse_col<T: FromStr<Err = Error>>(row: &Row, idx: usize) -> rusqlite::Result<T> {
    let text: String = row.get(idx)?;
    text.parse().map_err(|e: Error| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn app_from_row(row: &Row) -> rusqlite::Result<AppRecord> {
    Ok(AppRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        format: parse_col(row, 2)?,
        backend: row.get(3)?,
        source: SourceCfg {
            kind: parse_col::<SourceKind>(row, 4)?,
            location: row.get(5)?,
            pattern: row.get(6)?,
        },
        current: row.get(7)?,
        desktop: row.get::<_, i64>(8)? != 0,
        command: row.get(9)?,
    })
}

fn version_from_row(row: &Row) -> rusqlite::Result<VersionRecord> {
    Ok(VersionRecord {
        label: row.get(0)?,
        version: row.get(1)?,
        marker: row.get(2)?,
        entry: PathBuf::from(row.get::<_, String>(3)?),
        exec_kind: parse_col(row, 4)?,
        installed_at: row.get(5)?,
    })
}

fn replace_files(tx: &rusqlite::Transaction, id: &str, files: &[FileRecord]) -> Result<()> {
    tx.execute("DELETE FROM files WHERE app_id = ?1", [id])?;
    let mut insert =
        tx.prepare("INSERT OR REPLACE INTO files (app_id, path, kind) VALUES (?1, ?2, ?3)")?;
    for f in files {
        insert.execute(params![id, path_text(&f.path)?, f.kind.as_str()])?;
    }
    Ok(())
}

impl Manifest {
    pub fn open(path: &Path) -> Result<Manifest> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).at(parent)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Manifest> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Manifest> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        if conn.path().is_some_and(|p| !p.is_empty()) {
            // Readers keep working while another process writes.
            conn.pragma_update(None, "journal_mode", "WAL")?;
        }
        conn.pragma_update(None, "foreign_keys", true)?;
        let found: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(Error::Unsupported(format!(
                "manifest schema version {found} is newer than this build supports"
            )));
        }
        if found < 1 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(SCHEMA_V1)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        }
        Ok(Manifest { conn })
    }

    /// Records an install atomically: upserts the app, appends the version,
    /// replaces the app's file set and makes the new version current.
    pub fn commit_install(
        &mut self,
        app: &AppRecord,
        ver: &VersionRecord,
        files: &[FileRecord],
    ) -> Result<()> {
        self.commit_install_with(app, ver, files, &[], &[])
    }

    /// Like `commit_install`, and in the same transaction also stores `options`
    /// and drops the version rows labelled in `prune`.
    pub fn commit_install_with(
        &mut self,
        app: &AppRecord,
        ver: &VersionRecord,
        files: &[FileRecord],
        options: &[(&str, &str)],
        prune: &[&str],
    ) -> Result<()> {
        let ts = now();
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO apps (id, name, format, backend, source_kind, source_location, \
                 source_pattern, current, desktop, command, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11) \
             ON CONFLICT(id) DO UPDATE SET \
                 name = excluded.name, format = excluded.format, backend = excluded.backend, \
                 source_kind = excluded.source_kind, source_location = excluded.source_location, \
                 source_pattern = excluded.source_pattern, current = excluded.current, \
                 desktop = excluded.desktop, command = excluded.command, \
                 updated_at = excluded.updated_at",
            params![
                app.id,
                app.name,
                app.format.as_str(),
                app.backend,
                app.source.kind.as_str(),
                app.source.location,
                app.source.pattern,
                ver.label,
                app.desktop,
                app.command,
                ts,
            ],
        )?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM versions WHERE app_id = ?1",
            [&app.id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO versions \
                 (app_id, label, version, marker, entry, exec_kind, seq, installed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                app.id,
                ver.label,
                ver.version,
                ver.marker,
                path_text(&ver.entry)?,
                ver.exec_kind.as_str(),
                seq,
                ver.installed_at,
            ],
        )?;
        replace_files(&tx, &app.id, files)?;
        for (key, value) in options {
            tx.execute(
                "INSERT INTO options (app_id, key, value) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(app_id, key) DO UPDATE SET value = excluded.value",
                params![app.id, key, value],
            )?;
        }
        for label in prune {
            tx.execute(
                "DELETE FROM versions WHERE app_id = ?1 AND label = ?2",
                params![app.id, label],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Records a system-wide version in one transaction: the app row, the
    /// version (replacing any row with the same label), the options, and the
    /// removal of versions beyond `KEEP_VERSIONS`, never the new one.
    pub fn commit_native_version(
        &mut self,
        app: &AppRecord,
        ver: &VersionRecord,
        options: &[(&str, &str)],
    ) -> Result<()> {
        let ts = now();
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO apps (id, name, format, backend, source_kind, source_location, \
                 source_pattern, current, desktop, command, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11) \
             ON CONFLICT(id) DO UPDATE SET \
                 name = excluded.name, format = excluded.format, backend = excluded.backend, \
                 source_kind = excluded.source_kind, source_location = excluded.source_location, \
                 source_pattern = excluded.source_pattern, current = excluded.current, \
                 desktop = excluded.desktop, command = excluded.command, \
                 updated_at = excluded.updated_at",
            params![
                app.id,
                app.name,
                app.format.as_str(),
                app.backend,
                app.source.kind.as_str(),
                app.source.location,
                app.source.pattern,
                ver.label,
                app.desktop,
                app.command,
                ts,
            ],
        )?;
        tx.execute(
            "DELETE FROM versions WHERE app_id = ?1 AND label = ?2",
            params![app.id, ver.label],
        )?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM versions WHERE app_id = ?1",
            [&app.id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO versions \
                 (app_id, label, version, marker, entry, exec_kind, seq, installed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                app.id,
                ver.label,
                ver.version,
                ver.marker,
                path_text(&ver.entry)?,
                ver.exec_kind.as_str(),
                seq,
                ver.installed_at,
            ],
        )?;
        for (key, value) in options {
            tx.execute(
                "INSERT INTO options (app_id, key, value) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(app_id, key) DO UPDATE SET value = excluded.value",
                params![app.id, key, value],
            )?;
        }
        tx.execute(
            "DELETE FROM versions WHERE app_id = ?1 AND label <> ?2 AND seq NOT IN \
                 (SELECT seq FROM versions WHERE app_id = ?1 ORDER BY seq DESC LIMIT ?3)",
            params![app.id, ver.label, crate::engine::KEEP_VERSIONS as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<AppRecord>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {APP_COLUMNS} FROM apps WHERE id = ?1"),
                [id],
                app_from_row,
            )
            .optional()?)
    }

    pub fn list(&self) -> Result<Vec<AppRecord>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {APP_COLUMNS} FROM apps ORDER BY id"))?;
        let rows = stmt.query_map([], app_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn versions(&self, id: &str) -> Result<Vec<VersionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT label, version, marker, entry, exec_kind, installed_at \
             FROM versions WHERE app_id = ?1 ORDER BY seq DESC",
        )?;
        let rows = stmt.query_map([id], version_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn files(&self, id: &str) -> Result<Vec<FileRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, kind FROM files WHERE app_id = ?1 ORDER BY path")?;
        let rows = stmt.query_map([id], |row| {
            Ok(FileRecord {
                path: PathBuf::from(row.get::<_, String>(0)?),
                kind: parse_col(row, 1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Makes `label` current and replaces the app's recorded files.
    pub fn set_current(&mut self, id: &str, label: &str, files: &[FileRecord]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let has_version: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM versions WHERE app_id = ?1 AND label = ?2)",
            params![id, label],
            |r| r.get(0),
        )?;
        if !has_version {
            return Err(Error::NotFound(format!("{id} {label}")));
        }
        tx.execute(
            "UPDATE apps SET current = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, label, now()],
        )?;
        replace_files(&tx, id, files)?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_version(&mut self, id: &str, label: &str) -> Result<()> {
        let n = self.conn.execute(
            "DELETE FROM versions WHERE app_id = ?1 AND label = ?2",
            params![id, label],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("{id} {label}")));
        }
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Result<()> {
        let n = self.conn.execute("DELETE FROM apps WHERE id = ?1", [id])?;
        if n == 0 {
            return Err(Error::NotFound(id.to_string()));
        }
        Ok(())
    }

    pub fn set_option(&mut self, id: &str, key: &str, value: &str) -> Result<()> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM apps WHERE id = ?1)",
            [id],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(Error::NotFound(id.to_string()));
        }
        self.conn.execute(
            "INSERT INTO options (app_id, key, value) VALUES (?1, ?2, ?3) \
             ON CONFLICT(app_id, key) DO UPDATE SET value = excluded.value",
            params![id, key, value],
        )?;
        Ok(())
    }

    pub fn option(&self, id: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM options WHERE app_id = ?1 AND key = ?2",
                params![id, key],
                |r| r.get(0),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_manifest_uses_wal() {
        let dir = tempfile::tempdir().unwrap();
        let m = Manifest::open(&dir.path().join("db.sqlite")).unwrap();
        let mode: String = m
            .conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    fn app(id: &str, current: &str) -> AppRecord {
        AppRecord {
            id: id.into(),
            name: "Demo App".into(),
            format: FormatKind::AppImage,
            backend: "appimage".into(),
            source: SourceCfg {
                kind: SourceKind::Github,
                location: "owner/repo".into(),
                pattern: Some("*x86_64*".into()),
            },
            current: current.into(),
            desktop: true,
            command: Some("demo".into()),
        }
    }

    fn ver(label: &str, at: i64) -> VersionRecord {
        VersionRecord {
            label: label.into(),
            version: Some(format!("1.{label}")),
            marker: None,
            entry: PathBuf::from(format!("/apps/demo/{label}/Demo.AppImage")),
            exec_kind: ExecKind::Elf,
            installed_at: at,
        }
    }

    fn files() -> Vec<FileRecord> {
        vec![
            FileRecord {
                path: "/home/u/.local/bin/demo".into(),
                kind: FileKind::Bin,
            },
            FileRecord {
                path: "/home/u/.local/share/applications/unpakt-demo.desktop".into(),
                kind: FileKind::Desktop,
            },
        ]
    }

    #[test]
    fn round_trip_install() {
        let mut m = Manifest::open_in_memory().unwrap();
        let (a, v, f) = (app("demo", "a"), ver("a", 100), files());
        m.commit_install(&a, &v, &f).unwrap();

        assert_eq!(m.get("demo").unwrap().unwrap(), a);
        assert_eq!(m.versions("demo").unwrap(), vec![v]);
        let mut expected = f;
        expected.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(m.files("demo").unwrap(), expected);
        assert_eq!(m.list().unwrap(), vec![a]);
        assert!(m.get("missing").unwrap().is_none());
    }

    #[test]
    fn versions_newest_first() {
        let mut m = Manifest::open_in_memory().unwrap();
        // installed_at is deliberately descending: order must follow seq.
        for (label, at) in [("a", 300), ("b", 200), ("c", 100)] {
            m.commit_install(&app("demo", label), &ver(label, at), &files())
                .unwrap();
        }
        let labels: Vec<_> = m
            .versions("demo")
            .unwrap()
            .into_iter()
            .map(|v| v.label)
            .collect();
        assert_eq!(labels, ["c", "b", "a"]);
        assert_eq!(m.get("demo").unwrap().unwrap().current, "c");
    }

    #[test]
    fn remove_cascades() {
        let mut m = Manifest::open_in_memory().unwrap();
        m.commit_install(&app("demo", "a"), &ver("a", 1), &files())
            .unwrap();
        m.set_option("demo", "k", "v").unwrap();
        m.remove("demo").unwrap();
        assert!(m.get("demo").unwrap().is_none());
        assert!(m.versions("demo").unwrap().is_empty());
        assert!(m.files("demo").unwrap().is_empty());
        assert!(m.option("demo", "k").unwrap().is_none());
        assert!(matches!(m.remove("demo"), Err(Error::NotFound(_))));
    }

    #[test]
    fn reopen_keeps_data_and_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("nested/db.sqlite");
        {
            let mut m = Manifest::open(&db).unwrap();
            m.commit_install(&app("demo", "a"), &ver("a", 1), &files())
                .unwrap();
        }
        let m = Manifest::open(&db).unwrap();
        assert_eq!(m.get("demo").unwrap().unwrap().current, "a");
        let v: i64 = m
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn options_round_trip() {
        let mut m = Manifest::open_in_memory().unwrap();
        m.commit_install(&app("demo", "a"), &ver("a", 1), &[])
            .unwrap();
        assert!(m.option("demo", "k").unwrap().is_none());
        m.set_option("demo", "k", "v1").unwrap();
        m.set_option("demo", "k", "v2").unwrap();
        assert_eq!(m.option("demo", "k").unwrap().as_deref(), Some("v2"));
        assert!(matches!(
            m.set_option("nope", "k", "v"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn reinstall_preserves_created_at_and_replaces_files() {
        let mut m = Manifest::open_in_memory().unwrap();
        m.commit_install(&app("demo", "a"), &ver("a", 1), &files())
            .unwrap();
        m.conn
            .execute("UPDATE apps SET created_at = 5 WHERE id = 'demo'", [])
            .unwrap();
        let only_bin = vec![files().remove(0)];
        m.commit_install(&app("demo", "b"), &ver("b", 2), &only_bin)
            .unwrap();
        let created: i64 = m
            .conn
            .query_row("SELECT created_at FROM apps WHERE id = 'demo'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(created, 5);
        assert_eq!(m.files("demo").unwrap(), only_bin);
    }

    #[test]
    fn set_current_and_delete_version() {
        let mut m = Manifest::open_in_memory().unwrap();
        m.commit_install(&app("demo", "a"), &ver("a", 1), &files())
            .unwrap();
        m.commit_install(&app("demo", "b"), &ver("b", 2), &files())
            .unwrap();
        let only_bin = vec![files().remove(0)];
        m.set_current("demo", "a", &only_bin).unwrap();
        assert_eq!(m.get("demo").unwrap().unwrap().current, "a");
        assert_eq!(m.files("demo").unwrap(), only_bin);
        assert!(matches!(
            m.set_current("demo", "zzz", &[]),
            Err(Error::NotFound(_))
        ));
        m.delete_version("demo", "b").unwrap();
        assert_eq!(m.versions("demo").unwrap().len(), 1);
        assert!(matches!(
            m.delete_version("demo", "b"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn failed_install_leaves_no_partial_state() {
        let mut m = Manifest::open_in_memory().unwrap();
        m.commit_install(&app("demo", "a"), &ver("a", 1), &files())
            .unwrap();
        // Same label again violates the primary key after the app upsert.
        assert!(
            m.commit_install(&app("demo", "a"), &ver("a", 2), &[])
                .is_err()
        );
        assert_eq!(m.files("demo").unwrap().len(), 2);
        assert_eq!(m.versions("demo").unwrap().len(), 1);
    }

    #[test]
    fn failed_install_with_options_leaves_no_option_rows() {
        let mut m = Manifest::open_in_memory().unwrap();
        m.commit_install_with(
            &app("demo", "a"),
            &ver("a", 1),
            &[],
            &[("entry", "bin/a")],
            &[],
        )
        .unwrap();
        assert!(
            m.commit_install_with(
                &app("demo", "a"),
                &ver("a", 2),
                &[],
                &[("pattern", "*.x")],
                &[]
            )
            .is_err()
        );
        assert_eq!(m.option("demo", "entry").unwrap().as_deref(), Some("bin/a"));
        assert_eq!(m.option("demo", "pattern").unwrap(), None);
    }

    #[test]
    fn failed_commit_keeps_rows_it_would_have_pruned() {
        let mut m = Manifest::open_in_memory().unwrap();
        m.commit_install(&app("demo", "a"), &ver("a", 1), &[])
            .unwrap();
        m.commit_install(&app("demo", "b"), &ver("b", 2), &[])
            .unwrap();
        // Re-adding label `b` fails on the primary key, after the prune ran.
        assert!(
            m.commit_install_with(&app("demo", "b"), &ver("b", 3), &[], &[], &["a"])
                .is_err()
        );
        assert_eq!(m.versions("demo").unwrap().len(), 2);

        m.commit_install_with(&app("demo", "c"), &ver("c", 3), &[], &[], &["a"])
            .unwrap();
        let labels: Vec<_> = m
            .versions("demo")
            .unwrap()
            .into_iter()
            .map(|v| v.label)
            .collect();
        assert_eq!(labels, ["c", "b"]);
    }
}
