use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::downloader::ensure_ytdlp;
use crate::{ensure_server_and_stream, server, Config};

struct UiState {
    app_dir: PathBuf,
}

#[derive(Serialize)]
struct StatusResponse {
    app_dir: String,
    proxy_prefix: &'static str,
    ytdlp_path: String,
    ffmpeg_path: String,
    ytdlp_exists: bool,
    ffmpeg_exists: bool,
    media_server_port: u16,
    media_server_running: bool,
    config: Config,
}

#[derive(Deserialize)]
struct StreamRequest {
    video_url: String,
}

#[derive(Serialize)]
struct StreamResponse {
    stream_url: String,
    filtered_args: Vec<String>,
}

#[derive(Deserialize)]
struct ExecuteRequest {
    video_url: String,
    extra_args: Vec<String>,
}

#[derive(Serialize)]
struct ExecuteResponse {
    filtered_args: Vec<String>,
    stdout: String,
    stderr: String,
}

#[derive(Serialize)]
struct MessageResponse {
    message: String,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

pub async fn run_ui(port: u16) -> Result<()> {
    let app_dir = crate::exe_dir()?;
    let state = Arc::new(UiState { app_dir });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(get_status))
        .route("/api/config", get(get_config).post(save_config))
        .route("/api/stream", post(create_stream))
        .route("/api/execute", post(execute_ytdlp))
        .route("/api/tools/update-ytdlp", post(update_ytdlp))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context(format!("binding UI server to {addr}"))?;

    let url = format!("http://127.0.0.1:{port}/");
    tracing::info!(url = %url, "visual UI started");
    let _ = open_browser(&url);

