<#
.SYNOPSIS
    Build and package vrc-ytdlp for release testing.

.DESCRIPTION
    1. Builds the Rust release binary
    2. Downloads yt-dlp, ffmpeg, and ffprobe
    3. Packages everything into a ready-to-run zip

.EXAMPLE
    .\build.ps1              # build + package
    .\build.ps1 -SkipBuild   # package only (reuse existing binary)
#>

param(
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$ScriptDir  = Split-Path -Parent $MyInvocation.MyCommand.Path
$ToolsDir   = Join-Path $ScriptDir "tools"
$StageDir   = Join-Path $ScriptDir "dist"
$Exe        = Join-Path $ScriptDir "target\release\vrc-ytdlp.exe"

$YtDlpAsset = "yt-dlp_x86.exe"
$GithubApi  = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest"
$FfmpegUrl  = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip"
$PluginRepo = "Brainicism/bgutil-ytdlp-pot-provider"
$GhProxy    = "https://gh-proxy.org/"

# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------

function Write-Step($msg) { Write-Host "`n:: $msg" -ForegroundColor Cyan }
function Write-Ok($msg)   { Write-Host "   $msg" -ForegroundColor Green }
function Write-Skip($msg) { Write-Host "   $msg" -ForegroundColor DarkGray }
function Use-ProxyUrl($url) { "$GhProxy$url" }

# --------------------------------------------------------------------------
# 1. Build
# --------------------------------------------------------------------------

if (-not $SkipBuild) {
    Write-Step "Building release binary"
    Push-Location $ScriptDir
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    Pop-Location
    Write-Ok "Built: $Exe"
} else {
    if (-not (Test-Path $Exe)) { throw "Binary not found at $Exe - run without -SkipBuild first" }
    Write-Skip "Skipping build (using existing binary)"
}

# --------------------------------------------------------------------------
# 2. Download dependencies into tools/
# --------------------------------------------------------------------------

if (-not (Test-Path $ToolsDir)) { New-Item -ItemType Directory -Path $ToolsDir | Out-Null }

# --- yt-dlp ---

$YtDlpPath   = Join-Path $ToolsDir "yt-dlp.exe"
$VersionFile = Join-Path $ToolsDir "version.txt"

Write-Step "yt-dlp"
$release = Invoke-RestMethod -Uri (Use-ProxyUrl $GithubApi) -Headers @{ "User-Agent" = "VRC-YtDlp-Build" }
$tag     = $release.tag_name
$asset   = $release.assets | Where-Object { $_.name -eq $YtDlpAsset }
if (-not $asset) { throw "Asset $YtDlpAsset not found in release $tag" }

$currentVer = if (Test-Path $VersionFile) { (Get-Content $VersionFile).Trim() } else { "" }

if ((Test-Path $YtDlpPath) -and ($currentVer -eq $tag)) {
    Write-Ok "Up to date ($tag)"
} else {
    Write-Host "   Downloading $tag..." -ForegroundColor Yellow
    $tmp = Join-Path $ToolsDir ".yt-dlp.tmp"
    Invoke-WebRequest -Uri (Use-ProxyUrl $asset.browser_download_url) -OutFile $tmp -UseBasicParsing
    if (Test-Path $YtDlpPath) { Remove-Item $YtDlpPath -Force }
    Move-Item $tmp $YtDlpPath
    Set-Content -Path $VersionFile -Value $tag
    Write-Ok "Installed $tag"
}

# --- ffmpeg + ffprobe ---

$FfmpegPath  = Join-Path $ToolsDir "ffmpeg.exe"
$FfprobePath = Join-Path $ToolsDir "ffprobe.exe"

Write-Step "ffmpeg"
if ((Test-Path $FfmpegPath) -and (Test-Path $FfprobePath)) {
    Write-Ok "Already installed"
} else {
    Write-Host "   Downloading..." -ForegroundColor Yellow
    $zip = Join-Path $ToolsDir "ffmpeg.zip"
    Invoke-WebRequest -Uri $FfmpegUrl -OutFile $zip -UseBasicParsing

    $extractDir = Join-Path $ToolsDir "_extract"
    if (Test-Path $extractDir) { Remove-Item $extractDir -Recurse -Force }
    Expand-Archive -Path $zip -DestinationPath $extractDir

    $binDir = Get-ChildItem -Path $extractDir -Recurse -Directory -Filter "bin" | Select-Object -First 1
    if (-not $binDir) { throw "bin/ not found in ffmpeg archive" }

    Copy-Item (Join-Path $binDir.FullName "ffmpeg.exe")  $FfmpegPath  -Force
    Copy-Item (Join-Path $binDir.FullName "ffprobe.exe") $FfprobePath -Force

    Remove-Item $zip -Force
    Remove-Item $extractDir -Recurse -Force
    Write-Ok "Installed"
}

# --- yt-dlp plugins (bgutil-pot PO token provider) ---

$PluginDir = Join-Path $ToolsDir "yt-dlp-plugins"

Write-Step "yt-dlp plugins"
if ((Test-Path $PluginDir) -and (Get-ChildItem $PluginDir -Recurse -Filter "*.py").Count -gt 0) {
    Write-Ok "Already installed"
} else {
    Write-Host "   Downloading bgutil-pot plugins..." -ForegroundColor Yellow
    $pluginZip = Join-Path $ToolsDir "bgutil-plugins.zip"
    $pluginApi = "https://api.github.com/repos/$PluginRepo/releases/latest"
    $pluginRelease = Invoke-RestMethod -Uri (Use-ProxyUrl $pluginApi) -Headers @{ "User-Agent" = "VRC-YtDlp-Build" }
    $pluginAsset = $pluginRelease.assets | Where-Object { $_.name -match "\.zip$" } | Select-Object -First 1
    if (-not $pluginAsset) { throw "No zip asset found in $PluginRepo release" }

    Invoke-WebRequest -Uri (Use-ProxyUrl $pluginAsset.browser_download_url) -OutFile $pluginZip -UseBasicParsing

    $pluginExtract = Join-Path $ToolsDir "_plugin_extract"
    if (Test-Path $pluginExtract) { Remove-Item $pluginExtract -Recurse -Force }
    Expand-Archive -Path $pluginZip -DestinationPath $pluginExtract

    if (-not (Test-Path $PluginDir)) { New-Item -ItemType Directory -Path $PluginDir | Out-Null }

    # Preserve yt_dlp_plugins/extractor/ structure expected by yt-dlp
    $pluginSrc = Get-ChildItem -Path $pluginExtract -Directory -Filter "yt_dlp_plugins" -Recurse | Select-Object -First 1
    if ($pluginSrc) {
        Copy-Item $pluginSrc.FullName $PluginDir -Recurse -Force
    } else {
        # Fallback: copy py files directly
        Get-ChildItem -Path $pluginExtract -Recurse -Filter "*.py" | ForEach-Object {
            Copy-Item $_.FullName $PluginDir -Force
        }
    }

    Remove-Item $pluginZip -Force
    Remove-Item $pluginExtract -Recurse -Force

    $count = (Get-ChildItem $PluginDir -Recurse -Filter "*.py").Count
    Write-Ok "Installed $count plugins"
}

# --------------------------------------------------------------------------
# 3. Read version from Cargo.toml
# --------------------------------------------------------------------------

$cargoToml = Get-Content (Join-Path $ScriptDir "Cargo.toml") -Raw
$pattern   = 'version\s*=\s*"([^"]+)"'
$verMatch  = [regex]::Match($cargoToml, $pattern)
$version   = if ($verMatch.Success) { $verMatch.Groups[1].Value } else { "unknown" }

# --------------------------------------------------------------------------
# 4. Stage and package
# --------------------------------------------------------------------------

Write-Step "Packaging vrc-ytdlp v$version"

if (Test-Path $StageDir) { Remove-Item $StageDir -Recurse -Force }
$pkgDir = Join-Path $StageDir "vrc-ytdlp"
New-Item -ItemType Directory -Path $pkgDir | Out-Null
$pkgToolsDir = Join-Path $pkgDir "tools"
New-Item -ItemType Directory -Path $pkgToolsDir | Out-Null

# Copy binary
Copy-Item $Exe $pkgDir

# Copy tools
Copy-Item $YtDlpPath   $pkgToolsDir
Copy-Item $FfmpegPath  $pkgToolsDir
Copy-Item $FfprobePath $pkgToolsDir

# Copy yt-dlp plugins (preserving yt_dlp_plugins/extractor/ structure)
if (Test-Path $PluginDir) {
    Copy-Item $PluginDir (Join-Path $pkgToolsDir "yt-dlp-plugins") -Recurse
}

# Copy setup script (for future updates)
Copy-Item (Join-Path $ScriptDir "setup.ps1") $pkgDir

# Create zip
$zipName = "vrc-ytdlp-v$version.zip"
$zipPath = Join-Path $StageDir $zipName
Compress-Archive -Path $pkgDir -DestinationPath $zipPath -Force

# --------------------------------------------------------------------------
# 5. Summary
# --------------------------------------------------------------------------

$zipSize = [math]::Round((Get-Item $zipPath).Length / 1MB, 1)

Write-Step "Done"
Write-Host ""
Write-Host "   Package: dist\$zipName ($zipSize MB)" -ForegroundColor White
Write-Host ""
Write-Host "   Contents:" -ForegroundColor DarkGray

Get-ChildItem $pkgDir -Recurse -File | ForEach-Object {
    $rel = $_.FullName.Substring($pkgDir.Length + 1) -replace '\\','/'
    Write-Host ("     {0,-30} {1,10:N0} KB" -f $rel, ($_.Length / 1KB)) -ForegroundColor DarkGray
}

Write-Host ""
Write-Host "   To test: unzip dist\$zipName, then run vrc-ytdlp.exe" -ForegroundColor Yellow
Write-Host ""
