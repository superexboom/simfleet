# PUBLISH THE NATIVE ENGINE FROM THIS MACHINE, for the engine wfsim.app serves:
#
#   powershell -ExecutionPolicy Bypass -File tools\publish-native-engine.ps1 [-Lanes 4] [-Jobs 4] [-NoUpload]
#
# Faster than the native-engine workflow, which must build a reference wasm
# because wfsim.app turns GitHub's runners away: from here the site answers,
# so each build is held to the wasm the site actually serves. Builds the
# Windows engine for any x86-64 and for x86-64-v3 (tools/build-native-engine.ps1
# -Strip: card text blanked), holds each to that wasm on WFSim's 32 board
# rows, the client contract's gas vectors (docs/CLIENTS.md section "Conformance" 7,
# tests/gas/rows.json) and the riven canary, and only then uploads what the
# release engine-<id> does not hold yet - nothing published is ever replaced.
# The Linux builds are the workflow's, which finds the Windows ones here and
# builds only what is missing.
#
# Needs git, Rust, Python and the GitHub CLI (gh), signed in to the repository.
param(
    # Lanes for the checks (0: every core), and cargo's jobs (0: cargo's own).
    [int]$Lanes = 0,
    [int]$Jobs = 0,
    [switch]$NoUpload,
    [string]$Repo = "superexboom/simfleet"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
# UTF-8 without the BOM Windows PowerShell would write.
function Write-Text($path, $text) { [System.IO.File]::WriteAllText($path, $text, (New-Object System.Text.UTF8Encoding $false)) }
if ($Jobs -gt 0) { $env:CARGO_BUILD_JOBS = "$Jobs" }
cargo build --release --locked --manifest-path (Join-Path $root "Cargo.toml")
if ($LASTEXITCODE -ne 0) { exit 1 }

$rel = Invoke-RestMethod "https://wfsim.app/release.json" -Headers @{ "Cache-Control" = "no-store" }
$id = $rel.engine
$files = @("$id.dll", "$id-v3.dll")
$ErrorActionPreference = "Continue"
$have = @(gh release view "engine-$id" -R $Repo --json assets -q ".assets[].name" 2>$null)
$ErrorActionPreference = "Stop"
$missing = @($files | Where-Object { $have -notcontains $_ })
Write-Host "served engine $id (wasm $($rel.wasm)); release holds: $(if ($have.Count) { $have -join ', ' } else { 'nothing' })"
if ($missing.Count -eq 0) { Write-Host "nothing to publish"; exit 0 }

$stage = Join-Path $root "target\publish\$id"
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Join-Path $stage "engines") | Out-Null
foreach ($cpu in "", "x86-64-v3") {
    & (Join-Path $PSScriptRoot "build-native-engine.ps1") -Strip -Engine $id -Cpu $cpu -Out (Join-Path $stage "engines")
    if ($LASTEXITCODE -ne 0) { Write-Host "build failed"; exit 1 }
}
$commit = (Get-Content (Join-Path $stage "engines\$id.commit") -Raw).Trim()
Copy-Item (Join-Path $root "target\release\simfleet.exe") $stage
$cache = Join-Path $root "target\release\cache"
if (Test-Path $cache) { Copy-Item -Recurse $cache (Join-Path $stage "cache") }

