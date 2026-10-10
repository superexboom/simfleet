# Build the native engine for the engine wfsim.app serves right now.
#
#   powershell -ExecutionPolicy Bypass -File tools\build-native-engine.ps1 [-Strip]
#
# You normally do not need this: simfleet downloads the published build of the
# served engine on its own (the native-engine workflow). This is for building
# one yourself.
#
# Needs git and a Rust toolchain (https://rustup.rs); -Strip needs Python. Clones
# WFSim into .wfsim-src (or updates it), finds the newest commit whose engine id
# equals the served one, optionally blanks Digital Extremes' card text
# (tools/strip_card_text.py, required before a build is published), builds
# native-engine/ against it, and writes engines\<engine id>.dll next to
# simfleet.exe. simfleet checks it against the wasm before it uses it;
# `simfleet check-native` checks it on 32 real board rows.
param(
    [string]$Server = "https://wfsim.app",
    [int]$Depth = 60,
    [string]$Out = "",
    [switch]$Strip,
    # A WFSim commit already known to carry the served engine (the workflow's check).
    [string]$Commit = ""
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$src = Join-Path $root ".wfsim-src"
$exe = Join-Path $root "target\release\simfleet.exe"
if (-not (Test-Path $exe)) { cargo build --release --manifest-path (Join-Path $root "Cargo.toml") }
if (-not $Out) { $Out = Join-Path (Split-Path -Parent $exe) "engines" }

$rel = Invoke-RestMethod "$Server/release.json" -Headers @{ "Cache-Control" = "no-store" }
$want = $rel.engine
Write-Host "served engine: $want"

if (-not (Test-Path $src)) {
    git clone --depth $Depth https://github.com/magenie33/wfsim.git $src
} else {
    git -C $src fetch --depth $Depth origin main
    git -C $src reset -q --hard origin/main
}
if ($LASTEXITCODE -ne 0) { exit 1 }

$found = $null
$candidates = if ($Commit) { @($Commit) } else { git -C $src rev-list -n $Depth HEAD }
foreach ($c in $candidates) {
    git -C $src checkout -q $c
    $ids = & $exe engine-id $src
    if ($ids -contains $want) { $found = $c; break }
}
if (-not $found) {
    Write-Host "no commit in the last $Depth matches $want - the served engine is not on GitHub yet. simfleet keeps using the wasm."
    exit 2
}
Write-Host "commit $found has engine $want"
if ($Strip) {
    python (Join-Path $root "tools\strip_card_text.py") $src
    if ($LASTEXITCODE -ne 0) { exit 1 }
}
$env:SFE_ENGINE_ID = $want
$env:CARGO_TARGET_DIR = Join-Path $root "target\native-engine"
Copy-Item (Join-Path $src "Cargo.lock") (Join-Path $root "native-engine\Cargo.lock") -Force
cargo build --release --manifest-path (Join-Path $root "native-engine\Cargo.toml")
if ($LASTEXITCODE -ne 0) { exit 1 }
New-Item -ItemType Directory -Force $Out | Out-Null
Copy-Item (Join-Path $env:CARGO_TARGET_DIR "release\simfleet_native_engine.dll") (Join-Path $Out "$want.dll") -Force
"$found" | Set-Content -Encoding ascii (Join-Path $Out "$want.commit")
Write-Host "done: $Out\$want.dll"
