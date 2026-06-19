# throwback installer for Windows
#
# Install or update throwback with:
#
#   irm https://raw.githubusercontent.com/nathankellenicki/throwback/main/scripts/install.ps1 | iex
#
# Installs to ~\.throwback and adds it to your PATH.

$ErrorActionPreference = "Stop"

$Repo = "nathankellenicki/throwback"
$InstallDir = "$env:USERPROFILE\.throwback"

# ── Detect platform ──────────────────────────────

$Arch = $env:PROCESSOR_ARCHITECTURE
switch ($Arch) {
    "AMD64" { $Platform = "windows-x64" }
    default { Write-Error "Unsupported architecture: $Arch"; exit 1 }
}

# ── Fetch latest release tag ─────────────────────

Write-Host "Fetching latest release..."
$Release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
$Version = $Release.tag_name

if (-not $Version) {
    Write-Error "Failed to determine latest release version."
    exit 1
}

Write-Host "Installing throwback $Version for $Platform..."

# ── Download and extract ─────────────────────────

$Url = "https://github.com/$Repo/releases/download/$Version/throwback-$Version-$Platform.zip"
$TmpZip = Join-Path $env:TEMP "throwback-install.zip"
$TmpDir = Join-Path $env:TEMP "throwback-install"

Invoke-WebRequest -Uri $Url -OutFile $TmpZip

if (Test-Path $TmpDir) { Remove-Item $TmpDir -Recurse -Force }
Expand-Archive -Path $TmpZip -DestinationPath $TmpDir

# ── Install to ~\.throwback ──────────────────────

if (Test-Path $InstallDir) { Remove-Item $InstallDir -Recurse -Force }
Move-Item (Join-Path $TmpDir "throwback") $InstallDir

# ── Clean up temp files ──────────────────────────

Remove-Item $TmpZip -Force -ErrorAction SilentlyContinue
Remove-Item $TmpDir -Recurse -Force -ErrorAction SilentlyContinue

# ── Add to PATH if not already present ───────────

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$InstallDir;$UserPath", "User")
    Write-Host ""
    Write-Host "Added $InstallDir to your PATH."
    Write-Host "Restart your terminal for the PATH change to take effect."
}

# ── Done ─────────────────────────────────────────

Write-Host ""
Write-Host "throwback $Version installed successfully."
Write-Host ""
Write-Host "Run 'throwback' to get started."
Write-Host ""
