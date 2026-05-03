mod cache;
mod downloader;
mod executor;
mod lifecycle;
mod pipeline;
mod server;
mod ui;
mod util;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use downloader::ensure_ytdlp;
use executor::run_ytdlp;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let raw_args: Vec<String> = env::args().skip(1).collect();

    // Handle --serve mode: run as background media server
    if raw_args.iter().any(|a| a == "--serve") {
        let port = parse_flag_value(&raw_args, "--port")
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_server_port());
        let idle_timeout = parse_flag_value(&raw_args, "--idle-timeout")
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_server_idle_timeout_secs());

        let app_dir = exe_dir()?;
        let _guard = setup_logging(&app_dir);
        let config = load_config(&app_dir.join("config.json"))?;
        let ytdlp_path = resolve_ytdlp_path(&config, &app_dir)?;
        let ffmpeg_path = resolve_ffmpeg_path(&config, &app_dir)?;

        tracing::info!(
            ytdlp = %ytdlp_path.display(),
            ffmpeg = %ffmpeg_path.display(),
            "server tool paths resolved"
        );

        let plugin_dirs = config.plugin_dirs.as_ref().map(|p| {
            if Path::new(p).is_absolute() {
                PathBuf::from(p)
            } else {
                app_dir.join(p)
            }
        });

        // Auto-detect bgutil-pot binary next to the executable
        let bgutil_pot_path = {
            let name = if cfg!(windows) {
                "bgutil-pot.exe"
            } else {
                "bgutil-pot"
            };
            let candidate = app_dir.join(name);
            if candidate.exists() {
                Some(candidate)
            } else {
                None
            }
        };

        let cache_dir = if Path::new(&config.cache_dir).is_absolute() {
            PathBuf::from(&config.cache_dir)
        } else {
            app_dir.join(&config.cache_dir)
        };

        let server_config = server::ServerConfig {
            ytdlp_path,
            ffmpeg_path,
            plugin_dirs,
            extractor_args: config.extractor_args.clone(),
            cache_dir,
            cache_max_size_mb: config.cache_max_size_mb,
            cache_ttl_secs: config.cache_ttl_secs,
        };
        return lifecycle::run_managed_server(
            port,
            idle_timeout,
            server_config,
            bgutil_pot_path,
            config.bgutil_pot_port,
        )
        .await;
    }

    // Launch the visual UI when started without arguments, or explicitly
    // with --ui. This keeps double-click usage friendly while preserving
    // the existing CLI and server modes for VRChat integration.
    if raw_args.is_empty() || raw_args.iter().any(|a| a == "--ui") {
        let port = parse_flag_value(&raw_args, "--ui-port")
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_ui_port());

        let app_dir = exe_dir()?;
        let _guard = setup_logging(&app_dir);
        return ui::run_ui(port).await;
    }

    let app_dir = exe_dir()?;
    let _guard = setup_logging(&app_dir);
    let config = load_config(&app_dir.join("config.json"))?;
    let ytdlp_path = resolve_ytdlp_path(&config, &app_dir)?;
    ensure_ytdlp(&ytdlp_path, &config).await?;
    let args = filter_args(&raw_args, &config);

    let is_get_url = args.iter().any(|a| a == "--get-url");
    let video_url = args
        .iter()
        .find(|a| a.starts_with("http://") || a.starts_with("https://"))
        .cloned();

    tracing::info!(
        url = video_url.as_deref().unwrap_or("(none)"),
        get_url = is_get_url,
        "request received"
    );

    // Only route YouTube URLs through our server — everything else gets
    // passed straight to yt-dlp unmodified so it handles Cloudflare,
    // impersonation, and site-specific quirks itself.
    let is_youtube = video_url
        .as_ref()
        .map(|u| is_youtube_url(u))
        .unwrap_or(false);

    if is_get_url && is_youtube {
        let video_url = video_url.unwrap();

        let url = ensure_server_and_stream(
            &config,
            &video_url,
            &ytdlp_path.to_string_lossy(),
            &args,
        )
        .await?;
        println!("{url}");
    } else {
        tracing::info!(url = video_url.as_deref().unwrap_or("(none)"), "passing through to yt-dlp");
        run_ytdlp(
            &ytdlp_path,
            &args,
            Duration::from_secs(config.execution_timeout_secs),
        )?;
    }

    Ok(())
}

