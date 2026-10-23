# Build a native engine in an isolated upstream worktree. Existing source
# changes are never reset. -Source selects an existing upstream repository;
# -NoPatch builds its original implementation for comparison.
param(
    [string]$Server = "https://wfsim.app",
    [int]$Depth = 60,
    [string]$Out = "",
    [switch]$Strip,
    [switch]$NoPatch,
    [string]$Commit = "",
    [string]$Engine = "",
    [string]$Cpu = "",
    [string]$Source = "",
    # Official GitHub source tarball for an exact Commit when Git transport fails.
    [string]$Archive = "",
    # Dedicated empty CI location keeps Rust source paths stable across runs.
    [string]$BuildRoot = "",
    [int]$Jobs = 0,
    # Opt-in: train on offline board rows, then rebuild with PGO and ThinLTO.
    [switch]$Pgo,
    [string]$TrainingOrders = "",
    [int]$TrainingRuns = 128,
    [int]$TrainingLanes = 8
)
$ErrorActionPreference = "Stop"
$win = ($null -eq $IsWindows) -or $IsWindows
$root = Split-Path -Parent $PSScriptRoot
if ($Pgo -and ($NoPatch -or $TrainingRuns -lt 1 -or $TrainingLanes -lt 1)) {
    throw 'PGO requires patched board ABI and positive training runs/lanes'
}
if ($Pgo -and -not $TrainingOrders) { $TrainingOrders = Join-Path $root 'experiments/prepared-board/orders.json' }
if ($Pgo) { $TrainingOrders = (Resolve-Path -LiteralPath $TrainingOrders).Path }
$revision = (Get-Content -Raw -LiteralPath (Join-Path $root 'native-engine/revision')).Trim()
if ($revision -notmatch '^[0-9]+$') { throw 'Invalid native build revision' }
$python = if ($win) { "python" } else { "python3" }
function Write-Utf8($path, $text) { [System.IO.File]::WriteAllText($path, $text, (New-Object System.Text.UTF8Encoding $false)) }
function Source-Hash($path) {
    $stream = [IO.File]::OpenRead([IO.Path]::GetFullPath($path))
    $digest = [Security.Cryptography.SHA256]::Create()
    try { ([BitConverter]::ToString($digest.ComputeHash($stream))).Replace('-', '').ToLowerInvariant() }
    finally { $stream.Dispose(); $digest.Dispose() }
}
if (-not $Out) { $Out = Join-Path $root "target/release/engines" }
if (-not $Engine) { $Engine = (Invoke-RestMethod "$Server/release.json" -Headers @{"Cache-Control"="no-store"}).engine }
if ($Engine -notmatch '^[A-Za-z0-9._-]{1,40}$' -or $Engine -in @('.', '..')) { throw "Invalid engine id" }
$repo = if ($Source) { (Resolve-Path -LiteralPath $Source).Path } else { Join-Path $root ".wfsim-src" }
if ($Archive) {
    if ($Commit -notmatch '^[a-f0-9]{40}$' -or $Source) { throw 'Archive requires an exact 40-character Commit and no Source' }
    $Archive = (Resolve-Path -LiteralPath $Archive).Path
} elseif (-not (Test-Path -LiteralPath $repo)) {
    git clone --depth $Depth https://github.com/magenie33/wfsim.git $repo
    if ($LASTEXITCODE -ne 0) { throw "Clone failed" }
} elseif (-not $Source -and -not $Commit) {
    git -C $repo fetch --depth $Depth origin main
    if ($LASTEXITCODE -ne 0) { throw "Fetch failed" }
}
$start = if ($Commit) { $Commit } else { "origin/main" }
$buildRoot = if ($BuildRoot) { [IO.Path]::GetFullPath($BuildRoot) } else { Join-Path $root ("target/native-build/" + [guid]::NewGuid().ToString('N')) }
if (Test-Path -LiteralPath $buildRoot) { throw 'BuildRoot must be a new dedicated directory' }
New-Item -ItemType Directory -Force -Path $buildRoot | Out-Null
$src = Join-Path $buildRoot "wfsim"
$crate = Join-Path $buildRoot "wrapper"
$found = $null
if ($Archive) {
    New-Item -ItemType Directory -Path $src | Out-Null
    & tar -xzf $Archive -C $src --strip-components=1
    if ($LASTEXITCODE -ne 0) { throw 'Source archive extraction failed' }
    git -C $src init -q
    if ($LASTEXITCODE -ne 0) { throw 'Patch workspace initialization failed' }
    $ids = & $python (Join-Path $root "tools/engine_id.py") $src
    if ($LASTEXITCODE -ne 0) { throw "Engine id failed" }
    if ($ids -contains $Engine) { $found = $Commit }
} else {
    git -C $repo worktree add --detach $src $start
    if ($LASTEXITCODE -ne 0) { throw "Worktree creation failed" }
    $candidates = if ($Commit) { @($Commit) } else { git -C $src rev-list -n $Depth HEAD }
    foreach ($candidate in $candidates) {
        git -C $src checkout -q $candidate
        if ($LASTEXITCODE -ne 0) { throw "Checkout failed" }
        $ids = & $python (Join-Path $root "tools/engine_id.py") $src
        if ($LASTEXITCODE -ne 0) { throw "Engine id failed" }
        if ($ids -contains $Engine) { $found = (git -C $src rev-parse HEAD).Trim(); break }
    }
}
if (-not $found) { throw "No source commit matches served engine $Engine" }
$applied = @()
if (-not $NoPatch) {
    foreach ($patch in Get-ChildItem (Join-Path $root "native-engine/patches") -Filter '*.patch' | Sort-Object Name) {
        git -C $src apply --check -- $patch.FullName 2>&1 | Out-Null
        if ($LASTEXITCODE -eq 0) {
            git -C $src apply -- $patch.FullName
            if ($LASTEXITCODE -ne 0) { throw "Patch failed: $($patch.Name)" }
            $applied += $patch.Name
        } elseif ($patch.Name -eq '003-static-panel-metadata.patch') {
            throw "Required memory-lifetime patch $($patch.Name) does not apply to $found; refusing a build that can leak per panel"
        } else { Write-Warning "Patch $($patch.Name) does not apply to $found; disabled" }
    }
}
if ($Strip) {
    & $python (Join-Path $root "tools/strip_card_text.py") $src
    if ($LASTEXITCODE -ne 0) { throw "Card text stripping failed" }
}
New-Item -ItemType Directory -Force -Path $crate | Out-Null
$manifest = Get-Content -Raw -LiteralPath (Join-Path $root "native-engine/Cargo.toml")
$libSource = (Join-Path $root "native-engine/src/lib.rs").Replace('\','/')
$manifest = $manifest.Replace('crate-type = ["cdylib"]', ('crate-type = ["cdylib"]' + "`npath = " + '"' + $libSource + '"'))
$manifest = $manifest.Replace('../.wfsim-src/', $src.Replace('\','/') + '/')
Write-Utf8 (Join-Path $crate "Cargo.toml") $manifest
Copy-Item -LiteralPath (Join-Path $src "Cargo.lock") -Destination (Join-Path $crate "Cargo.lock")
$names = @('SFE_ENGINE_ID','SFE_INLINE_OPTIMIZER','CARGO_TARGET_DIR','RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','CARGO_PROFILE_RELEASE_LTO','LLVM_PROFILE_FILE')
$previous = @{}
foreach ($name in $names) { $previous[$name] = [Environment]::GetEnvironmentVariable($name,'Process') }
try {
    Remove-Item -LiteralPath Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
    $env:SFE_ENGINE_ID = $Engine
    $env:SFE_INLINE_OPTIMIZER = if ($applied -contains '002-single-worker.patch') { '1' } else { '0' }
    $target = Join-Path $root 'target/native-engine'
    $suffix = ''
    $flags = @()
    if ($Cpu) {
        if ($Cpu -notmatch '^[A-Za-z0-9_-]+$') { throw "Invalid CPU target" }
        $suffix = if ($Cpu -match '^x86-64-(v[0-9])$') { '-' + $Matches[1] } else { '-' + $Cpu }
        $target += '-' + $Cpu
        $flags += '-C target-cpu=' + $Cpu
    }
    if ($win) { $flags += '-C target-feature=+crt-static' }
    if ($Pgo) { $target += '-pgo' }
    $env:RUSTFLAGS = $flags -join ' '
    $env:CARGO_TARGET_DIR = $target
    $buildArgs = @('build','--release','--manifest-path',(Join-Path $crate 'Cargo.toml'))
    if ($applied -contains '001-prepared-board.patch') { $buildArgs += @('--features','fast-board') }
    if ($applied -contains '004-frozen-optimize-plan.patch') { $buildArgs += @('--features','cached-optimize') }
    if ($Jobs -gt 0) { $buildArgs += @('-j',"$Jobs") }
    $pgoInfo = $null
    if ($Pgo) {
        if ($applied -notcontains '001-prepared-board.patch') { throw 'PGO requires the opaque board ABI patch' }
        $rustSysroot = (& rustc --print sysroot).Trim()
        $rustDetails = & rustc -vV
        $hostTriple = (($rustDetails | Where-Object { $_ -like 'host:*' }) -replace '^host:\s*','').Trim()
        $profdataTool = Join-Path $rustSysroot "lib/rustlib/$hostTriple/bin/llvm-profdata"
        if ($win) { $profdataTool += '.exe' }
        if (-not (Test-Path -LiteralPath $profdataTool)) { throw 'PGO requires: rustup component add llvm-tools' }
        $profiles = Join-Path $buildRoot 'profiles'
        New-Item -ItemType Directory -Path $profiles | Out-Null
        $profile = Join-Path $buildRoot 'board.profdata'
        $env:CARGO_PROFILE_RELEASE_LTO = 'thin'
        # Encoded flags preserve paths containing spaces in both build phases.
        # Give the DLL an explicit profile path; do not rely on the host's CRT
        # environment/exit hooks to discover LLVM_PROFILE_FILE.
        Remove-Item -LiteralPath Env:LLVM_PROFILE_FILE -ErrorAction SilentlyContinue
        $encoded = @()
        foreach ($flag in $flags) { $encoded += ($flag -split ' ', 2) }
        $env:CARGO_ENCODED_RUSTFLAGS = ($encoded + @('-C', "profile-generate=$profiles")) -join [char]31
        & cargo @buildArgs
        if ($LASTEXITCODE -ne 0) { throw 'PGO instrumented build failed' }
        $trainingLib = if ($win) { 'simfleet_native_engine.dll' } elseif ($IsMacOS) { 'libsimfleet_native_engine.dylib' } else { 'libsimfleet_native_engine.so' }
        $trainingResult = Join-Path $buildRoot 'training.json'
        & $python (Join-Path $root 'tools/bench-board.py') --library "training=$(Join-Path $target "release/$trainingLib")" --orders $TrainingOrders --out $trainingResult --lanes $TrainingLanes --runs $TrainingRuns --repeats 1
        if ($LASTEXITCODE -ne 0) { throw 'PGO training failed' }
        if (-not (Get-ChildItem -LiteralPath $profiles -Filter '*.profraw')) { throw "PGO training produced no profiles in $profiles" }
        & $profdataTool merge -o $profile $profiles
        if ($LASTEXITCODE -ne 0) { throw 'PGO profile merge failed' }
        # Cargo's encoded flags preserve a profile path containing spaces.
        $env:CARGO_ENCODED_RUSTFLAGS = ($encoded + @('-C', "profile-use=$profile", '-C', 'llvm-args=-pgo-warn-missing-function')) -join [char]31
        & cargo @buildArgs
        if ($LASTEXITCODE -ne 0) { throw 'PGO optimized build failed' }
        & $python (Join-Path $root 'tools/bench-board.py') --library "optimized=$(Join-Path $target "release/$trainingLib")" --orders $TrainingOrders --out (Join-Path $buildRoot 'pgo-check.json') --lanes $TrainingLanes --runs $TrainingRuns --repeats 1 --expect $trainingResult
        if ($LASTEXITCODE -ne 0) { throw 'PGO changed a full board answer' }
        $pgoInfo = @{
            lto='thin'; training_runs=$TrainingRuns; training_lanes=$TrainingLanes
            orders_sha256=(Source-Hash $TrainingOrders)
            profile_sha256=(Source-Hash $profile)
            training_result=$trainingResult; rustc=($rustDetails -join "`n")
        }
    } else {
        & cargo @buildArgs
        if ($LASTEXITCODE -ne 0) { throw "Native build failed" }
    }
    New-Item -ItemType Directory -Force -Path $Out | Out-Null
    $stem = if ($NoPatch) { $Engine } else { "$Engine-sfe$revision" }
    $lib, $name = if ($win) { 'simfleet_native_engine.dll', "$stem$suffix.dll" }
        elseif ($IsMacOS) { 'libsimfleet_native_engine.dylib', "lib$stem$suffix.dylib" }
        else { 'libsimfleet_native_engine.so', "lib$stem$suffix.so" }
    $destination = Join-Path $Out $name
    Copy-Item -LiteralPath (Join-Path $target "release/$lib") -Destination $destination -Force
    $hash = Source-Hash $destination
    Write-Utf8 ($destination + '.sha256') ($hash + '  ' + $name + "`n")
    Write-Utf8 (Join-Path $Out "$Engine.commit") ($found + "`n")
    Write-Utf8 (Join-Path $Out "$stem$suffix.patches") (($applied -join "`n") + "`n")
    $wrapperCommit = 'source-bundle'
    if (Test-Path -LiteralPath (Join-Path $root '.git')) { $wrapperCommit = (git -C $root rev-parse HEAD).Trim() }
    $sourceHashes = @{}
    foreach ($file in @(Get-Item (Join-Path $root 'native-engine/Cargo.toml')) + @(Get-ChildItem (Join-Path $root 'native-engine/src') -Filter '*.rs') + @(Get-Item (Join-Path $root 'src/json_numbers.rs'))) {
        $sourceHashes[$file.FullName.Substring($root.Length + 1)] = Source-Hash $file.FullName
    }
    $patchHashes = @{}
    foreach ($patchName in $applied) {
        $patchHashes[$patchName] = Source-Hash (Join-Path $root "native-engine/patches/$patchName")
    }
    $provenance = @{ engine=$Engine; revision=$revision; source=$found; wrapper=$wrapperCommit; wrapper_files=$sourceHashes; patch_files=$patchHashes; cpu=$Cpu; sha256=$hash; patches=$applied }
    if ($Archive) { $provenance.archive_sha256 = Source-Hash $Archive }
    if ($pgoInfo) { $provenance.pgo = $pgoInfo }
    Write-Utf8 ($destination + '.build.json') ($provenance | ConvertTo-Json -Depth 5)
    Write-Output "Built $destination; run check-native before use. Source retained at $src"
} finally {
    foreach ($name in $names) {
        if ($null -eq $previous[$name]) { Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue }
        else { [Environment]::SetEnvironmentVariable($name,$previous[$name],'Process') }
    }
}
