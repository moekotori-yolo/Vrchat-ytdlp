use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use tokio_util::sync::CancellationToken;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cache::{self, VideoCache};
use crate::pipeline::{self, PipelineConfig};

// --- State ---

struct StreamEntry {
    video_url: String,
    ytdlp_args: Vec<String>,
    /// Last time this entry was accessed (registration or GET request).
    /// The periodic purge removes entries idle for longer than 5 minutes.
    last_accessed: u64,
}

pub struct ServerConfig {
    pub ytdlp_path: PathBuf,
    pub ffmpeg_path: PathBuf,
    pub plugin_dirs: Option<PathBuf>,
    pub extractor_args: Vec<String>,
    pub cache_dir: PathBuf,
    pub cache_max_size_mb: u64,
    pub cache_ttl_secs: u64,
}

struct AppState {
    streams: tokio::sync::Mutex<HashMap<String, StreamEntry>>,
    next_id: AtomicU64,
    last_activity: AtomicU64,
    active_pipelines: AtomicU64,
    server_config: ServerConfig,
    cache: Arc<VideoCache>,
}

impl AppState {
    async fn new(config: ServerConfig) -> Result<Self> {
        let cache = VideoCache::new(
            config.cache_dir.clone(),
            config.cache_max_size_mb,
            config.cache_ttl_secs,
        )
        .await?;

        Ok(Self {
            streams: tokio::sync::Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            last_activity: AtomicU64::new(now_secs()),
            active_pipelines: AtomicU64::new(0),
            server_config: config,
            cache: Arc::new(cache),
        })
    }

    fn touch(&self) {
        self.last_activity.store(now_secs(), Ordering::Relaxed);
    }

    fn idle_secs(&self) -> u64 {
        now_secs().saturating_sub(self.last_activity.load(Ordering::Relaxed))
    }
}

use crate::util::now_secs;

// --- Routes ---

#[derive(Deserialize)]
struct RegisterRequest {
    video_url: String,
    #[serde(default)]
    #[allow(dead_code)]
    ytdlp_path: String, // kept for backwards compat, ignored in favor of server config
    ytdlp_args: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct RegisterResponse {
    id: String,
}

async fn health() -> &'static str {
    "ok"
}

async fn register_stream(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<RegisterRequest>,
) -> impl IntoResponse {
    state.touch();
    let id = state
        .next_id
        .fetch_add(1, Ordering::Relaxed)
        .to_string();

    tracing::info!(id = %id, video_url = %req.video_url, "registered stream");

    state.streams.lock().await.insert(
        id.clone(),
        StreamEntry {
            video_url: req.video_url,
            ytdlp_args: req.ytdlp_args,
            last_accessed: now_secs(),
        },
    );

    axum::Json(RegisterResponse { id })
}

