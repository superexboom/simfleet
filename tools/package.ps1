# Build and pack a release: dist/simfleet-<version>-windows-x64.zip, or on
# Linux (pwsh) dist/simfleet-<version>-linux-x64.tar.gz, on macOS (Apple
# silicon) dist/simfleet-<version>-macos-arm64.tar.gz - simfleet,
# README.md, LICENSE, NOTICE.md, THIRD-PARTY-LICENSES.md.
#
# The native engine is NOT included: simfleet downloads the published build of
# the engine wfsim.app serves (NOTICE.md). Engine libraries are published
# separately by native-engine.yml, never alongside a client release.
$ErrorActionPreference = "Stop"
$win = ($null -eq $IsWindows) -or $IsWindows
$root = Split-Path -Parent $PSScriptRoot
cargo build --release --locked --manifest-path (Join-Path $root "Cargo.toml")
if ($LASTEXITCODE -ne 0) { exit 1 }
$ver = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$dist = Join-Path $root "dist"
$stageRoot = Join-Path $dist ('.stage-' + [guid]::NewGuid().ToString('N'))
$stage = Join-Path $stageRoot "simfleet"
New-Item -ItemType Directory -Force $stage | Out-Null
$bin = Join-Path (Join-Path (Join-Path $root "target") "release") $(if ($win) { "simfleet.exe" } else { "simfleet" })
Copy-Item $bin $stage
foreach ($f in "README.md", "LICENSE", "NOTICE.md", "THIRD-PARTY-LICENSES.md") {
    Copy-Item (Join-Path $root $f) $stage
}
if ($win) {
    $out = Join-Path $dist "simfleet-$ver-windows-x64.zip"
    Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $out -Force
} else {
    $plat = if ($IsMacOS) { "macos-arm64" } else { "linux-x64" }
    $out = Join-Path $dist "simfleet-$ver-$plat.tar.gz"
    tar -C $stageRoot -czf $out simfleet
    if ($LASTEXITCODE -ne 0) { exit 1 }
}
Write-Host "done: $out"
