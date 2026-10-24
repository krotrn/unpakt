mod common;

use common::{Comp, E, MockForge, build_tarball};
use unpakt_core::Error;
use unpakt_core::Layout;
use unpakt_core::engine::{AddOpts, Engine};
use unpakt_core::host::Host;
use unpakt_core::http::Http;
use unpakt_core::model::SourceKind;
use unpakt_core::source::{for_kind, resolve};

const ELF: &[u8] = b"\x7fELF-fixture";

fn tarball_bytes(dir: &std::path::Path) -> Vec<u8> {
    let path = build_tarball(
        dir,
        "unpakt-fixture-mock-1.2.0.tar.gz",
        Comp::Gz,
        &[E::File(
            "unpakt-fixture-mock/unpakt-fixture-mock",
            ELF,
            0o755,
        )],
    );
    std::fs::read(path).unwrap()
}

#[test]
fn resolve_routes_inputs() {
    let (plugin, cfg) = resolve("https://github.com/o/r").unwrap();
    assert_eq!(plugin.kind(), SourceKind::Github);
    assert_eq!(cfg.location, "https://github.com/o/r");

    let (plugin, cfg) = resolve("https://github.com/o/r.git").unwrap();
    assert_eq!(plugin.kind(), SourceKind::Github);
    assert_eq!(cfg.location, "https://github.com/o/r");

    let (plugin, cfg) = resolve("https://example.com/a.tar.gz").unwrap();
    assert_eq!(plugin.kind(), SourceKind::DirectUrl);
    assert_eq!(cfg.location, "https://example.com/a.tar.gz");

    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("x.AppImage");
    std::fs::write(&file, b"x").unwrap();
    let (plugin, cfg) = resolve(file.to_str().unwrap()).unwrap();
    assert_eq!(plugin.kind(), SourceKind::LocalFile);
    assert_eq!(cfg.location, file.canonicalize().unwrap().to_string_lossy());

    assert!(matches!(resolve("nonsense"), Err(Error::NotFound(_))));
}

#[test]
fn github_repo_urls_with_other_hosts_spellings_are_repos() {
    for url in [
        "https://www.github.com/o/r",
        "http://github.com/o/r",
        "http://www.github.com/o/r.git",
    ] {
        let (plugin, cfg) = resolve(url).unwrap();
        assert_eq!(plugin.kind(), SourceKind::Github, "{url}");
        assert_eq!(cfg.location, "https://github.com/o/r", "{url}");
    }
    for url in [
        "https://www.github.com/o/r/tree/main",
        "http://github.com/o/r/blob/main/x",
    ] {
        assert!(matches!(resolve(url), Err(Error::Refused(_))), "{url}");
    }
    let (plugin, _) = resolve("http://www.github.com/o/r/releases/download/v1/x.AppImage").unwrap();
    assert_eq!(plugin.kind(), SourceKind::DirectUrl);
}

#[test]
fn github_urls_with_paths_are_not_latest_release() {
    let (plugin, cfg) = resolve("https://github.com/o/r/releases/download/v1/x.AppImage").unwrap();
    assert_eq!(plugin.kind(), SourceKind::DirectUrl);
    assert_eq!(
        cfg.location,
        "https://github.com/o/r/releases/download/v1/x.AppImage"
    );

    for url in [
        "https://github.com/o/r/releases/tag/v1",
        "https://github.com/o/r/tree/main",
        "https://github.com/o/r/blob/main/x",
        "https://github.com/settings/profile/x",
    ] {
        assert!(matches!(resolve(url), Err(Error::Refused(_))), "{url}");
    }

    let (plugin, cfg) = resolve("https://github.com/o/r.git/").unwrap();
    assert_eq!(plugin.kind(), SourceKind::Github);
    assert_eq!(cfg.location, "https://github.com/o/r");
}

#[test]
fn failing_head_does_not_block_direct_url() {
    let forge = MockForge::start();
    let tmp = tempfile::tempdir().unwrap();
    let bytes = tarball_bytes(tmp.path());
    forge.publish(
        "v1",
        vec![("unpakt-fixture-mock-1.2.0.tar.gz".into(), bytes)],
    );
    forge.set_head_status(405);
    let http = Http::new(&forge.api_url(), None).unwrap();
    let url = forge.download_url("unpakt-fixture-mock-1.2.0.tar.gz");
    let (plugin, cfg) = resolve(&url).unwrap();
    let release = plugin.latest(&cfg, &http).unwrap();
    assert_eq!(release.marker, None);
    assert_eq!(http.head_marker(&url).unwrap(), None);

    let root = tempfile::tempdir().unwrap();
    let layout = Layout::under(root.path());
    let bin = layout.bin.clone();
    let engine = Engine::new(
        layout,
        Host {
            fuse2: true,
            ..Host::detect()
        },
    )
    .unwrap()
    .with_path_var(bin.into_os_string())
    .with_http(http);
    let plan = engine.inspect(&url, AddOpts::default()).unwrap();
    assert_eq!(plan.marker, None);
    assert_eq!(plan.plan.label, "1.2.0");
}