async fn stream_video(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    state.touch();

    let entry = {
        let mut streams = state.streams.lock().await;
        streams.get_mut(&id).map(|e| {
            e.last_accessed = now_secs();
            (e.video_url.clone(), e.ytdlp_args.clone())
        })
    };

    let (video_url, ytdlp_args) = match entry {
        Some(e) => e,
        None => {
            tracing::warn!(id = %id, "stream not found");
            return error_response(StatusCode::NOT_FOUND, "stream not found");
        }
    };

    // Parse Range header (for seek support in VRChat video players)
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| parse_range_header(s));

    // --- Check cache ---
    if let Some(cached_path) = state.cache.get(&video_url).await {
        tracing::info!(id = %id, video_url = %video_url, "serving from cache");
        // Don't remove the stream entry — VRChat video players may re-request
        // the same stream ID for seeking. Stale entries are purged periodically.
        return cache::serve_cached_file(&cached_path, range).await;
    }

    // --- Background download already running? Don't block, try Mode 1 again ---
    if state.cache.is_background_downloading(&video_url).await {
        tracing::info!(id = %id, "background download in progress, trying HLS redirect");
        // Fall through to Mode 1 extraction below
    }
    // --- Another foreground request downloading? Wait for it ---
    else if let Some(mut waiter) = state.cache.start_download(&video_url).await {
        tracing::info!(id = %id, "waiting for in-progress download of same URL");
        match waiter.recv().await {
            Ok(Ok(())) => {
                if let Some(cached_path) = state.cache.get(&video_url).await {
                    tracing::info!(id = %id, "serving from cache after wait");
                    return cache::serve_cached_file(&cached_path, range).await;
                }
            }
            _ => {
                tracing::warn!(id = %id, "waited download failed");
            }
        }
    }

    tracing::info!(id = %id, video_url = %video_url, "cache miss");

    let pipeline_config = PipelineConfig {
        ytdlp_path: state.server_config.ytdlp_path.clone(),
        ffmpeg_path: state.server_config.ffmpeg_path.clone(),
        ytdlp_args,
        plugin_dirs: state.server_config.plugin_dirs.clone(),
        extractor_args: state.server_config.extractor_args.clone(),
    };

    // --- Mode 1: HLS passthrough (fast) ---
    if let Ok(extracted) = pipeline::extract_streaming_url(&pipeline_config, &video_url, &id).await
    {
        if extracted.is_hls && pipeline::validate_hls_url(&extracted.url, &id).await {
            tracing::info!(id = %id, "Mode 1: redirecting to HLS stream");
            spawn_background_download(state.clone(), pipeline_config.clone(), video_url, id);
            return Response::builder()
                .status(StatusCode::FOUND)
                .header("location", &extracted.url)
                .body(Body::empty())
                .unwrap();
        }
    }

    // --- Mode 2: Download + remux + serve (reliable fallback) ---
    tracing::info!(id = %id, "Mode 2: downloading and remuxing");
    match download_and_cache(&state, &pipeline_config, &video_url, &id).await {
        Ok(cached_path) => cache::serve_cached_file(&cached_path, range).await,
        Err(e) => {
            tracing::error!(id = %id, error = %e, "pipeline failed");
            error_response(StatusCode::BAD_GATEWAY, &format!("pipeline failed: {e}"))
        }
    }
}

/// Download a video, remux it, and register in cache. Used by both Mode 2
/// (foreground) and the background caching task.
async fn download_and_cache(
    state: &Arc<AppState>,
    config: &PipelineConfig,
    video_url: &str,
    stream_id: &str,
) -> Result<PathBuf> {
    state.active_pipelines.fetch_add(1, Ordering::Relaxed);

    let cache_tmp = state.cache.tmp_path(video_url);
    let result = pipeline::download_to_file(config, video_url, stream_id, &cache_tmp).await;

    state.active_pipelines.fetch_sub(1, Ordering::Relaxed);
    state.touch();

    match result {
        Ok(()) => state
            .cache
            .finish_download(video_url, &cache_tmp)
            .await
            .context("cache finalization failed"),
        Err(e) => {
            state
                .cache
                .fail_download(video_url, &e.to_string())
                .await;
            Err(e)
        }
    }
}

/// Fire-and-forget background download for Mode 1 caching.
fn spawn_background_download(
    state: Arc<AppState>,
    config: PipelineConfig,
    video_url: String,
    stream_id: String,
) {
    tokio::spawn(async move {
        if !state.cache.start_background_download(&video_url).await {
            return; // another task already downloading
        }
        let result = download_and_cache(&state, &config, &video_url, &stream_id).await;
        state.cache.finish_background_download(&video_url).await;
        match result {
            Ok(_) => tracing::info!(id = %stream_id, "background cache complete"),
            Err(e) => tracing::warn!(id = %stream_id, error = %e, "background download failed"),
        }
    });
}

/// Parse "Range: bytes=START-END" header. Returns (start, optional_end).
fn parse_range_header(header: &str) -> Option<(u64, Option<u64>)> {
    let bytes_prefix = header.strip_prefix("bytes=")?;
    let mut parts = bytes_prefix.splitn(2, '-');
    let start_str = parts.next()?;
    let end_str = parts.next()?;

    let start = start_str.parse::<u64>().ok()?;
    let end = if end_str.is_empty() {
        None
    } else {
        Some(end_str.parse::<u64>().ok()?)
    };

    Some((start, end))
}

fn error_response(status: StatusCode, msg: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::from(msg.to_string()))
        .unwrap()
}

// --- Server Entry Point ---

