# Build the native engine for the engine wfsim.app serves right now.
#
#   powershell -ExecutionPolicy Bypass -File tools\build-native-engine.ps1
#
# Needs git and a Rust toolchain (https://rustup.rs). Clones WFSim into
# .wfsim-src (or updates it), finds the newest commit whose engine id equals
# the served one, builds native-engine/ against it, and puts the library in
# engines\<engine id>.dll next to simfleet.exe. simfleet checks it against the
# wasm on a canary row before it uses it.
param(
    [string]$Server = "https://wfsim.app",
    [int]$Depth = 60,
    [string]$Out = ""
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
    git -C $src reset --hard origin/main
}

$found = $null
foreach ($c in (git -C $src rev-list -n $Depth HEAD)) {
    git -C $src checkout -q $c
    $ids = & $exe engine-id $src
    if ($ids -contains $want) { $found = $c; break }
}
if (-not $found) {
    Write-Host "no commit in the last $Depth matches $want - the served engine is not on GitHub yet. simfleet keeps using the wasm."
    exit 1
}
Write-Host "commit $found has engine $want; building..."
$env:SFE_ENGINE_ID = $want
$env:CARGO_TARGET_DIR = Join-Path $root "target\native-engine"
Copy-Item (Join-Path $src "Cargo.lock") (Join-Path $root "native-engine\Cargo.lock") -Force
cargo build --release --manifest-path (Join-Path $root "native-engine\Cargo.toml")
New-Item -ItemType Directory -Force $Out | Out-Null
Copy-Item (Join-Path $env:CARGO_TARGET_DIR "release\simfleet_native_engine.dll") (Join-Path $Out "$want.dll") -Force
Write-Host "done: $Out\$want.dll"