#[test]
fn github_latest_from_mock() {
    let forge = MockForge::start();
    forge.publish("v1.2.0", vec![("a-1.2.0.tar.gz".into(), b"aaa".to_vec())]);
    let http = Http::new(&forge.api_url(), None).unwrap();
    let (plugin, cfg) = resolve("https://github.com/o/r").unwrap();
    let release = plugin.latest(&cfg, &http).unwrap();
    assert_eq!(release.version.as_deref(), Some("v1.2.0"));
    assert_eq!(release.marker.as_deref(), Some("v1.2.0"));
    assert_eq!(release.assets.len(), 1);
    assert_eq!(release.assets[0].name, "a-1.2.0.tar.gz");
    assert_eq!(release.assets[0].url, forge.download_url("a-1.2.0.tar.gz"));
    assert_eq!(release.assets[0].size, Some(3));
    assert_eq!(for_kind(SourceKind::Github).kind(), SourceKind::Github);
}

#[test]
fn download_hashes_and_records_etag() {
    let forge = MockForge::start();
    forge.publish("v1", vec![("blob.bin".into(), b"hello world".to_vec())]);
    let http = Http::new(&forge.api_url(), None).unwrap();
    let dest = tempfile::tempdir().unwrap();
    let dl = http
        .download(&forge.download_url("blob.bin"), dest.path())
        .unwrap();
    assert_eq!(dl.path, dest.path().join("blob.bin"));
    assert_eq!(std::fs::read(&dl.path).unwrap(), b"hello world");
    assert_eq!(
        dl.sha256,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
    assert_eq!(dl.etag.as_deref(), Some(forge.etag("blob.bin").as_str()));
    assert!(dl.last_modified.is_some());
    assert_eq!(
        http.head_marker(&forge.download_url("blob.bin")).unwrap(),
        Some(forge.etag("blob.bin"))
    );
    assert!(forge.auth_for("/dl/blob.bin").iter().all(Option::is_none));
}

#[test]
fn truncated_download_errors_and_leaves_no_part_file() {
    let forge = MockForge::start();
    forge.publish("v1", vec![("blob.bin".into(), vec![7u8; 4096])]);
    forge.set_truncate(true);
    let http = Http::new(&forge.api_url(), None).unwrap();
    let dest = tempfile::tempdir().unwrap();
    let err = http
        .download(&forge.download_url("blob.bin"), dest.path())
        .unwrap_err();
    assert!(matches!(err, Error::Http(_)), "{err}");
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
}

#[test]
fn download_rejects_unsafe_file_names() {
    let forge = MockForge::start();
    let http = Http::new(&forge.api_url(), None).unwrap();
    let dest = tempfile::tempdir().unwrap();
    let base = forge.api_url();
    for url in [
        format!("{base}/dl/"),
        format!("{base}/dl/.."),
        format!("{base}/dl/%2e%2e"),
        format!("{base}/dl/a%2Fb"),
        format!("{base}/dl/a%00b"),
    ] {
        assert!(http.download(&url, dest.path()).is_err(), "{url}");
    }
    assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
}

#[test]
fn token_goes_to_the_api_host_only() {
    let forge = MockForge::start();
    forge.publish("v1", vec![("blob.bin".into(), b"x".to_vec())]);
    let http = Http::new(&forge.api_url(), Some("sekret".into())).unwrap();
    let _: serde_json::Value = http
        .get_json(&format!("{}/repos/o/r/releases/latest", forge.api_url()))
        .unwrap();
    assert_eq!(
        forge.auth_for("/repos/o/r/releases/latest"),
        vec![Some("Bearer sekret".to_string())]
    );
    let dest = tempfile::tempdir().unwrap();
    http.download(&forge.download_url("blob.bin"), dest.path())
        .unwrap();
    assert!(forge.auth_for("/dl/blob.bin").iter().all(Option::is_none));
    // Another host:port never gets the token.
    let other = MockForge::start();
    other.publish("v1", vec![]);
    let _: serde_json::Value = http
        .get_json(&format!("{}/repos/o/r/releases/latest", other.api_url()))
        .unwrap();
    assert_eq!(other.auth_for("/repos/o/r/releases/latest"), vec![None]);
}

#[test]
fn engine_installs_from_mock_github() {
    let forge = MockForge::start();
    let tmp = tempfile::tempdir().unwrap();
    let bytes = tarball_bytes(tmp.path());
    forge.publish(
        "v1.2.0",
        vec![
            (
                "unpakt-fixture-mock-1.2.0.tar.gz.sha256".into(),
                b"abc".to_vec(),
            ),
            ("unpakt-fixture-mock-1.2.0.tar.gz".into(), bytes),
        ],
    );
    let root = tempfile::tempdir().unwrap();
    let layout = Layout::under(root.path());
    let bin = layout.bin.clone();
    let mut engine = Engine::new(
        layout,
        Host {
            fuse2: true,
            ..Host::detect()
        },
    )
    .unwrap()
    .with_path_var(bin.into_os_string())
    .with_http(Http::new(&forge.api_url(), None).unwrap());
    let plan = engine
        .inspect("https://github.com/o/r", AddOpts::default())
        .unwrap();
    assert_eq!(plan.plan.label, "1.2.0");
    assert_eq!(plan.marker.as_deref(), Some("v1.2.0"));
    assert_eq!(plan.source.kind, SourceKind::Github);
    assert_eq!(plan.source.location, "https://github.com/o/r");
    let app = engine.install(plan).unwrap();
    assert_eq!(app.current, "1.2.0");
    assert_eq!(app.source.kind, SourceKind::Github);
}