pub(crate) async fn ensure_server_and_stream(
    config: &Config,
    video_url: &str,
    ytdlp_path: &str,
    ytdlp_args: &[String],
) -> Result<String> {
    let port = config.server_port;
    let idle_timeout = config.server_idle_timeout_secs;

    // Check if server is already running
    if !server::check_server_health(port).await {
        tracing::info!("media server not running, starting...");
        server::spawn_server_process(port, idle_timeout)?;

        // Wait for server to come up
        let mut attempts = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if server::check_server_health(port).await {
                tracing::info!("media server is ready");
                break;
            }
            attempts += 1;
            if attempts > 50 {
                bail!("media server failed to start within 5 seconds");
            }
        }
    }

    let stream_id =
        server::register_stream_with_server(port, video_url, ytdlp_path, ytdlp_args).await?;
    let url = server::stream_url(port, &stream_id);
    tracing::info!(url = %url, "stream registered");
    Ok(url)
}

fn parse_flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().map(|s| s.as_str());
        }
    }
    None
}

// --- Config ---

fn default_ytdlp_location() -> String {
    if cfg!(windows) {
        "tools/yt-dlp.exe".into()
    } else {
        "./yt-dlp".into()
    }
}
fn default_ffmpeg_location() -> String {
    if cfg!(windows) {
        "tools/ffmpeg.exe".into()
    } else {
        "./ffmpeg".into()
    }
}
fn default_allowed_args() -> Vec<String> {
    vec!["--get-url".into()]
}
fn default_custom_args() -> Vec<String> {
    vec![
        "--no-check-certificate".into(),
        "--no-warnings".into(),
        "--no-cache-dir".into(),
        "-f".into(),
        "bv[vcodec^=avc][height<=1080]+ba[acodec^=mp4a]/bv[height<=1080]+ba/b[height<=1080]/b"
            .into(),
    ]
}
fn default_cookies_browser() -> String {
    "firefox".into()
}
fn default_execution_timeout_secs() -> u64 {
    120
}
fn default_update_check_days() -> u64 {
    1
}
fn default_extractor_args() -> Vec<String> {
    vec![]
}
fn default_plugin_dirs() -> Option<String> {
    Some(if cfg!(windows) {
        "tools/yt-dlp-plugins".into()
    } else {
        "./yt-dlp-plugins".into()
    })
}
fn default_server_port() -> u16 {
    9851
}
fn default_server_idle_timeout_secs() -> u64 {
    300
}
fn default_bgutil_pot_port() -> u16 {
    4416
}
fn default_cache_dir() -> String {
    "./cache".into()
}
fn default_cache_max_size_mb() -> u64 {
    2048
}
fn default_cache_ttl_secs() -> u64 {
    86400
}
fn default_ui_port() -> u16 {
    9860
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_ytdlp_location")]
    pub ytdlp_location: String,
    #[serde(default = "default_ffmpeg_location")]
    pub ffmpeg_location: String,
    #[serde(default = "default_allowed_args")]
    pub allowed_args: Vec<String>,
    #[serde(default = "default_custom_args")]
    pub custom_args: Vec<String>,
    #[serde(default)]
    pub cookies: bool,
    #[serde(default = "default_cookies_browser")]
    pub cookies_browser: String,
    #[serde(default = "default_execution_timeout_secs")]
    pub execution_timeout_secs: u64,
    #[serde(default = "default_update_check_days")]
    pub update_check_days: u64,
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    #[serde(default = "default_server_idle_timeout_secs")]
    pub server_idle_timeout_secs: u64,
    /// Directory containing yt-dlp plugins (e.g., PO token provider)
    #[serde(default = "default_plugin_dirs")]
    pub plugin_dirs: Option<String>,
    /// Extra yt-dlp extractor args
    #[serde(default = "default_extractor_args")]
    pub extractor_args: Vec<String>,
    /// Port for the bgutil-pot PO token server
    #[serde(default = "default_bgutil_pot_port")]
    pub bgutil_pot_port: u16,
    /// Directory for cached videos
    #[serde(default = "default_cache_dir")]
    pub cache_dir: String,
    /// Maximum cache size in MB
    #[serde(default = "default_cache_max_size_mb")]
    pub cache_max_size_mb: u64,
    /// Cache entry time-to-live in seconds
    #[serde(default = "default_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ytdlp_location: default_ytdlp_location(),
            ffmpeg_location: default_ffmpeg_location(),
            allowed_args: default_allowed_args(),
            custom_args: default_custom_args(),
            cookies: false,
            cookies_browser: default_cookies_browser(),
            execution_timeout_secs: default_execution_timeout_secs(),
            update_check_days: default_update_check_days(),
            server_port: default_server_port(),
            server_idle_timeout_secs: default_server_idle_timeout_secs(),
            plugin_dirs: default_plugin_dirs(),
            extractor_args: default_extractor_args(),
            bgutil_pot_port: default_bgutil_pot_port(),
            cache_dir: default_cache_dir(),
            cache_max_size_mb: default_cache_max_size_mb(),
            cache_ttl_secs: default_cache_ttl_secs(),
        }
    }
}

