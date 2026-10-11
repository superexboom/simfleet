# Build the native engine for the engine wfsim.app serves right now.
#
#   powershell -ExecutionPolicy Bypass -File tools\build-native-engine.ps1 [-Strip]   # Windows
#   pwsh tools/build-native-engine.ps1 [-Strip]                                        # Linux
#
# You normally do not need this: simfleet downloads the published build of the
# served engine on its own (the native-engine workflow). This is for building
# one yourself.
#
# Needs git and a Rust toolchain (https://rustup.rs); -Strip needs Python. Clones
# WFSim into .wfsim-src (or updates it), finds the newest commit whose engine id
# equals the served one, optionally blanks Digital Extremes' card text
# (tools/strip_card_text.py, required before a build is published), applies
# native-engine/patches/*.patch (speed only, the same bits; -NoPatch skips them,
# and a patch that no longer applies is skipped with a warning), builds
# native-engine/ against it, and writes engines/<engine id>.dll (Linux:
# engines/lib<engine id>.so) next to simfleet (with -Cpu x86-64-v3:
# <engine id>-v3, the AVX2 build simfleet prefers on CPUs that have it).
# simfleet checks it against the wasm before it uses it;
# `simfleet check-native` checks it on 32 real board rows.
param(
    [string]$Server = "https://wfsim.app",
    [int]$Depth = 60,
    [string]$Out = "",
    [switch]$Strip,
    [switch]$NoPatch,
    # A WFSim commit already known to carry the engine (the workflow's check).
    [string]$Commit = "",
    # The engine id to build, instead of asking wfsim.app (the workflow cannot).
    [string]$Engine = "",
    # A CPU level to build for (x86-64-v3); the file is named for it (-v3).
    [string]$Cpu = ""
)
$ErrorActionPreference = "Stop"
# Windows PowerShell 5.1 has no $IsWindows, and runs only on Windows.
$win = ($null -eq $IsWindows) -or $IsWindows
$root = Split-Path -Parent $PSScriptRoot
$src = Join-Path $root ".wfsim-src"
$exe = Join-Path (Join-Path (Join-Path $root "target") "release") $(if ($win) { "simfleet.exe" } else { "simfleet" })
$python = if ($win) { "python" } else { "python3" }
if (-not (Test-Path $exe)) { cargo build --release --manifest-path (Join-Path $root "Cargo.toml") }
if (-not $Out) { $Out = Join-Path (Split-Path -Parent $exe) "engines" }

if ($Engine) {
    $want = $Engine
} else {
    $rel = Invoke-RestMethod "$Server/release.json" -Headers @{ "Cache-Control" = "no-store" }
    $want = $rel.engine
}
Write-Host "engine: $want"

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
    & $python (Join-Path (Join-Path $root "tools") "strip_card_text.py") $src
    if ($LASTEXITCODE -ne 0) { exit 1 }
}
# THE PATCHES, each applied whole or not at all; the engine id stays WFSim's,
# since the answers are the same bits (check-native holds them to that).
$applied = @()
if (-not $NoPatch) {
    foreach ($p in Get-ChildItem (Join-Path (Join-Path $root "native-engine") "patches") -Filter *.patch -ErrorAction SilentlyContinue | Sort-Object Name) {
        # A refusal is git writing to stderr, which Windows PowerShell would stop on.
        $ErrorActionPreference = "Continue"
        git -C $src apply --check $p.FullName 2>&1 | Out-Null
        $ok = $LASTEXITCODE -eq 0
        $ErrorActionPreference = "Stop"
        if ($ok) {
            git -C $src apply $p.FullName
            $applied += $p.Name
            Write-Host "patch applied: $($p.Name)"
        } else {
            Write-Warning "patch $($p.Name) no longer applies to $found; building without it"
        }
    }
}
$env:SFE_ENGINE_ID = $want
$env:CARGO_TARGET_DIR = Join-Path (Join-Path $root "target") "native-engine"
$suffix = ""
if ($Cpu) {
    $suffix = if ($Cpu -match '^x86-64-(v\d)$') { "-" + $Matches[1] } else { "-$Cpu" }
    $env:RUSTFLAGS = "-C target-cpu=$Cpu"
    $env:CARGO_TARGET_DIR += "-$Cpu"
}
$ne = Join-Path $root "native-engine"
Copy-Item (Join-Path $src "Cargo.lock") (Join-Path $ne "Cargo.lock") -Force
# A RIVEN GAIN natively needs the optimizer patch: the entry point is built only with it.
$features = @()
if ($applied -contains "optimizer-one-thread.patch") { $features = @("--features", "riven") }
cargo build --release --manifest-path (Join-Path $ne "Cargo.toml") @features
$built = $LASTEXITCODE
# The checkout goes back as it was, patched or not.
if ($applied.Count -gt 0) { git -C $src checkout -q -- webapi engine optimizer }
if ($built -ne 0) { exit 1 }
New-Item -ItemType Directory -Force $Out | Out-Null
$lib, $name = if ($win) { "simfleet_native_engine.dll", "$want$suffix.dll" } else { "libsimfleet_native_engine.so", "lib$want$suffix.so" }
Copy-Item (Join-Path (Join-Path $env:CARGO_TARGET_DIR "release") $lib) (Join-Path $Out $name) -Force
"$found" | Set-Content -Encoding ascii (Join-Path $Out "$want.commit")
($applied -join "`n") | Set-Content -Encoding ascii (Join-Path $Out "$want$suffix.patches")
Write-Host "done: $(Join-Path $Out $name)$(if ($applied.Count) { " (patched: $($applied -join ', '))" })"
