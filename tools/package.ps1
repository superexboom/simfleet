# Build and zip a release: dist\simfleet-<version>-windows-x64.zip
# (simfleet.exe, README.md, LICENSE, NOTICE.md, THIRD-PARTY-LICENSES.md).
#
# The native engine is NOT included: it embeds WFSim's data/, part of which is
# Digital Extremes material (see NOTICE.md). -WithNativeEngine adds it for a
# private copy only.
param([switch]$WithNativeEngine)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
cargo build --release --manifest-path (Join-Path $root "Cargo.toml")
if ($LASTEXITCODE -ne 0) { exit 1 }
$ver = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$stage = Join-Path $root "dist\simfleet"
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
foreach ($f in "target\release\simfleet.exe", "README.md", "LICENSE", "NOTICE.md", "THIRD-PARTY-LICENSES.md") {
    Copy-Item (Join-Path $root $f) $stage
}
$eng = Join-Path $root "target\release\engines"
if ($WithNativeEngine -and (Test-Path $eng)) { Copy-Item -Recurse $eng (Join-Path $stage "engines") }
$zip = Join-Path $root "dist\simfleet-$ver-windows-x64.zip"
Remove-Item $zip -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $zip
Write-Host "done: $zip"