pub(crate) fn load_config(path: &Path) -> Result<Config> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let config = Config::default();
            write_config(path, &config)?;
            return Ok(config);
        }
        Err(e) => return Err(e).context("reading config file"),
    };

    let mut config: Config = match serde_json::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("invalid config, resetting to defaults: {e}");
            let config = Config::default();
            write_config(path, &config)?;
            return Ok(config);
        }
    };

    // Migrate old allowed_args_with_values format
    if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&content) {
        if let Some(arr) = raw
            .get("allowed_args_with_values")
            .and_then(|v| v.as_array())
        {
            for val in arr {
                if let Some(s) = val.as_str() {
                    let entry = if s.ends_with('=') {
                        s.to_string()
                    } else {
                        format!("{}=", s)
                    };
                    if !config.allowed_args.contains(&entry) {
                        config.allowed_args.push(entry);
                    }
                }
            }
        }
    }

    Ok(config)
}

pub(crate) fn write_config(path: &Path, config: &Config) -> Result<()> {
    let json = serde_json::to_string_pretty(config)?;
    fs::write(path, json).context("writing config file")
}

// --- Arg Filtering ---

pub(crate) fn filter_args(input: &[String], config: &Config) -> Vec<String> {
    let mut result = Vec::new();
    let mut i = 0;

    while i < input.len() {
        let arg = &input[i];

        // Pass through positional arguments (URLs, etc.)
        if !arg.starts_with('-') {
            result.push(arg.clone());
            i += 1;
            continue;
        }

        if let Some(entry) = find_allowed(arg, &config.allowed_args) {
            if entry.ends_with('=') {
                // Value-taking arg
                if arg.contains('=') {
                    // Inline form: --flag=value
                    result.push(arg.clone());
                    i += 1;
                } else if i + 1 < input.len() {
                    // Two-arg form: --flag value
                    result.push(arg.clone());
                    result.push(input[i + 1].clone());
                    i += 2;
                } else {
                    // Flag with no value following — drop it
                    tracing::debug!(arg, "dropping value-taking arg with no value");
                    i += 1;
                }
            } else {
                // Standalone flag
                result.push(arg.clone());
                i += 1;
            }
        } else {
            // When dropping unknown flags, also skip the next arg if it looks like a value
            if !arg.contains('=') && i + 1 < input.len() && !input[i + 1].starts_with('-') {
                tracing::debug!(arg, value = &input[i + 1], "dropping disallowed arg with value");
                i += 2;
            } else {
                tracing::debug!(arg, "dropping disallowed arg");
                i += 1;
            }
        }
    }

    result.extend(config.custom_args.iter().cloned());

    if config.cookies {
        // Check for a cookies.txt file first (works on headless servers)
        let app_dir = exe_dir().unwrap_or_default();
        let cookie_file = app_dir.join("cookies.txt");
        if cookie_file.exists() {
            result.push("--cookies".into());
            result.push(cookie_file.to_string_lossy().to_string());
        } else {
            result.push(format!("--cookies-from-browser={}", config.cookies_browser));
        }
    }

    if let Some(js_runtime) = detect_js_runtime() {
        result.push("--js-runtimes".into());
        result.push(js_runtime);
    }

    result
}

