use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::Config;

pub const GH_PROXY_PREFIX: &str = "https://gh-proxy.org/";
const GITHUB_API_URL: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";
// Use yt-dlp_x86.exe on Windows (py2exe, no PyInstaller temp dir issues)
// Use yt-dlp_linux on Linux
const ASSET_NAME: &str = if cfg!(windows) {
    "yt-dlp_x86.exe"
} else {
    "yt-dlp_linux"
};

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

pub async fn ensure_ytdlp(exe_path: &Path, config: &Config) -> Result<()> {
    let exe_dir = exe_path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(exe_dir).context("creating yt-dlp directory")?;

    let version_path = exe_dir.join("version.txt");

    if exe_path.exists() {
        // Check if update is needed
        if !should_check_update(&version_path, config.update_check_days) {
            tracing::debug!("skipping update check (checked recently)");
            return Ok(());
        }

        match try_update(exe_path, &version_path).await {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("update check failed, continuing with existing binary: {e}");
            }
        }
    } else {
        tracing::info!("yt-dlp not found, downloading...");
        download_latest(exe_path, &version_path).await?;
        tracing::info!("yt-dlp downloaded successfully");
    }

    Ok(())
}

fn should_check_update(version_path: &Path, check_days: u64) -> bool {
    let mtime = match fs::metadata(version_path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return true,
    };

    let age = SystemTime::now()
        .duration_since(mtime)
        .unwrap_or(Duration::MAX);

    age > Duration::from_secs(check_days * 86400)
}

async fn try_update(exe_path: &Path, version_path: &Path) -> Result<()> {
    let client = build_client()?;
    let release = fetch_release(&client).await?;

    let current_version = fs::read_to_string(version_path).unwrap_or_default();
    let current_version = current_version.trim();

    if current_version == release.tag_name {
        // Same version — touch the file to reset check interval
        tracing::info!(version = %release.tag_name, "yt-dlp is up to date");
        fs::write(version_path, &release.tag_name).ok();
        return Ok(());
    }

    tracing::info!(
        current = current_version,
        latest = %release.tag_name,
        "new yt-dlp version available, updating"
    );

    download_release(&client, &release, exe_path, version_path).await
}

async fn download_latest(exe_path: &Path, version_path: &Path) -> Result<()> {
    let client = build_client()?;
    let release = fetch_release(&client).await?;
    download_release(&client, &release, exe_path, version_path).await
}

async fn download_release(
    client: &reqwest::Client,
    release: &GitHubRelease,
    exe_path: &Path,
    version_path: &Path,
) -> Result<()> {
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == ASSET_NAME)
        .context(format!("{ASSET_NAME} not found in release assets"))?;

    let bytes = download_bytes(client, &asset.browser_download_url).await?;

    let tmp_path = exe_path.with_file_name(".yt-dlp-download.tmp");

    // Write to temp, then replace
    if let Err(e) = write_and_replace(&tmp_path, exe_path, &bytes) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    fs::write(version_path, &release.tag_name).context("writing version file")?;
    Ok(())
}

fn write_and_replace(tmp: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    fs::write(tmp, bytes).context("writing temp file")?;

    // On Windows, rename fails if destination exists; delete first.
    // Ignore NotFound errors (file may not exist yet on first download).
    if let Err(e) = fs::remove_file(target) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(e).context("removing old binary");
        }
    }

    fs::rename(tmp, target).context("renaming temp to target")
}

fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("VRC-YtDlp")
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}

async fn fetch_release(client: &reqwest::Client) -> Result<GitHubRelease> {
    let proxied_api = proxied_url(GITHUB_API_URL);
    let mut last_error = None;

    for url in [proxied_api.as_str(), GITHUB_API_URL] {
        match client.get(url).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(ok_resp) => {
                    return ok_resp
                        .json()
                        .await
                        .with_context(|| format!("parsing release JSON from {url}"));
                }
                Err(err) => {
                    last_error = Some(
                        anyhow::Error::new(err)
                            .context(format!("fetching latest release from {url}")),
                    )
                }
            },
            Err(err) => {
                last_error = Some(
                    anyhow::Error::new(err)
                        .context(format!("requesting latest release from {url}")),
                )
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("fetching latest release failed")))
}

async fn download_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let proxied = proxied_url(url);
    let mut last_error = None;

    for candidate in [proxied.as_str(), url] {
        tracing::info!(url = %candidate, "downloading yt-dlp");
        match client.get(candidate).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(ok_resp) => {
                    let bytes = ok_resp
                        .bytes()
                        .await
                        .with_context(|| format!("reading yt-dlp binary from {candidate}"))?;
                    return Ok(bytes.to_vec());
                }
                Err(err) => {
                    last_error = Some(
                        anyhow::Error::new(err)
                            .context(format!("downloading yt-dlp binary from {candidate}")),
                    )
                }
            },
            Err(err) => {
                last_error = Some(
                    anyhow::Error::new(err)
                        .context(format!("requesting yt-dlp binary from {candidate}")),
                )
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("downloading yt-dlp binary failed")))
}

fn proxied_url(url: &str) -> String {
    format!("{GH_PROXY_PREFIX}{url}")
}