# THE CHECKS, each build alone (SIMFLEET_CPU), against the served wasm.
$rows = Join-Path $root ".wfsim-src\tests\gas\rows.json"
$args_ = @("check-native")
if (Test-Path $rows) { $args_ += @("--rows", $rows) }
if ($Lanes -gt 0) { $args_ += @("--lanes", "$Lanes") }
# simfleet writes UTF-8; read it as such (Windows PowerShell would use the code page).
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
$verdicts = @()
foreach ($cpu in "baseline", "v3") {
    Write-Host "== check $cpu"
    $env:SIMFLEET_CPU = $cpu
    $ErrorActionPreference = "Continue"
    $out = & (Join-Path $stage "simfleet.exe") @args_ 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    $ErrorActionPreference = "Stop"
    $out | ForEach-Object { Write-Host $_ }
    if ($code -ne 0) { Write-Host "check failed ($cpu): nothing published"; exit 1 }
    # THE VERDICT, from check-native's ASCII line: the rows, and the riven canary's word.
    $line = $out | Where-Object { $_ -match "^CHECK-NATIVE rows=(\d+) skipped=\d+ riven=(\w+)" } | Select-Object -First 1
    if (-not $line) { Write-Host "no verdict line ($cpu): nothing published"; exit 1 }
    $null = $line -match "rows=(\d+) skipped=\d+ riven=(\w+)"
    $riven = if ($Matches[2] -eq "same") { "the riven canary gave the same answer" } else { "the riven canary did not agree (riven gains stay on the wasm)" }
    $verdicts += "$(if ($cpu -eq 'v3') { 'x86-64-v3' } else { 'x86-64' }): $($Matches[1]) rows bit for bit (WFSim's board rows and the client contract's gas vectors: score, work, gas), $riven"
}
Remove-Item Env:SIMFLEET_CPU
if ($NoUpload) { Write-Host "checked; -NoUpload: $stage"; exit 0 }

# THE FILES and their sums, and the notes: the corresponding source named.
$up = @()
$sums = ""
foreach ($f in $missing) {
    $p = Join-Path $stage "engines\$f"
    $sum = (Get-FileHash -Algorithm SHA256 $p).Hash.ToLower()
    "$sum  $f" | Set-Content -Encoding ascii -NoNewline "$p.sha256"
    $up += @($p, "$p.sha256")
    $sums += "- ``$f``: ``$sum```n"
}
$head = (git -C $root rev-parse HEAD).Trim()
$tree = "https://github.com/$Repo/tree/$head"
$checked = $verdicts -join "; "
if ($have.Count -gt 0) {
    $notes = (gh release view "engine-$id" -R $Repo --json body -q .body | Out-String).TrimEnd()
    $notes += "`n`n**Added later**, built and checked on a maintainer's machine by tools/publish-native-engine.ps1 at simfleet [``$head``]($tree), card text blanked the same way:`n`n$sums"
    Write-Text (Join-Path $stage "notes.md") $notes
    gh release upload "engine-$id" @up -R $Repo
    gh release edit "engine-$id" -R $Repo --notes-file (Join-Path $stage "notes.md")
} else {
    $notes = @"
Native build of WFSim engine **$id** for simfleet. simfleet downloads it on its own when wfsim.app serves this engine; there is nothing to install.

- **Builds.** Windows ``$id.dll`` runs on any x86-64, ``$id-v3.dll`` on x86-64-v3 CPUs (AVX2, FMA, BMI2). simfleet picks the one the CPU runs; both give the same bits. Linux builds are added by the [native-engine workflow](https://github.com/$Repo/actions/workflows/native-engine.yml) where they match.
- **Source.** WFSim commit [``$commit``](https://github.com/magenie33/wfsim/tree/$commit) as committed (AGPL-3.0-or-later, by magenie33 and WFSim contributors), through [native-engine/]($tree/native-engine) at simfleet [``$head``]($tree): WFSim's API and optimizer behind C functions, the optimizer asked for one worker as a request may (the wasm build's batch of 16), mimalloc as the allocator (as WFSim's own library has it).
- **No card text.** Digital Extremes' card text was blanked from WFSim's ``data/`` before the build ([tools/strip_card_text.py](https://github.com/$Repo/blob/$head/tools/strip_card_text.py)), since WFSim grants nothing for it ([LICENSE-DATA.md](https://github.com/magenie33/wfsim/blob/main/LICENSE-DATA.md)). The engine only displays that text; no number depends on it.
- **Checked** against wfsim.app's own wasm $($rel.wasm), each build: $checked. simfleet checks the one it uses against the site's wasm again before every use.
- Unofficial; not made or endorsed by WFSim or Digital Extremes. *Warframe* is a trademark of Digital Extremes Ltd.

SHA-256:

$sums
"@
    Write-Text (Join-Path $stage "notes.md") $notes
    gh release create "engine-$id" @up -R $Repo --title "native engine $id" --notes-file (Join-Path $stage "notes.md") --latest=false --target $head
}
if ($LASTEXITCODE -ne 0) { Write-Host "upload failed"; exit 1 }
Write-Host "published engine-${id}: $($missing -join ', ')"