fn find_allowed<'a>(arg: &str, allowed: &'a [String]) -> Option<&'a str> {
    for entry in allowed {
        if entry.ends_with('=') {
            let prefix = &entry[..entry.len() - 1];
            if arg == prefix || arg.starts_with(entry.as_str()) {
                return Some(entry);
            }
        } else if arg == entry.as_str() {
            return Some(entry);
        }
    }
    None
}

// --- URL Classification ---

fn is_youtube_url(url: &str) -> bool {
    ["youtube.com", "youtu.be", "youtube-nocookie.com", "music.youtube.com"]
        .iter()
        .any(|host| url.contains(host))
}

// --- JS Runtime Detection ---

fn detect_js_runtime() -> Option<String> {
    // yt-dlp preference order: deno, node, bun, quickjs
    const CANDIDATES: &[(&str, &[&str])] = &[
        ("deno", &["deno", "deno.exe"]),
        ("node", &["node", "node.exe"]),
        ("bun", &["bun", "bun.exe"]),
        ("quickjs", &["qjs", "qjs.exe"]),
    ];

    for (name, binaries) in CANDIDATES {
        for bin in *binaries {
            if let Some(path) = which(bin) {
                let runtime = format!("{}:{}", name, path.display());
                tracing::info!(runtime = %runtime, "detected JS runtime for yt-dlp");
                return Some(runtime);
            }
        }
    }

    tracing::warn!("no JS runtime found — yt-dlp may fail to solve YouTube challenges");
    None
}

fn which(binary: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// --- Logging ---

fn setup_logging(app_dir: &Path) -> tracing_appender::non_blocking::WorkerGuard {
    let file_appender = tracing_appender::rolling::daily(app_dir, "vrc-ytdlp.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    cleanup_old_logs(app_dir);
    guard
}

fn cleanup_old_logs(dir: &Path) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut log_files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("vrc-ytdlp.log."))
                .unwrap_or(false)
        })
        .collect();

    log_files.sort();

    if log_files.len() > 3 {
        for old in &log_files[..log_files.len() - 3] {
            let _ = fs::remove_file(old);
        }
    }
}

// --- Path Resolution ---

pub(crate) fn exe_dir() -> Result<PathBuf> {
    let exe = env::current_exe().context("cannot determine executable path")?;
    Ok(exe.parent().unwrap_or(Path::new(".")).to_path_buf())
}

pub(crate) fn resolve_ytdlp_path(config: &Config, app_dir: &Path) -> Result<PathBuf> {
    let loc = &config.ytdlp_location;

    let resolved = if Path::new(loc).is_absolute() {
        PathBuf::from(loc)
    } else {
        app_dir.join(loc.replace('/', std::path::MAIN_SEPARATOR_STR))
    };

    if let Ok(current_exe) = env::current_exe() {
        if resolved.exists() {
            let a = fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
            let b = fs::canonicalize(&current_exe).unwrap_or(current_exe);
            if a == b {
                bail!(
                    "ytdlp_location ('{}') points to this executable. Configure a different path.",
                    loc
                );
            }
        }
    }

    Ok(resolved)
}

pub(crate) fn resolve_ffmpeg_path(config: &Config, app_dir: &Path) -> Result<PathBuf> {
    let loc = &config.ffmpeg_location;

    // If explicitly set and non-empty, resolve it
    if !loc.is_empty() {
        let resolved = if Path::new(loc).is_absolute() {
            PathBuf::from(loc)
        } else {
            app_dir.join(loc.replace('/', std::path::MAIN_SEPARATOR_STR))
        };

        if resolved.exists() {
            return Ok(resolved);
        }
    }

    // Fall back to PATH
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Some(path) = which(name) {
        return Ok(path);
    }

    bail!(
        "ffmpeg not found. Place it next to the executable, set ffmpeg_location in config.json, or install it on PATH."
    );
}
