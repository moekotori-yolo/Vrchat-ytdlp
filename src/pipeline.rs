use std::path::{Path, PathBuf};
use std::process::Stdio;

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

/// Configuration for the pipeline tools.
#[derive(Clone, Debug)]
pub struct PipelineConfig {
    pub ytdlp_path: PathBuf,
    pub ffmpeg_path: PathBuf,
    pub ytdlp_args: Vec<String>,
    /// Directory containing yt-dlp plugins (e.g., PO token provider)
    pub plugin_dirs: Option<PathBuf>,
    /// Extra extractor args (e.g., "youtube:player-client=mweb")
    pub extractor_args: Vec<String>,
}

// ---------------------------------------------------------------------------
// Public API — Mode 1: HLS passthrough (fast)
// ---------------------------------------------------------------------------

/// Result of URL extraction.
pub struct ExtractedUrl {
    pub url: String,
    pub is_hls: bool,
}

/// Extract a streaming URL via yt-dlp --get-url. Returns the URL and whether
/// it's an HLS m3u8 stream (which VRChat's AVPro can play natively).
pub async fn extract_streaming_url(
    config: &PipelineConfig,
    video_url: &str,
    stream_id: &str,
) -> Result<ExtractedUrl> {
    let work_dir = config.ytdlp_path.parent().unwrap_or(Path::new("."));
    let path_env = augmented_path(work_dir);

    let mut args = build_common_args(config);
    args.push("-f".into());
    args.push("(mp4/best)[height<=?1080]".into());
    args.push("--get-url".into());
    args.push(video_url.into());

    tracing::info!(id = %stream_id, "extracting streaming URL");

    let mut cmd = Command::new(&config.ytdlp_path);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_ytdlp_env(&mut cmd, work_dir, &path_env);

    #[cfg(windows)]
    apply_no_window(&mut cmd);

    let output = cmd.output().await.context("running yt-dlp --get-url")?;

    let stderr_text = String::from_utf8_lossy(&output.stderr);
    log_ytdlp_stderr(stream_id, &stderr_text);

    if !output.status.success() {
        bail!("yt-dlp --get-url failed: {}", stderr_text.lines()
            .filter(|l| l.contains("ERROR"))
            .last()
            .unwrap_or("unknown error"));
    }

    let url = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();

    if url.is_empty() {
        bail!("yt-dlp --get-url returned no URL");
    }

    let is_hls = url.contains(".m3u8") || url.contains("manifest/hls");
    tracing::info!(id = %stream_id, is_hls, "extracted URL");

    Ok(ExtractedUrl { url, is_hls })
}

/// Validate an HLS m3u8 URL by fetching the manifest and checking the first
/// segment is accessible. Retries up to 7 times with 1.5s delay.
pub async fn validate_hls_url(url: &str, stream_id: &str) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    let manifest = match client.get(url).send().await {
        Ok(r) if r.status().is_success() => match r.text().await {
            Ok(t) => t,
            Err(_) => return false,
        },
        _ => return false,
    };

    let segment_url = match manifest.lines().find(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        Some(u) => u.trim().to_string(),
        None => return false,
    };

    for attempt in 1..=7 {
        match client.head(&segment_url).send().await {
            Ok(r) if r.status().is_success() => {
                tracing::info!(id = %stream_id, attempt, "HLS segment validated");
                return true;
            }
            Ok(r) => {
                tracing::debug!(id = %stream_id, attempt, status = %r.status(), "HLS segment not ready");
            }
            Err(e) => {
                tracing::debug!(id = %stream_id, attempt, error = %e, "HLS segment check failed");
            }
        }
        if attempt < 7 {
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    }

    tracing::warn!(id = %stream_id, "HLS validation failed after 7 attempts");
    false
}

// ---------------------------------------------------------------------------
// Public API — Mode 2: Download + remux (reliable fallback)
// ---------------------------------------------------------------------------

