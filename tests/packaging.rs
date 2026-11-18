//! `packaging/render.sh`: the formula and manifests the release workflow
//! pushes must carry the release's version and checksums, and nothing else.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ASSETS: [&str; 4] = [
    "gz-macos-arm64",
    "gz-macos-x86_64",
    "gz-linux-x86_64",
    "gz-windows-x86_64.exe",
];

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gz-packaging-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A distinct fake checksum per asset: the asset's index repeated.
fn fake_sum(index: usize) -> String {
    format!("{index}a").repeat(32)
}

fn write_sums(dir: &Path) {
    for (index, asset) in ASSETS.iter().enumerate() {
        // The Windows build writes `sha256sum`'s binary-mode `*name` form.
        let name = if asset.ends_with(".exe") {
            format!("*{asset}")
        } else {
            asset.to_string()
        };
        std::fs::write(
            dir.join(format!("{asset}.sha256")),
            format!("{}  {name}\n", fake_sum(index)),
        )
        .unwrap();
    }
}

fn render(version: &str, out: &Path, sums: &Path) -> Output {
    Command::new("sh")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/render.sh"))
        .args([
            version,
            &out.display().to_string(),
            &sums.display().to_string(),
        ])
        .output()
        .unwrap()
}

fn read(path: PathBuf) -> String {
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

#[test]
fn renders_every_manifest_with_the_release_version_and_checksums() {
    let dir = temp_dir("ok");
    let sums = dir.join("sums");
    std::fs::create_dir_all(&sums).unwrap();
    write_sums(&sums);
    let out = dir.join("out");
    let result = render("v1.2.3", &out, &sums);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let formula = read(out.join("homebrew/Formula/gz.rb"));
    assert!(formula.contains("version \"1.2.3\""), "{formula}");
    for (index, asset) in ASSETS.iter().take(3).enumerate() {
        assert!(
            formula.contains(&format!("releases/download/v1.2.3/{asset}\"")),
            "{asset} url in\n{formula}"
        );
        assert!(
            formula.contains(&format!("sha256 \"{}\"", fake_sum(index))),
            "{formula}"
        );
    }

    let scoop: serde_json::Value =
        serde_json::from_str(&read(out.join("scoop/bucket/gz.json"))).unwrap();
    assert_eq!(scoop["version"], "1.2.3");
    let windows = &scoop["architecture"]["64bit"];
    assert_eq!(windows["hash"], fake_sum(3));
    assert_eq!(
        windows["url"],
        "https://github.com/Sarvesh-GanesanW/gz-cli/releases/download/v1.2.3/gz-windows-x86_64.exe#/gz.exe"
    );

    let winget = out.join("winget/manifests/g/Groundzero/gz/1.2.3");
    let installer = read(winget.join("Groundzero.gz.installer.yaml"));
    assert!(
        installer.contains(&format!("InstallerSha256: {}", fake_sum(3).to_uppercase())),
        "{installer}"
    );
    for file in [
        "Groundzero.gz.yaml",
        "Groundzero.gz.installer.yaml",
        "Groundzero.gz.locale.en-US.yaml",
    ] {
        let text = read(winget.join(file));
        assert!(text.contains("PackageVersion: 1.2.3"), "{file}:\n{text}");
        assert!(!text.contains('@'), "placeholder left in {file}:\n{text}");
    }
}

#[test]
fn refuses_a_missing_or_malformed_checksum() {
    let dir = temp_dir("bad");
    let sums = dir.join("sums");
    std::fs::create_dir_all(&sums).unwrap();
    write_sums(&sums);
    std::fs::remove_file(sums.join("gz-linux-x86_64.sha256")).unwrap();
    let missing = render("1.2.3", &dir.join("out"), &sums);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("gz-linux-x86_64.sha256"));

    std::fs::write(sums.join("gz-linux-x86_64.sha256"), "Not Found\n").unwrap();
    let malformed = render("1.2.3", &dir.join("out"), &sums);
    assert!(!malformed.status.success());
    assert!(String::from_utf8_lossy(&malformed.stderr).contains("does not hold a sha256"));
}

#[test]
fn refuses_a_version_that_is_not_a_release() {
    let dir = temp_dir("version");
    let result = render("latest", &dir.join("out"), &dir);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("not a release version"));
}

fn publish(base: &Path, src: &Path, token: Option<&str>) -> Output {
    let mut cmd = Command::new("sh");
    cmd.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/publish-repo.sh"))
        .args(["owner/homebrew-gz", &src.display().to_string(), "gz v1.2.3"])
        .env(
            "PACKAGE_REPOS_GIT_BASE",
            format!("file://{}", base.display()),
        )
        // Never the developer's git settings (signing, hooks, identity).
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("PACKAGE_REPOS_TOKEN");
    if let Some(token) = token {
        cmd.env("PACKAGE_REPOS_TOKEN", token);
    }
    cmd.output().unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn publishing_pushes_changed_files_once_and_needs_a_token() {
    let dir = temp_dir("publish");
    let remote = dir.join("remote/owner/homebrew-gz.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "--quiet", "--bare"]);
    let src = dir.join("rendered");
    std::fs::create_dir_all(src.join("Formula")).unwrap();
    std::fs::write(src.join("Formula/gz.rb"), "class Gz < Formula\nend\n").unwrap();

    let refused = publish(&dir.join("remote"), &src, None);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("PACKAGE_REPOS_TOKEN is not set"));

    let first = publish(&dir.join("remote"), &src, Some("test-token"));
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        git(&remote, &["show", "main:Formula/gz.rb"]),
        "class Gz < Formula\nend\n"
    );
    assert!(git(&remote, &["log", "-1", "--format=%s", "main"]).contains("gz v1.2.3"));

    let again = publish(&dir.join("remote"), &src, Some("test-token"));
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert!(String::from_utf8_lossy(&again.stdout).contains("nothing to push"));
    assert_eq!(git(&remote, &["rev-list", "--count", "main"]).trim(), "1");
}
