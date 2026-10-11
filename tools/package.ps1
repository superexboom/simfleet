# Build and pack a release: dist/simfleet-<version>-windows-x64.zip, or on
# Linux (pwsh) dist/simfleet-<version>-linux-x64.tar.gz - simfleet,
# README.md, LICENSE, NOTICE.md, THIRD-PARTY-LICENSES.md.
#
# The native engine is NOT included: simfleet downloads the published build of
# the engine wfsim.app serves (NOTICE.md). -WithNativeEngine adds a local one
# for a private copy only.
param([switch]$WithNativeEngine)
$ErrorActionPreference = "Stop"
$win = ($null -eq $IsWindows) -or $IsWindows
$root = Split-Path -Parent $PSScriptRoot
cargo build --release --locked --manifest-path (Join-Path $root "Cargo.toml")
if ($LASTEXITCODE -ne 0) { exit 1 }
$ver = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$dist = Join-Path $root "dist"
$stage = Join-Path $dist "simfleet"
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
$bin = Join-Path (Join-Path (Join-Path $root "target") "release") $(if ($win) { "simfleet.exe" } else { "simfleet" })
Copy-Item $bin $stage
foreach ($f in "README.md", "LICENSE", "NOTICE.md", "THIRD-PARTY-LICENSES.md") {
    Copy-Item (Join-Path $root $f) $stage
}
$eng = Join-Path (Split-Path -Parent $bin) "engines"
if ($WithNativeEngine -and (Test-Path $eng)) { Copy-Item -Recurse $eng (Join-Path $stage "engines") }
if ($win) {
    $out = Join-Path $dist "simfleet-$ver-windows-x64.zip"
    Remove-Item $out -ErrorAction SilentlyContinue
    Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $out
} else {
    $out = Join-Path $dist "simfleet-$ver-linux-x64.tar.gz"
    tar -C $dist -czf $out simfleet
    if ($LASTEXITCODE -ne 0) { exit 1 }
}
Write-Host "done: $out"