/// Download a video and remux it into a VRChat-compatible MP4 file.
///
/// 1. Downloads via yt-dlp to a temp file
/// 2. Probes codecs, remuxes/transcodes via ffmpeg to `output_path` with
///    faststart moov placement (seekable from first byte)
/// 3. Cleans up the temp file
///
/// The resulting file is a standard MP4 (h264+AAC) with proper duration and
/// Content-Length — no fragmented/live-stream behaviour.
pub async fn download_to_file(
    config: &PipelineConfig,
    video_url: &str,
    stream_id: &str,
    output_path: &Path,
) -> Result<()> {
    let work_dir = config.ytdlp_path.parent().unwrap_or(Path::new("."));
    let tmp_dir = work_dir.join("tmp");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let path_env = augmented_path(work_dir);

    let dl_tmp = tmp_dir.join(format!("dl_{stream_id}.mp4"));

    // --- Step 1: Download via yt-dlp ---
    download_with_retries(config, video_url, stream_id, &dl_tmp, work_dir, &path_env).await?;

    // --- Step 2: Probe and remux via ffmpeg ---
    let (needs_transcode_video, needs_transcode_audio) =
        probe_codecs(&config.ffmpeg_path, &dl_tmp).await;

    remux_to_file(
        &config.ffmpeg_path,
        &dl_tmp,
        output_path,
        needs_transcode_video,
        needs_transcode_audio,
        stream_id,
    )
    .await?;

    // --- Step 3: Cleanup ---
    let _ = std::fs::remove_file(&dl_tmp);

    let size = std::fs::metadata(output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!(id = %stream_id, size, "download and remux complete");

    Ok(())
}

/// Download a video via yt-dlp with retry logic for transient errors.
async fn download_with_retries(
    config: &PipelineConfig,
    video_url: &str,
    stream_id: &str,
    output: &Path,
    work_dir: &Path,
    path_env: &std::ffi::OsStr,
) -> Result<()> {
    let ytdlp_args = build_file_args(config, video_url, output);
    let max_retries = 3;
    let mut last_error = String::new();

    for attempt in 1..=max_retries {
        if attempt > 1 {
            let delay = std::time::Duration::from_secs(2u64.pow(attempt as u32 - 1));
            tracing::info!(id = %stream_id, attempt, delay_secs = delay.as_secs(), "retrying download");
            tokio::time::sleep(delay).await;
        }

        tracing::info!(id = %stream_id, attempt, "downloading via yt-dlp");

        let _ = std::fs::remove_file(output);

        let mut cmd = Command::new(&config.ytdlp_path);
        cmd.args(&ytdlp_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_ytdlp_env(&mut cmd, work_dir, path_env);

        #[cfg(windows)]
        apply_no_window(&mut cmd);

        let result = cmd.output().await.context("running yt-dlp download")?;
        let stderr_text = String::from_utf8_lossy(&result.stderr);
        log_ytdlp_stderr(stream_id, &stderr_text);

        if result.status.success() && output.exists() {
            let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
            if size > 0 {
                tracing::info!(id = %stream_id, size, attempt, "yt-dlp download complete");
                return Ok(());
            }
        }

        last_error = stderr_text
            .lines()
            .filter(|l| l.contains("ERROR"))
            .last()
            .unwrap_or("unknown error")
            .to_string();

        let is_transient = stderr_text.contains("403")
            || stderr_text.contains("timed out")
            || stderr_text.contains("Connection reset");

        if !is_transient {
            tracing::warn!(id = %stream_id, "non-transient error, skipping retries");
            break;
        }

        tracing::warn!(id = %stream_id, attempt, "download failed (transient), will retry");
    }

    let _ = std::fs::remove_file(output);
    bail!("yt-dlp download failed after {max_retries} attempts: {last_error}");
}

/// Remux (or transcode) to a standard MP4 with moov atom at the front.
async fn remux_to_file(
    ffmpeg_path: &Path,
    input: &Path,
    output: &Path,
    needs_transcode_video: bool,
    needs_transcode_audio: bool,
    stream_id: &str,
) -> Result<()> {
    tracing::info!(id = %stream_id, "remuxing to VRChat-compatible MP4");

    let mut cmd = Command::new(ffmpeg_path);
    cmd.args(["-hide_banner", "-loglevel", "warning", "-y"]);
    cmd.args(["-i", &input.to_string_lossy()]);

    if needs_transcode_video {
        tracing::info!(id = %stream_id, "transcoding video to h264");
        cmd.args(["-c:v", "libx264", "-preset", "fast", "-crf", "23"]);
    } else {
        cmd.args(["-c:v", "copy"]);
    }

    if needs_transcode_audio {
        tracing::info!(id = %stream_id, "transcoding audio to aac");
        cmd.args(["-c:a", "aac", "-b:a", "192k"]);
    } else {
        cmd.args(["-c:a", "copy"]);
    }

    cmd.args(["-bsf:a", "aac_adtstoasc"]);
    cmd.args(["-movflags", "+faststart"]);
    cmd.args(["-f", "mp4"]);
    cmd.arg(&output.to_string_lossy().to_string());
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    apply_no_window(&mut cmd);

    let result = cmd.output().await.context("running ffmpeg remux")?;

    let stderr_text = String::from_utf8_lossy(&result.stderr);
    for line in stderr_text.lines() {
        let line = line.trim();
        if !line.is_empty() {
            tracing::warn!(id = %stream_id, process = "ffmpeg", "{}", line);
        }
    }

    if !result.status.success() {
        bail!("ffmpeg remux failed with {}", result.status);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// yt-dlp argument builders
// ---------------------------------------------------------------------------

/// Build the common yt-dlp arguments shared by both strategies.
fn build_common_args(config: &PipelineConfig) -> Vec<String> {
    let mut args = Vec::new();

    if let Some(ref plugin_dir) = config.plugin_dirs {
        args.push("--plugin-dirs".into());
        args.push(plugin_dir.to_string_lossy().to_string());
    }

    for ea in &config.extractor_args {
        args.push("--extractor-args".into());
        args.push(ea.clone());
    }

    let mut skip_next = false;
    for arg in &config.ytdlp_args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--get-url" {
            continue;
        }
        if arg == "-f" || arg == "--format" {
            skip_next = true;
            continue;
        }
        if arg.starts_with("http://") || arg.starts_with("https://") {
            continue;
        }
        args.push(arg.clone());
    }

    // Tell yt-dlp where ffmpeg is (for internal merging of split formats)
    let ffmpeg_dir = config
        .ffmpeg_path
        .parent()
        .unwrap_or(Path::new("."))
        .to_string_lossy()
        .to_string();
    args.push("--ffmpeg-location".into());
    args.push(ffmpeg_dir);

    args
}

/// Build args for downloading to a temp file. yt-dlp merges split formats
/// internally via ffmpeg before writing.
fn build_file_args(config: &PipelineConfig, video_url: &str, output_path: &Path) -> Vec<String> {
    let mut args = build_common_args(config);

    // Same format selector as pipe but also allows direct HTTPS as last resort
    args.push("-f".into());
    args.push(
        "b[height<=1080][protocol^=m3u8][vcodec^=avc]\
         /b[height<=1080][protocol^=m3u8]\
         /b[height<=1080][vcodec^=avc][acodec^=mp4a]\
         /bv[vcodec^=avc][height<=1080]+ba[acodec^=mp4a]\
         /bv[height<=1080]+ba\
         /b[height<=1080]\
         /b"
        .into(),
    );

    args.push("--merge-output-format".into());
    args.push("mp4".into());

    args.push("-o".into());
    args.push(output_path.to_string_lossy().to_string());

    args.push(video_url.into());
    args
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_mp4_video_codec(codec: &str) -> bool {
    let c = codec.to_lowercase();
    c.is_empty()
        || c == "none"
        || c == "unknown"
        || c.starts_with("avc")
        || c.starts_with("h264")
        || c.starts_with("h.264")
        || c.starts_with("hevc")
        || c.starts_with("h265")
        || c.starts_with("h.265")
        || c.starts_with("mp4v")
}

fn is_mp4_audio_codec(codec: &str) -> bool {
    let c = codec.to_lowercase();
    c.is_empty()
        || c == "none"
        || c == "unknown"
        || c.starts_with("mp4a")
        || c.starts_with("aac")
        || c == "mp3"
}

async fn probe_codecs(ffmpeg_path: &Path, file: &Path) -> (bool, bool) {
    let ffprobe_path = ffmpeg_path.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });

    let mut cmd = Command::new(&ffprobe_path);
    cmd.args([
            "-v",
            "quiet",
            "-show_entries",
            "stream=codec_name,codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    #[cfg(windows)]
    apply_no_window(&mut cmd);

    let output = cmd.output().await;

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => {
            tracing::warn!("ffprobe failed, assuming transcoding needed");
            return (true, true);
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut vcodec = String::new();
    let mut acodec = String::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() >= 2 {
            match parts[1] {
                "video" => vcodec = parts[0].to_string(),
                "audio" => acodec = parts[0].to_string(),
                _ => {}
            }
        }
    }

    let needs_v = !is_mp4_video_codec(&vcodec);
    let needs_a = !is_mp4_audio_codec(&acodec);

    if needs_v {
        tracing::info!(vcodec = %vcodec, "video needs transcoding to h264");
    }
    if needs_a {
        tracing::info!(acodec = %acodec, "audio needs transcoding to aac");
    }

    (needs_v, needs_a)
}

fn log_ytdlp_stderr(stream_id: &str, stderr_text: &str) {
    for line in stderr_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains("ERROR") || line.contains("error") {
            tracing::error!(id = %stream_id, process = "yt-dlp", "{}", line);
        } else if line.contains("WARNING") || line.contains("warning") {
            tracing::warn!(id = %stream_id, process = "yt-dlp", "{}", line);
        } else {
            tracing::debug!(id = %stream_id, process = "yt-dlp", "{}", line);
        }
    }
}

fn augmented_path(tool_dir: &Path) -> std::ffi::OsString {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut result = tool_dir.as_os_str().to_owned();
    if !current.is_empty() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        result.push(sep);
        result.push(current);
    }
    result
}

/// Apply common environment setup for spawning yt-dlp.
/// Clears PyInstaller variables that can cause crashes when yt-dlp.exe
/// (a PyInstaller bundle) is spawned as a child process.
fn apply_ytdlp_env(cmd: &mut Command, work_dir: &Path, path_env: &std::ffi::OsStr) {
    // Create a tmp dir next to yt-dlp and set TEMP/TMP to it.
    // PyInstaller checks TMP first, then TEMP — both must be set.
    // The directory MUST exist before yt-dlp starts.
    let tmp_dir = work_dir.join("tmp");
    match std::fs::create_dir_all(&tmp_dir) {
        Ok(_) => tracing::debug!(path = %tmp_dir.display(), "created temp dir for yt-dlp"),
        Err(e) => tracing::error!(path = %tmp_dir.display(), error = %e, "failed to create temp dir"),
    }

    cmd.current_dir(work_dir)
        .env("PATH", path_env)
        .env("TEMP", &tmp_dir)
        .env("TMP", &tmp_dir);
}

/// Apply CREATE_NO_WINDOW on Windows for ffmpeg/ffprobe processes.
#[cfg(windows)]
fn apply_no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

