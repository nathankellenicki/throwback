$ErrorActionPreference = "Stop"

# Get version from Cargo.toml
$version = (Select-String -Path "Cargo.toml" -Pattern '^version' | Select-Object -First 1).Line -replace '.*"(.*)".*', '$1'

$arch = if ([System.Environment]::Is64BitOperatingSystem) { "x64" } else { "x86" }
$zipName = "throwback-v${version}-windows-${arch}.zip"

Write-Host "Building Throwback v${version} for windows-${arch}..."
cargo build --release --bin throwback
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host "Generating THIRD_PARTY_NOTICES.md..."
if (-not (Get-Command cargo-about -ErrorAction SilentlyContinue)) {
    Write-Host "cargo-about not found, installing..."
    cargo install cargo-about --locked
    if ($LASTEXITCODE -ne 0) { exit 1 }
}
cargo about generate about.hbs -o THIRD_PARTY_NOTICES.md
if ($LASTEXITCODE -ne 0) { exit 1 }

Write-Host "Creating ${zipName}..."

$staging = Join-Path $env:TEMP "throwback-release"
if (Test-Path $staging) { Remove-Item $staging -Recurse -Force }
New-Item -ItemType Directory -Path "$staging\throwback" | Out-Null

Copy-Item "target\release\throwback.exe" "$staging\throwback\"
Copy-Item "README.md" "$staging\throwback\"
Copy-Item "LICENSE" "$staging\throwback\"
Copy-Item "THIRD_PARTY_NOTICES.md" "$staging\throwback\"

if (-not (Test-Path "releases")) { New-Item -ItemType Directory -Path "releases" | Out-Null }
$zipPath = Join-Path "releases" $zipName
if (Test-Path $zipPath) { Remove-Item $zipPath }
Compress-Archive -Path "$staging\throwback" -DestinationPath $zipPath

Remove-Item $staging -Recurse -Force

Write-Host "Done: releases\${zipName}"