pub async fn run_server(
    port: u16,
    idle_timeout_secs: u64,
    server_config: ServerConfig,
    shutdown: CancellationToken,
) -> Result<()> {
    let state = Arc::new(
        AppState::new(server_config)
            .await
            .context("initializing server state")?,
    );

    // Start background cache eviction
    cache::spawn_eviction_task(state.cache.clone(), shutdown.clone());

    let app = Router::new()
        .route("/health", get(health))
        .route("/stream", post(register_stream))
        .route("/stream/{id}", get(stream_video))
        .with_state(state.clone());

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context(format!("binding to {addr}"))?;

    tracing::info!(addr = %addr, "media server started");

    // Idle timeout watcher — cancels the shutdown token when idle.
    // Also periodically cleans up stale stream entries (registered but never requested).
    let state_for_timeout = state.clone();
    let idle_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let mut tick = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            if idle_shutdown.is_cancelled() {
                return;
            }
            tick += 1;

            // Every 60s, purge stale stream entries older than 5 minutes
            if tick % 6 == 0 {
                let now = crate::util::now_secs();
                let mut streams = state_for_timeout.streams.lock().await;
                let before = streams.len();
                streams.retain(|_id, entry| {
                    now.saturating_sub(entry.last_accessed) < 300
                });
                let removed = before - streams.len();
                if removed > 0 {
                    tracing::info!(removed, "purged stale stream entries");
                }
            }

            let idle = state_for_timeout.idle_secs();
            let active = state_for_timeout.active_pipelines.load(Ordering::Relaxed);
            if idle >= idle_timeout_secs && active == 0 {
                tracing::info!(idle_secs = idle, "server idle timeout reached, shutting down");
                idle_shutdown.cancel();
                return;
            }
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("running media server")
}

// --- Client helpers (used by the wrapper to talk to the server) ---

pub async fn check_server_health(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/health");
    reqwest::get(&url)
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

pub fn spawn_server_process(port: u16, idle_timeout_secs: u64) -> Result<()> {
    let exe = std::env::current_exe().context("getting current exe path")?;

    // Prevent the server from inheriting our stdout/stderr handles.
    // Without this, a parent process (e.g., VRChat) waiting on our pipes
    // will hang forever because the server holds a copy of the handle.
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE};
        unsafe {
            let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
            let stderr = GetStdHandle(STD_ERROR_HANDLE);
            SetHandleInformation(stdout, HANDLE_FLAG_INHERIT, 0);
            SetHandleInformation(stderr, HANDLE_FLAG_INHERIT, 0);
        }
    }

    tracing::debug!(exe = %exe.display(), port, idle_timeout_secs, "spawning detached server process");

    let mut cmd = std::process::Command::new(&exe);
    cmd.args([
        "--serve",
        "--port",
        &port.to_string(),
        "--idle-timeout",
        &idle_timeout_secs.to_string(),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    // Ensure the detached server inherits a valid temp directory
    .env("TEMP", std::env::temp_dir())
    .env("TMP", std::env::temp_dir());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;

        // Try with breakaway first — escapes VRChat's Job Object so the
        // server isn't killed when VRChat terminates our CLI process.
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
        match cmd.spawn() {
            Ok(_) => {
                tracing::debug!("server process spawned (with job breakaway)");
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(error = %e, "breakaway spawn failed, retrying without");
                cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
            }
        }
    }

    cmd.spawn().context("spawning server process")?;

    tracing::debug!("server process spawned");
    Ok(())
}

pub async fn register_stream_with_server(
    port: u16,
    video_url: &str,
    ytdlp_path: &str,
    ytdlp_args: &[String],
) -> Result<String> {
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "video_url": video_url,
        "ytdlp_path": ytdlp_path,
        "ytdlp_args": ytdlp_args,
    });

    tracing::debug!(port, video_url, "registering stream with server");

    let resp: RegisterResponse = client
        .post(format!("http://127.0.0.1:{port}/stream"))
        .json(&body)
        .send()
        .await
        .context("posting stream to server")?
        .error_for_status()
        .context("server returned error")?
        .json()
        .await
        .context("parsing server response")?;

    tracing::debug!(id = %resp.id, "stream registered");
    Ok(resp.id)
}

pub fn stream_url(port: u16, id: &str) -> String {
    format!("http://127.0.0.1:{port}/stream/{id}")
}
