use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::server::{self, ServerConfig};

// ---------------------------------------------------------------------------
// Top-level coordinator
// ---------------------------------------------------------------------------

/// Run the media server with all supporting services managed automatically.
///
/// This is the single entry point for `--serve` mode. It:
/// 1. Spawns bgutil-pot as a child (if binary found)
/// 2. Registers a signal handler for clean SIGTERM/SIGINT shutdown
/// 3. Runs the HTTP media server with graceful shutdown
/// 4. Tears down bgutil-pot on exit
///
/// Multiple CLI invocations can register streams concurrently — they all
/// talk to the same server. The idle timeout shuts everything down when
/// there are no active streams and no recent activity.
pub async fn run_managed_server(
    port: u16,
    idle_timeout_secs: u64,
    server_config: ServerConfig,
    bgutil_pot_path: Option<PathBuf>,
    bgutil_pot_port: u16,
) -> Result<()> {
    // On Windows, create a Job Object so all child processes (bgutil-pot,
    // yt-dlp, ffmpeg) are automatically killed when our process exits.
    #[cfg(windows)]
    let _job = create_job_object().ok();

    let shutdown = CancellationToken::new();

    // 1. Spawn bgutil-pot if available
    let mut bgutil = match bgutil_pot_path {
        Some(ref path) if path.exists() => {
            match BgutilPot::spawn(path, "127.0.0.1", bgutil_pot_port).await {
                Ok(pot) => {
                    tracing::info!("bgutil-pot started on port {bgutil_pot_port}");
                    Some(pot)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to start bgutil-pot, continuing without PO tokens");
                    None
                }
            }
        }
        _ => {
            tracing::debug!("bgutil-pot binary not found, PO tokens unavailable");
            None
        }
    };

    // 2. Monitor bgutil-pot in background (restart on crash)
    if let (Some(ref mut pot), Some(ref path)) = (&mut bgutil, &bgutil_pot_path) {
        let token = shutdown.clone();
        let child_id = pot.take_child();
        tokio::spawn(monitor_bgutil_pot(child_id, path.clone(), bgutil_pot_port, token));
    }

    // 3. Signal handler: SIGTERM/SIGINT → cancel token
    let signal_token = shutdown.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();

        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = signal(SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = ctrl_c => tracing::info!("received SIGINT, shutting down"),
                _ = sigterm.recv() => tracing::info!("received SIGTERM, shutting down"),
            }
        }

        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
            tracing::info!("received Ctrl+C, shutting down");
        }

        signal_token.cancel();
    });

    // 4. Run HTTP server (blocks until shutdown)
    let result = server::run_server(port, idle_timeout_secs, server_config, shutdown.clone()).await;

    // 5. Cleanup
    tracing::info!("server stopped, cleaning up");
    shutdown.cancel(); // ensure all background tasks exit

    // Give bgutil-pot a moment to die, then force-kill
    tokio::time::sleep(Duration::from_millis(500)).await;

    Ok(result.unwrap_or(()))
}

// ---------------------------------------------------------------------------
// bgutil-pot process manager
// ---------------------------------------------------------------------------

struct BgutilPot {
    child: Option<Child>,
}

impl BgutilPot {
    /// Spawn bgutil-pot server and wait for it to become healthy.
    async fn spawn(path: &Path, host: &str, port: u16) -> Result<Self> {
        let child = spawn_bgutil_pot(path, host, port).await?;
        Ok(Self { child: Some(child) })
    }

    /// Take ownership of the child process (for the monitor task).
    fn take_child(&mut self) -> Child {
        self.child.take().expect("child already taken")
    }
}

async fn spawn_bgutil_pot(path: &Path, host: &str, port: u16) -> Result<Child> {
    tracing::debug!(path = %path.display(), host, port, "spawning bgutil-pot");

    let mut cmd = Command::new(path);
    cmd.args(["server", "--host", host, "--port", &port.to_string()]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // On Windows, prevent a console window from appearing
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn().context("spawning bgutil-pot")?;

    // Forward stderr to tracing
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if !line.is_empty() {
                    tracing::debug!(process = "bgutil-pot", "{}", line);
                }
            }
        });
    }

    // Forward stdout too (bgutil-pot logs to stdout)
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if !line.is_empty() {
                    tracing::debug!(process = "bgutil-pot", "{}", line);
                }
            }
        });
    }

    // Wait for health check
    let health_url = format!("http://{host}:{port}/ping");
    let mut healthy = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if reqwest::get(&health_url)
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            healthy = true;
            break;
        }
    }

    if !healthy {
        let _ = child.kill().await;
        anyhow::bail!("bgutil-pot failed to become healthy within 5 seconds");
    }

    Ok(child)
}

/// Monitor bgutil-pot and restart it if it crashes.
async fn monitor_bgutil_pot(
    mut child: Child,
    path: PathBuf,
    port: u16,
    shutdown: CancellationToken,
) {
    let mut restart_count = 0u32;

    loop {
        tokio::select! {
            status = child.wait() => {
                if shutdown.is_cancelled() {
                    return;
                }

                match status {
                    Ok(s) => tracing::warn!(status = %s, "bgutil-pot exited unexpectedly"),
                    Err(e) => tracing::error!(error = %e, "bgutil-pot wait failed"),
                }

                restart_count += 1;
                if restart_count > 5 {
                    tracing::error!("bgutil-pot crashed too many times, giving up");
                    return;
                }

                let delay = Duration::from_secs(2u64.pow(restart_count.min(4)));
                tracing::info!(delay_secs = delay.as_secs(), attempt = restart_count, "restarting bgutil-pot");
                tokio::time::sleep(delay).await;

                if shutdown.is_cancelled() {
                    return;
                }

                match spawn_bgutil_pot(&path, "127.0.0.1", port).await {
                    Ok(new_child) => {
                        child = new_child;
                        tracing::info!("bgutil-pot restarted successfully");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to restart bgutil-pot");
                        return;
                    }
                }
            }
            _ = shutdown.cancelled() => {
                tracing::info!("shutting down bgutil-pot");
                let _ = child.kill().await;
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Windows Job Object — ensures all child processes die with the parent
// ---------------------------------------------------------------------------

#[cfg(windows)]
struct WinJobObject(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for WinJobObject {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Create a Windows Job Object and assign the current process to it.
/// All child processes inherit the job and are killed when this process exits.
#[cfg(windows)]
fn create_job_object() -> Result<WinJobObject> {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            anyhow::bail!("CreateJobObjectW failed");
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            windows_sys::Win32::Foundation::CloseHandle(job);
            anyhow::bail!("SetInformationJobObject failed");
        }

        // Assign current process to the job
        let current_process = windows_sys::Win32::System::Threading::GetCurrentProcess();
        if AssignProcessToJobObject(job, current_process) == 0 {
            // This can fail if the process is already in a job (e.g. from a debugger).
            // Non-fatal — just log and continue without job-based cleanup.
            windows_sys::Win32::Foundation::CloseHandle(job);
            tracing::debug!("could not assign current process to job object (already in a job?)");
            anyhow::bail!("AssignProcessToJobObject failed");
        }

        tracing::debug!("created job object for child process cleanup");
        Ok(WinJobObject(job))
    }
}
