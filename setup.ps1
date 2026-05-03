<#
.SYNOPSIS
    Downloads all required dependencies for vrc-ytdlp.

.DESCRIPTION
    Downloads yt-dlp and ffmpeg (including ffprobe) into the tools/ directory
    next to this script. Run this once before using vrc-ytdlp, or re-run to
    update to the latest versions.

.EXAMPLE
    .\setup.ps1
    .\setup.ps1 -Force   # re-download even if files already exist
#>

param(
    [switch]$Force
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ToolsDir  = Join-Path $ScriptDir "tools"

if (-not (Test-Path $ToolsDir)) {
    New-Item -ItemType Directory -Path $ToolsDir | Out-Null
}

# --- yt-dlp ---

$YtDlpPath    = Join-Path $ToolsDir "yt-dlp.exe"
$VersionFile  = Join-Path $ToolsDir "version.txt"
$YtDlpAsset   = "yt-dlp_x86.exe"
$GithubApi    = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest"
$GhProxy      = "https://gh-proxy.org/"

function Use-ProxyUrl($url) {
    return "$GhProxy$url"
}

function Get-YtDlp {
    Write-Host "[yt-dlp] Checking latest release..." -ForegroundColor Cyan
    $release = Invoke-RestMethod -Uri (Use-ProxyUrl $GithubApi) -Headers @{ "User-Agent" = "VRC-YtDlp-Setup" }
    $tag     = $release.tag_name
    $asset   = $release.assets | Where-Object { $_.name -eq $YtDlpAsset }

    if (-not $asset) {
        Write-Error "Could not find $YtDlpAsset in release $tag"
        return
    }

    $currentVersion = if (Test-Path $VersionFile) { (Get-Content $VersionFile).Trim() } else { "" }

    if ((-not $Force) -and (Test-Path $YtDlpPath) -and ($currentVersion -eq $tag)) {
        Write-Host "[yt-dlp] Already up to date ($tag)" -ForegroundColor Green
        return
    }

    Write-Host "[yt-dlp] Downloading $tag..." -ForegroundColor Yellow
    $tmp = Join-Path $ToolsDir ".yt-dlp-download.tmp"
    Invoke-WebRequest -Uri (Use-ProxyUrl $asset.browser_download_url) -OutFile $tmp -UseBasicParsing

    if (Test-Path $YtDlpPath) { Remove-Item $YtDlpPath -Force }
    Move-Item $tmp $YtDlpPath

    Set-Content -Path $VersionFile -Value $tag
    Write-Host "[yt-dlp] Installed $tag" -ForegroundColor Green
}

# --- ffmpeg + ffprobe ---

$FfmpegPath  = Join-Path $ToolsDir "ffmpeg.exe"
$FfprobePath = Join-Path $ToolsDir "ffprobe.exe"
$FfmpegUrl   = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip"

function Get-Ffmpeg {
    if ((-not $Force) -and (Test-Path $FfmpegPath) -and (Test-Path $FfprobePath)) {
        Write-Host "[ffmpeg] Already installed" -ForegroundColor Green
        return
    }

    Write-Host "[ffmpeg] Downloading release essentials..." -ForegroundColor Yellow
    $zip = Join-Path $ToolsDir "ffmpeg.zip"
    Invoke-WebRequest -Uri $FfmpegUrl -OutFile $zip -UseBasicParsing

    Write-Host "[ffmpeg] Extracting..." -ForegroundColor Yellow
    $extractDir = Join-Path $ToolsDir "_ffmpeg_extract"
    if (Test-Path $extractDir) { Remove-Item $extractDir -Recurse -Force }
    Expand-Archive -Path $zip -DestinationPath $extractDir

    # The zip contains a single top-level directory with bin/ inside
    $binDir = Get-ChildItem -Path $extractDir -Recurse -Directory -Filter "bin" | Select-Object -First 1

    if (-not $binDir) {
        Remove-Item $zip -Force
        Remove-Item $extractDir -Recurse -Force
        Write-Error "Could not find bin/ directory in ffmpeg archive"
        return
    }

    Copy-Item (Join-Path $binDir.FullName "ffmpeg.exe")  $FfmpegPath  -Force
    Copy-Item (Join-Path $binDir.FullName "ffprobe.exe") $FfprobePath -Force

    Remove-Item $zip -Force
    Remove-Item $extractDir -Recurse -Force

    $version = & $FfmpegPath -version 2>&1 | Select-Object -First 1
    Write-Host "[ffmpeg] Installed ($version)" -ForegroundColor Green
}

# --- yt-dlp plugins (bgutil-pot PO token provider) ---

$PluginDir  = Join-Path $ToolsDir "yt-dlp-plugins"
$PluginRepo = "Brainicism/bgutil-ytdlp-pot-provider"

function Get-Plugins {
    if ((-not $Force) -and (Test-Path $PluginDir) -and ((Get-ChildItem $PluginDir -Recurse -Filter "*.py").Count -gt 0)) {
        Write-Host "[plugins] Already installed" -ForegroundColor Green
        return
    }

    Write-Host "[plugins] Downloading bgutil-pot plugins..." -ForegroundColor Yellow
    $pluginApi = "https://api.github.com/repos/$PluginRepo/releases/latest"
    $pluginRelease = Invoke-RestMethod -Uri (Use-ProxyUrl $pluginApi) -Headers @{ "User-Agent" = "VRC-YtDlp-Setup" }
    $pluginAsset = $pluginRelease.assets | Where-Object { $_.name -match "\.zip$" } | Select-Object -First 1
    if (-not $pluginAsset) {
        Write-Host "[plugins] No zip asset found, skipping" -ForegroundColor Yellow
        return
    }

    $zip = Join-Path $ToolsDir "plugins.zip"
    Invoke-WebRequest -Uri (Use-ProxyUrl $pluginAsset.browser_download_url) -OutFile $zip -UseBasicParsing

    $extractDir = Join-Path $ToolsDir "_plugin_extract"
    if (Test-Path $extractDir) { Remove-Item $extractDir -Recurse -Force }
    Expand-Archive -Path $zip -DestinationPath $extractDir

    if (-not (Test-Path $PluginDir)) { New-Item -ItemType Directory -Path $PluginDir | Out-Null }

    # Preserve yt_dlp_plugins/extractor/ structure expected by yt-dlp
    $pluginSrc = Get-ChildItem -Path $extractDir -Directory -Filter "yt_dlp_plugins" -Recurse | Select-Object -First 1
    if ($pluginSrc) {
        Copy-Item $pluginSrc.FullName $PluginDir -Recurse -Force
    } else {
        Get-ChildItem -Path $extractDir -Recurse -Filter "*.py" | ForEach-Object {
            Copy-Item $_.FullName $PluginDir -Force
        }
    }

    Remove-Item $zip -Force
    Remove-Item $extractDir -Recurse -Force

    $count = (Get-ChildItem $PluginDir -Recurse -Filter "*.py").Count
    Write-Host "[plugins] Installed $count plugins" -ForegroundColor Green
}

# --- Run ---

Write-Host ""
Write-Host "vrc-ytdlp dependency setup" -ForegroundColor White
Write-Host "Target: $ToolsDir" -ForegroundColor DarkGray
Write-Host ""

Get-YtDlp
Get-Ffmpeg
Get-Plugins

Write-Host ""
Write-Host "Done. Tools installed to: $ToolsDir" -ForegroundColor White
Write-Host ""

# List what we have
Get-ChildItem $ToolsDir -Recurse -File | ForEach-Object {
    $rel = $_.FullName.Substring($ToolsDir.Length + 1) -replace '\\','/'
    Write-Host ("  {0,-40} {1,10:N0} KB" -f $rel, ($_.Length / 1KB)) -ForegroundColor DarkGray
}
Write-Host ""