    axum::serve(listener, app)
        .await
        .context("running visual UI server")
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn get_status(State(state): State<Arc<UiState>>) -> impl IntoResponse {
    let path = state.app_dir.join("config.json");
    match crate::load_config(&path) {
        Ok(config) => {
            let ytdlp_path = crate::resolve_ytdlp_path(&config, &state.app_dir)
                .unwrap_or_else(|_| state.app_dir.join(&config.ytdlp_location));
            let ffmpeg_path = best_effort_tool_path(&state.app_dir, &config.ffmpeg_location);
            let media_server_running = server::check_server_health(config.server_port).await;
            Json(StatusResponse {
                app_dir: state.app_dir.display().to_string(),
                proxy_prefix: crate::downloader::GH_PROXY_PREFIX,
                ytdlp_path: ytdlp_path.display().to_string(),
                ffmpeg_path: ffmpeg_path.display().to_string(),
                ytdlp_exists: ytdlp_path.exists(),
                ffmpeg_exists: ffmpeg_path.exists(),
                media_server_port: config.server_port,
                media_server_running,
                config,
            })
            .into_response()
        }
        Err(err) => error_json(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn get_config(State(state): State<Arc<UiState>>) -> impl IntoResponse {
    let path = state.app_dir.join("config.json");
    match crate::load_config(&path) {
        Ok(config) => Json(config).into_response(),
        Err(err) => error_json(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn save_config(
    State(state): State<Arc<UiState>>,
    Json(config): Json<Config>,
) -> impl IntoResponse {
    let path = state.app_dir.join("config.json");
    match crate::write_config(&path, &config) {
        Ok(()) => Json(MessageResponse {
            message: "配置已保存".into(),
        })
        .into_response(),
        Err(err) => error_json(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

async fn create_stream(
    State(state): State<Arc<UiState>>,
    Json(req): Json<StreamRequest>,
) -> impl IntoResponse {
    if req.video_url.trim().is_empty() {
        return error_json(StatusCode::BAD_REQUEST, anyhow::anyhow!("请输入视频地址"));
    }

    let (config, ytdlp_path) = match load_ytdlp_runtime(&state).await {
        Ok(runtime) => runtime,
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    if let Err(err) = ensure_ytdlp(&ytdlp_path, &config).await {
        return error_json(StatusCode::BAD_GATEWAY, err);
    }

    let raw_args = vec!["--get-url".to_string(), req.video_url.clone()];
    let filtered_args = crate::filter_args(&raw_args, &config);

    match ensure_server_and_stream(
        &config,
        &req.video_url,
        &ytdlp_path.to_string_lossy(),
        &filtered_args,
    )
    .await
    {
        Ok(stream_url) => Json(StreamResponse {
            stream_url,
            filtered_args,
        })
        .into_response(),
        Err(err) => error_json(StatusCode::BAD_GATEWAY, err),
    }
}

async fn execute_ytdlp(
    State(state): State<Arc<UiState>>,
    Json(req): Json<ExecuteRequest>,
) -> impl IntoResponse {
    let (config, ytdlp_path) = match load_ytdlp_runtime(&state).await {
        Ok(runtime) => runtime,
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    if let Err(err) = ensure_ytdlp(&ytdlp_path, &config).await {
        return error_json(StatusCode::BAD_GATEWAY, err);
    }

    let mut raw_args = sanitize_args(req.extra_args);
    if !req.video_url.trim().is_empty() {
        raw_args.push(req.video_url.trim().to_string());
    }

    if raw_args.is_empty() {
        return error_json(StatusCode::BAD_REQUEST, anyhow::anyhow!("请至少填写一个地址或参数"));
    }

    let filtered_args = crate::filter_args(&raw_args, &config);
    match run_ytdlp_capture_async(
        &ytdlp_path,
        &filtered_args,
        Duration::from_secs(config.execution_timeout_secs),
    )
    .await
    {
        Ok((stdout, stderr)) => Json(ExecuteResponse {
            filtered_args,
            stdout,
            stderr,
        })
        .into_response(),
        Err(err) => error_json(StatusCode::BAD_GATEWAY, err),
    }
}

async fn update_ytdlp(State(state): State<Arc<UiState>>) -> impl IntoResponse {
    let path = state.app_dir.join("config.json");
    let config = match crate::load_config(&path) {
        Ok(config) => config,
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    let ytdlp_path = match crate::resolve_ytdlp_path(&config, &state.app_dir) {
        Ok(path) => path,
        Err(err) => return error_json(StatusCode::INTERNAL_SERVER_ERROR, err),
    };

    match ensure_ytdlp(&ytdlp_path, &config).await {
        Ok(()) => Json(MessageResponse {
            message: format!("yt-dlp 已准备就绪: {}", ytdlp_path.display()),
        })
        .into_response(),
        Err(err) => error_json(StatusCode::BAD_GATEWAY, err),
    }
}

async fn load_ytdlp_runtime(state: &UiState) -> Result<(Config, PathBuf)> {
    let config_path = state.app_dir.join("config.json");
    let config = crate::load_config(&config_path)?;
    let ytdlp_path = crate::resolve_ytdlp_path(&config, &state.app_dir)?;
    Ok((config, ytdlp_path))
}

fn sanitize_args(args: Vec<String>) -> Vec<String> {
    args.into_iter()
        .map(|arg| arg.trim().to_string())
        .filter(|arg| !arg.is_empty())
        .collect()
}

fn best_effort_tool_path(app_dir: &Path, location: &str) -> PathBuf {
    let path = Path::new(location);
    if path.is_absolute() {
        PathBuf::from(location)
    } else {
        app_dir.join(location.replace('/', std::path::MAIN_SEPARATOR_STR))
    }
}

async fn run_ytdlp_capture_async(
    exe_path: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<(String, String)> {
    let work_dir = exe_path.parent().unwrap_or(Path::new("."));
    let tmp_dir = work_dir.join("tmp");
    std::fs::create_dir_all(&tmp_dir).context("creating tmp directory next to yt-dlp")?;

    let mut cmd = Command::new(exe_path);
    cmd.args(args)
        .current_dir(work_dir)
        .env("TEMP", &tmp_dir)
        .env("TMP", &tmp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .context("yt-dlp timed out")?
        .context("spawning yt-dlp")?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        let status = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".into());
        let message = if stderr.is_empty() {
            format!("yt-dlp exited with {status}")
        } else {
            format!("yt-dlp exited with {status}: {stderr}")
        };
        bail!(message);
    }

    Ok((stdout, stderr))
}

fn error_json(status: StatusCode, err: anyhow::Error) -> axum::response::Response {
    (
        status,
        Json(ErrorResponse {
            error: err.to_string(),
        }),
    )
        .into_response()
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("opening default browser")?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("opening default browser")?;
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("opening default browser")?;
        return Ok(());
    }

    #[allow(unreachable_code)]
    Ok(())
}

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>vrc-ytdlp 控制台</title>
  <style>
    :root {
      --bg: #f5efe3;
      --panel: rgba(255, 252, 246, 0.86);
      --panel-strong: rgba(255, 250, 242, 0.95);
      --ink: #1f2a33;
      --muted: #5b6a73;
      --accent: #d45f39;
      --accent-2: #0f8b8d;
      --accent-3: #f2b544;
      --line: rgba(31, 42, 51, 0.09);
      --shadow: 0 24px 70px rgba(85, 55, 22, 0.14);
      --success: #18805a;
      --danger: #b53f2c;
      --radius: 24px;
    }

    * {
      box-sizing: border-box;
    }

    body {
      margin: 0;
      min-height: 100vh;
      font-family: "Segoe UI Variable", "Bahnschrift", "Trebuchet MS", sans-serif;
      color: var(--ink);
      background:
        radial-gradient(circle at top left, rgba(242, 181, 68, 0.35), transparent 28%),
        radial-gradient(circle at top right, rgba(15, 139, 141, 0.22), transparent 30%),
        linear-gradient(145deg, #efe5d0 0%, #f8f3ea 45%, #ede6dc 100%);
      overflow-x: hidden;
    }

    body::before,
    body::after {
      content: "";
      position: fixed;
      z-index: 0;
      border-radius: 999px;
      filter: blur(10px);
      pointer-events: none;
    }

    body::before {
      width: 280px;
      height: 280px;
      top: -70px;
      right: -40px;
      background: rgba(212, 95, 57, 0.12);
    }

    body::after {
      width: 240px;
      height: 240px;
      left: -50px;
      bottom: 40px;
      background: rgba(15, 139, 141, 0.14);
    }

    .shell {
      position: relative;
      z-index: 1;
      width: min(1180px, calc(100% - 32px));
      margin: 32px auto 56px;
    }

    .hero {
      position: relative;
      overflow: hidden;
      padding: 30px;
      border-radius: 32px;
      background:
        linear-gradient(135deg, rgba(255, 252, 247, 0.95), rgba(247, 240, 228, 0.88)),
        linear-gradient(135deg, rgba(212, 95, 57, 0.16), rgba(15, 139, 141, 0.1));
      box-shadow: var(--shadow);
      border: 1px solid rgba(255, 255, 255, 0.6);
      animation: rise 0.7s ease;
    }

    .hero::before {
      content: "";
      position: absolute;
      inset: auto -60px -80px auto;
      width: 240px;
      height: 240px;
      background: radial-gradient(circle, rgba(212, 95, 57, 0.18), transparent 68%);
      transform: rotate(-14deg);
    }

    .eyebrow {
      display: inline-flex;
      align-items: center;
      gap: 10px;
      padding: 8px 14px;
      border-radius: 999px;
      background: rgba(31, 42, 51, 0.05);
      color: var(--muted);
      font-size: 13px;
      letter-spacing: 0.08em;
      text-transform: uppercase;
    }

    h1 {
      margin: 18px 0 12px;
      font-size: clamp(30px, 5vw, 56px);
      line-height: 0.96;
      letter-spacing: -0.04em;
      max-width: 700px;
    }

    .subtitle {
      margin: 0;
      max-width: 760px;
      color: var(--muted);
      font-size: 16px;
      line-height: 1.7;
    }

    .hero-grid,
    .panel-grid {
      display: grid;
      gap: 18px;
    }

    .hero-grid {
      grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
      margin-top: 26px;
    }

    .panel-grid {
      grid-template-columns: repeat(auto-fit, minmax(300px, 1fr));
      margin-top: 22px;
    }

    .stat,
    .panel {
      position: relative;
      overflow: hidden;
      border-radius: var(--radius);
      background: var(--panel);
      backdrop-filter: blur(14px);
      border: 1px solid rgba(255, 255, 255, 0.65);
      box-shadow: var(--shadow);
    }

    .stat {
      padding: 18px 20px;
      animation: rise 0.8s ease both;
    }

    .panel {
      padding: 22px;
      animation: rise 0.9s ease both;
    }

    .panel.wide {
      grid-column: 1 / -1;
    }

    .stat-label,
    .panel-kicker {
      color: var(--muted);
      font-size: 13px;
      letter-spacing: 0.05em;
      text-transform: uppercase;
    }

    .stat-value {
      margin-top: 8px;
      font-size: 20px;
      font-weight: 700;
      word-break: break-all;
    }

    .panel h2 {
      margin: 10px 0 8px;
      font-size: 24px;
      letter-spacing: -0.03em;
    }

    .panel p {
      margin: 0 0 18px;
      color: var(--muted);
      line-height: 1.65;
    }

    .row {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
      gap: 14px;
      margin-bottom: 14px;
    }

    label {
      display: block;
      font-size: 13px;
      color: var(--muted);
      margin-bottom: 7px;
      letter-spacing: 0.03em;
    }

    input,
    textarea,
    button {
      font: inherit;
    }

    input,
    textarea {
      width: 100%;
      border: 1px solid var(--line);
      border-radius: 18px;
      padding: 14px 15px;
      background: var(--panel-strong);
      color: var(--ink);
      outline: none;
      transition: border-color 0.18s ease, transform 0.18s ease, box-shadow 0.18s ease;
    }

    textarea {
      min-height: 116px;
      resize: vertical;
    }

    input:focus,
    textarea:focus {
      border-color: rgba(15, 139, 141, 0.45);
      box-shadow: 0 0 0 4px rgba(15, 139, 141, 0.08);
      transform: translateY(-1px);
    }

    .check {
      display: flex;
      align-items: center;
      gap: 10px;
      min-height: 54px;
      padding: 12px 14px;
      border-radius: 18px;
      border: 1px solid var(--line);
      background: var(--panel-strong);
    }

    .check input {
      width: 18px;
      height: 18px;
      margin: 0;
    }

    .actions {
      display: flex;
      flex-wrap: wrap;
      gap: 12px;
      margin-top: 16px;
    }

    button {
      border: 0;
      border-radius: 16px;
      padding: 13px 18px;
      cursor: pointer;
      transition: transform 0.18s ease, box-shadow 0.18s ease, opacity 0.18s ease;
    }

    button:hover {
      transform: translateY(-2px);
      box-shadow: 0 10px 24px rgba(31, 42, 51, 0.12);
    }

    button:disabled {
      opacity: 0.6;
      cursor: wait;
      transform: none;
      box-shadow: none;
    }

    .primary {
      background: linear-gradient(135deg, var(--accent), #e97f37);
      color: #fffdf9;
    }

    .secondary {
      background: linear-gradient(135deg, var(--accent-2), #1aa5a3);
      color: #f7fffe;
    }

    .ghost {
      background: rgba(31, 42, 51, 0.06);
      color: var(--ink);
    }

    .output {
      margin-top: 16px;
      padding: 16px;
      border-radius: 18px;
      background: rgba(31, 42, 51, 0.93);
      color: #f3f6f7;
      font-family: Consolas, "Cascadia Mono", monospace;
      white-space: pre-wrap;
      word-break: break-word;
      min-height: 74px;
      line-height: 1.55;
    }

    .chips {
      display: flex;
      flex-wrap: wrap;
      gap: 10px;
      margin-top: 12px;
    }

    .chip {
      display: inline-flex;
      align-items: center;
      gap: 8px;
      padding: 10px 12px;
      border-radius: 999px;
      background: rgba(31, 42, 51, 0.06);
      color: var(--muted);
      font-size: 13px;
    }

    .chip strong {
      color: var(--ink);
      font-weight: 700;
    }

    .banner {
      display: none;
      margin-top: 16px;
      padding: 14px 16px;
      border-radius: 18px;
      line-height: 1.5;
    }

    .banner.show {
      display: block;
      animation: rise 0.25s ease;
    }

    .banner.ok {
      background: rgba(24, 128, 90, 0.12);
      color: var(--success);
    }

    .banner.err {
      background: rgba(181, 63, 44, 0.12);
      color: var(--danger);
    }

    @keyframes rise {
      from {
        opacity: 0;
        transform: translateY(14px);
      }
      to {
        opacity: 1;
        transform: translateY(0);
      }
    }

    @media (max-width: 760px) {
      .shell {
        width: min(100% - 20px, 100%);
        margin-top: 18px;
      }

      .hero,
      .panel,
      .stat {
        border-radius: 24px;
      }

      h1 {
        font-size: 34px;
      }
    }
  </style>
</head>
<body>
  <div class="shell">
    <section class="hero">
      <div class="eyebrow">vrc-ytdlp visual cockpit</div>
      <h1>把 yt-dlp、VRChat 取流和本地服务装进一个更直观的界面。</h1>
      <p class="subtitle">
        这个界面保留原有命令行能力，同时把常用操作拆成可视化入口：
        生成本地播放地址、直接执行 yt-dlp、保存配置、更新工具状态。
      </p>
      <div id="banner" class="banner"></div>
      <div class="hero-grid">
        <div class="stat">
          <div class="stat-label">应用目录</div>
          <div class="stat-value" id="appDir">加载中...</div>
        </div>
        <div class="stat">
          <div class="stat-label">yt-dlp</div>
          <div class="stat-value" id="ytdlpStatus">检查中...</div>
        </div>
        <div class="stat">
          <div class="stat-label">媒体服务</div>
          <div class="stat-value" id="serverStatus">检查中...</div>
        </div>
        <div class="stat">
          <div class="stat-label">下载加速</div>
          <div class="stat-value" id="proxyStatus">检查中...</div>
        </div>
      </div>
    </section>

    <section class="panel-grid">
      <article class="panel">
        <div class="panel-kicker">核心操作</div>
        <h2>生成 VRChat 可播放地址</h2>
        <p>输入 YouTube 地址后，程序会自动检查或拉起本地媒体服务，并返回一个可直接给播放器使用的本地 URL。</p>
        <label for="streamUrl">视频地址</label>
        <input id="streamUrl" placeholder="https://www.youtube.com/watch?v=..." />
        <div class="actions">
          <button id="generateStreamBtn" class="primary">生成本地播放地址</button>
          <button id="copyStreamBtn" class="ghost">复制结果</button>
        </div>
        <div class="chips" id="streamArgs"></div>
        <div id="streamOutput" class="output">这里会显示生成后的本地播放地址。</div>
      </article>

      <article class="panel">
        <div class="panel-kicker">高级操作</div>
        <h2>直接执行 yt-dlp</h2>
        <p>适合测试直链、普通站点下载、调试参数过滤结果。附加参数请按“一行一个”填写。</p>
        <label for="execUrl">视频地址</label>
        <input id="execUrl" placeholder="https://example.com/video" />
        <label for="extraArgs">附加参数（一行一个）</label>
        <textarea id="extraArgs" placeholder="--get-url&#10;--proxy=http://127.0.0.1:7890"></textarea>
        <div class="actions">
          <button id="runExecBtn" class="secondary">执行 yt-dlp</button>
        </div>
        <div id="execOutput" class="output">这里会显示标准输出和错误输出。</div>
      </article>

      <article class="panel wide">
        <div class="panel-kicker">配置中心</div>
        <h2>运行配置</h2>
        <p>下面是当前程序使用的主要配置。保存后会直接写入同目录下的 <code>config.json</code>。</p>
        <div class="row">
          <div>
            <label for="ytdlp_location">yt-dlp 路径</label>
            <input id="ytdlp_location" />
          </div>
          <div>
            <label for="ffmpeg_location">ffmpeg 路径</label>
            <input id="ffmpeg_location" />
          </div>
          <div>
            <label for="plugin_dirs">插件目录</label>
            <input id="plugin_dirs" />
          </div>
        </div>
        <div class="row">
          <div>
            <label for="server_port">服务端口</label>
            <input id="server_port" type="number" />
          </div>
          <div>
            <label for="server_idle_timeout_secs">空闲超时（秒）</label>
            <input id="server_idle_timeout_secs" type="number" />
          </div>
          <div>
            <label for="bgutil_pot_port">bgutil-pot 端口</label>
            <input id="bgutil_pot_port" type="number" />
          </div>
        </div>
        <div class="row">
          <div>
            <label for="cache_dir">缓存目录</label>
            <input id="cache_dir" />
          </div>
          <div>
            <label for="cache_max_size_mb">缓存大小上限（MB）</label>
            <input id="cache_max_size_mb" type="number" />
          </div>
          <div>
            <label for="cache_ttl_secs">缓存保留时长（秒）</label>
            <input id="cache_ttl_secs" type="number" />
          </div>
        </div>
        <div class="row">
          <div>
            <label for="execution_timeout_secs">执行超时（秒）</label>
            <input id="execution_timeout_secs" type="number" />
          </div>
          <div>
            <label for="update_check_days">更新检查天数</label>
            <input id="update_check_days" type="number" />
          </div>
          <div>
            <label for="cookies_browser">cookies 浏览器</label>
            <input id="cookies_browser" />
          </div>
        </div>
        <div class="row">
          <div class="check">
            <input id="cookies" type="checkbox" />
            <label for="cookies" style="margin: 0;">启用 cookies</label>
          </div>
          <div>
            <label for="allowed_args">允许透传参数（一行一个）</label>
            <textarea id="allowed_args"></textarea>
          </div>
          <div>
            <label for="custom_args">默认附加参数（一行一个）</label>
            <textarea id="custom_args"></textarea>
          </div>
        </div>
        <div class="row">
          <div style="grid-column: 1 / -1;">
            <label for="extractor_args">Extractor 参数（一行一个）</label>
            <textarea id="extractor_args"></textarea>
          </div>
        </div>
        <div class="actions">
          <button id="saveConfigBtn" class="primary">保存配置</button>
          <button id="refreshBtn" class="ghost">刷新状态</button>
          <button id="updateYtdlpBtn" class="secondary">检查并更新 yt-dlp</button>
        </div>
      </article>
    </section>
  </div>

  <script>
    const banner = document.getElementById("banner");
    const streamOutput = document.getElementById("streamOutput");
    const execOutput = document.getElementById("execOutput");
    const streamArgs = document.getElementById("streamArgs");

    function showBanner(kind, text) {
      banner.className = `banner show ${kind}`;
      banner.textContent = text;
    }

    function clearBanner() {
      banner.className = "banner";
      banner.textContent = "";
    }

    function setBusy(button, busy, text) {
      if (!button.dataset.label) {
        button.dataset.label = button.textContent;
      }
      button.disabled = busy;
      button.textContent = busy ? text : button.dataset.label;
    }

    function lines(value) {
      return value
        .split(/\r?\n/)
        .map(item => item.trim())
        .filter(Boolean);
    }

    function joinLines(values) {
      return Array.isArray(values) ? values.join("\n") : "";
    }

    function fillConfig(config) {
      for (const [key, value] of Object.entries(config)) {
        const el = document.getElementById(key);
        if (!el) continue;
        if (el.type === "checkbox") {
          el.checked = Boolean(value);
        } else if (Array.isArray(value)) {
          el.value = joinLines(value);
        } else if (value === null || value === undefined) {
          el.value = "";
        } else {
          el.value = String(value);
        }
      }
    }

    function collectConfig() {
      return {
        ytdlp_location: document.getElementById("ytdlp_location").value.trim(),
        ffmpeg_location: document.getElementById("ffmpeg_location").value.trim(),
        allowed_args: lines(document.getElementById("allowed_args").value),
        custom_args: lines(document.getElementById("custom_args").value),
        cookies: document.getElementById("cookies").checked,
        cookies_browser: document.getElementById("cookies_browser").value.trim(),
        execution_timeout_secs: Number(document.getElementById("execution_timeout_secs").value || 0),
        update_check_days: Number(document.getElementById("update_check_days").value || 0),
        server_port: Number(document.getElementById("server_port").value || 0),
        server_idle_timeout_secs: Number(document.getElementById("server_idle_timeout_secs").value || 0),
        plugin_dirs: document.getElementById("plugin_dirs").value.trim() || null,
        extractor_args: lines(document.getElementById("extractor_args").value),
        bgutil_pot_port: Number(document.getElementById("bgutil_pot_port").value || 0),
        cache_dir: document.getElementById("cache_dir").value.trim(),
        cache_max_size_mb: Number(document.getElementById("cache_max_size_mb").value || 0),
        cache_ttl_secs: Number(document.getElementById("cache_ttl_secs").value || 0)
      };
    }

    async function request(url, options = {}) {
      const res = await fetch(url, {
        headers: { "Content-Type": "application/json" },
        ...options
      });
      const data = await res.json().catch(() => ({}));
      if (!res.ok) {
        throw new Error(data.error || "请求失败");
      }
      return data;
    }

    async function loadStatus() {
      const data = await request("/api/status");
      document.getElementById("appDir").textContent = data.app_dir;
      document.getElementById("ytdlpStatus").textContent = data.ytdlp_exists
        ? `就绪 · ${data.ytdlp_path}`
        : `未找到 · ${data.ytdlp_path}`;
      document.getElementById("serverStatus").textContent = data.media_server_running
        ? `运行中 · 127.0.0.1:${data.media_server_port}`
        : `待启动 · 127.0.0.1:${data.media_server_port}`;
      document.getElementById("proxyStatus").textContent = data.proxy_prefix;
      fillConfig(data.config);
      return data;
    }

    async function saveConfig() {
      const btn = document.getElementById("saveConfigBtn");
      setBusy(btn, true, "保存中...");
      clearBanner();
      try {
        const data = await request("/api/config", {
          method: "POST",
          body: JSON.stringify(collectConfig())
        });
        showBanner("ok", data.message);
        await loadStatus();
      } catch (err) {
        showBanner("err", err.message);
      } finally {
        setBusy(btn, false);
      }
    }

    async function generateStream() {
      const btn = document.getElementById("generateStreamBtn");
      setBusy(btn, true, "生成中...");
      streamOutput.textContent = "正在生成本地播放地址...";
      streamArgs.innerHTML = "";
      clearBanner();
      try {
        const data = await request("/api/stream", {
          method: "POST",
          body: JSON.stringify({ video_url: document.getElementById("streamUrl").value.trim() })
        });
        streamOutput.textContent = data.stream_url;
        data.filtered_args.forEach(arg => {
          const chip = document.createElement("span");
          chip.className = "chip";
          chip.innerHTML = `<strong>参数</strong>${arg}`;
          streamArgs.appendChild(chip);
        });
        showBanner("ok", "本地播放地址已生成。");
        await loadStatus();
      } catch (err) {
        streamOutput.textContent = err.message;
        showBanner("err", err.message);
      } finally {
        setBusy(btn, false);
      }
    }

    async function runExec() {
      const btn = document.getElementById("runExecBtn");
      setBusy(btn, true, "执行中...");
      execOutput.textContent = "正在运行 yt-dlp...";
      clearBanner();
      try {
        const data = await request("/api/execute", {
          method: "POST",
          body: JSON.stringify({
            video_url: document.getElementById("execUrl").value.trim(),
            extra_args: lines(document.getElementById("extraArgs").value)
          })
        });
        execOutput.textContent =
          `过滤后的参数:\n${data.filtered_args.join("\n")}\n\n` +
          `STDOUT:\n${data.stdout || "(empty)"}\n\nSTDERR:\n${data.stderr || "(empty)"}`;
        showBanner("ok", "yt-dlp 执行完成。");
      } catch (err) {
        execOutput.textContent = err.message;
        showBanner("err", err.message);
      } finally {
        setBusy(btn, false);
      }
    }

    async function updateYtdlp() {
      const btn = document.getElementById("updateYtdlpBtn");
      setBusy(btn, true, "检查中...");
      clearBanner();
      try {
        const data = await request("/api/tools/update-ytdlp", { method: "POST" });
        showBanner("ok", data.message);
        await loadStatus();
      } catch (err) {
        showBanner("err", err.message);
      } finally {
        setBusy(btn, false);
      }
    }

    async function copyStream() {
      const text = streamOutput.textContent.trim();
      if (!text || text.startsWith("这里会显示")) {
        showBanner("err", "还没有可复制的播放地址。");
        return;
      }
      try {
        await navigator.clipboard.writeText(text);
        showBanner("ok", "播放地址已复制到剪贴板。");
      } catch {
        showBanner("err", "复制失败，请手动复制。");
      }
    }

    document.getElementById("saveConfigBtn").addEventListener("click", saveConfig);
    document.getElementById("generateStreamBtn").addEventListener("click", generateStream);
    document.getElementById("runExecBtn").addEventListener("click", runExec);
    document.getElementById("updateYtdlpBtn").addEventListener("click", updateYtdlp);
    document.getElementById("copyStreamBtn").addEventListener("click", copyStream);
    document.getElementById("refreshBtn").addEventListener("click", async () => {
      clearBanner();
      try {
        await loadStatus();
        showBanner("ok", "状态已刷新。");
      } catch (err) {
        showBanner("err", err.message);
      }
    });

    loadStatus().catch(err => showBanner("err", err.message));
  </script>
</body>
</html>
"#;
