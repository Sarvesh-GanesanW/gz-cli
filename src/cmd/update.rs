use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const FALLBACK_REPO: &str = "Sarvesh-GanesanW/gz-cli";

fn repo() -> String {
    std::env::var("GZ_UPDATE_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| FALLBACK_REPO.to_string())
}

fn asset_name() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("gz-linux-x86_64"),
        ("macos", "aarch64") => Ok("gz-macos-arm64"),
        ("macos", "x86_64") => Ok("gz-macos-x86_64"),
        ("windows", "x86_64") => Ok("gz-windows-x86_64.exe"),
        (os, arch) => anyhow::bail!("no release binary for {os}/{arch} (build from source)"),
    }
}

fn parse_version(tag: &str) -> Vec<u64> {
    tag.trim_start_matches('v')
        .split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0))
        .collect()
}

/// Positive when `latest` is newer than `current`.
fn cmp_versions(current: &str, latest: &str) -> std::cmp::Ordering {
    let mut a = parse_version(current);
    let mut b = parse_version(latest);
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a.cmp(&b).reverse()
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("gz/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("cannot build http client")
}

async fn latest_tag(client: &reqwest::Client) -> Result<String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo());
    let release: Release = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?
        .error_for_status()
        .context("no releases published yet")?
        .json()
        .await
        .context("cannot parse release metadata")?;
    Ok(release.tag_name)
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let bytes = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot download {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed: {url}"))?
        .bytes()
        .await
        .context("cannot read download")?;
    Ok(bytes.to_vec())
}

pub async fn run(version: Option<&str>, check: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let client = client()?;
    let tag = match version {
        Some(v) => v.to_string(),
        None => latest_tag(&client).await?,
    };
    match cmp_versions(current, &tag) {
        std::cmp::Ordering::Less => {
            println!("gz {current} is newer than {tag}; nothing to do.");
            return Ok(());
        }
        std::cmp::Ordering::Equal => {
            println!("gz {current} is already the latest release.");
            return Ok(());
        }
        std::cmp::Ordering::Greater => {}
    }
    println!("gz {current} → {tag}");
    if check {
        return Ok(());
    }
    let asset = asset_name()?;
    let base = format!("https://github.com/{}/releases/download/{tag}", repo());
    let bytes = download(&client, &format!("{base}/{asset}")).await?;
    let checksum_url = format!("{base}/{asset}.sha256");
    let checksum = match download(&client, &checksum_url).await {
        Ok(raw) => String::from_utf8_lossy(&raw)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
        Err(_) => String::new(),
    };
    if !checksum.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let actual = format!("{:x}", hasher.finalize());
        anyhow::ensure!(
            actual.eq_ignore_ascii_case(&checksum),
            "checksum mismatch for {asset} (expected {checksum}, got {actual})"
        );
    }
    replace_current_exe(&bytes)?;
    println!("updated to {tag}. Run `gz --version` to confirm.");
    Ok(())
}

fn replace_current_exe(bytes: &[u8]) -> Result<()> {
    let exe = std::env::current_exe().context("cannot locate running binary")?;
    let backup = exe.with_extension("old");
    if backup.exists() {
        std::fs::remove_file(&backup).ok();
    }
    // Renaming the running binary first works on Windows too, where an
    // in-place overwrite of a live .exe is refused.
    std::fs::rename(&exe, &backup).with_context(|| {
        format!(
            "cannot replace {} (no write permission? try re-running elevated)",
            exe.display()
        )
    })?;
    if let Err(err) = std::fs::write(&exe, bytes) {
        let _ = std::fs::rename(&backup, &exe);
        return Err(err).with_context(|| format!("cannot write {}", exe.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).ok();
    }
    std::fs::remove_file(&backup).ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        use std::cmp::Ordering::*;
        assert_eq!(cmp_versions("0.1.0", "v0.2.0"), Greater);
        assert_eq!(cmp_versions("0.2.0", "v0.2.0"), Equal);
        assert_eq!(cmp_versions("0.2.0", "v0.10.0"), Greater);
        assert_eq!(cmp_versions("0.3.0", "v0.2.0"), Less);
        assert_eq!(cmp_versions("1.0", "v1.0.0"), Equal);
    }

    #[test]
    fn picks_asset_for_this_platform() {
        assert!(asset_name().is_ok());
    }
}
