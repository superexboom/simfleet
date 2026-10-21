# Build and pack a release: dist/simfleet-<version>-windows-x64.zip, or on
# Linux (pwsh) dist/simfleet-<version>-linux-x64.tar.gz, on macOS (Apple
# silicon) dist/simfleet-<version>-macos-arm64.tar.gz - simfleet,
# README.md, LICENSE, NOTICE.md, THIRD-PARTY-LICENSES.md.
#
# The native engine is NOT included: simfleet downloads the published build of
# the engine wfsim.app serves (NOTICE.md). -WithNativeEngine adds a local one
# with corresponding wrapper/patch sources and build manifests.
param([switch]$WithNativeEngine)
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
if (Test-Path (Join-Path $root "docs")) {
    Copy-Item -Recurse (Join-Path $root "docs") (Join-Path $stage "docs")
}
$eng = Join-Path (Split-Path -Parent $bin) "engines"
if ($WithNativeEngine) {
    $revision = (Get-Content -Raw -LiteralPath (Join-Path $root 'native-engine/revision')).Trim()
    $libraries = @(Get-ChildItem -LiteralPath $eng -File | Where-Object { $_.Name -match "-sfe$revision.*\.(dll|so|dylib)$" })
    if ($libraries.Count -eq 0) { throw 'No native library for the current revision' }
    $engineStage = Join-Path $stage 'engines'
    New-Item -ItemType Directory -Force -Path $engineStage | Out-Null
    foreach ($lib in $libraries) {
        $manifest = Get-Content -Raw -LiteralPath ($lib.FullName + '.build.json') | ConvertFrom-Json
        if ((Get-FileHash -LiteralPath $lib.FullName).Hash.ToLower() -ne $manifest.sha256) { throw "Library hash mismatch: $($lib.Name)" }
        foreach ($entry in $manifest.wrapper_files.PSObject.Properties) {
            $sourcePath = Join-Path $root ($entry.Name.Replace('\','/'))
            if ((Get-FileHash -LiteralPath $sourcePath).Hash.ToLower() -ne $entry.Value) { throw "Rebuild $($lib.Name): wrapper source changed ($($entry.Name))" }
        }
        foreach ($entry in $manifest.patch_files.PSObject.Properties) {
            $sourcePath = Join-Path $root "native-engine/patches/$($entry.Name)"
            if ((Get-FileHash -LiteralPath $sourcePath).Hash.ToLower() -ne $entry.Value) { throw "Rebuild $($lib.Name): patch changed ($($entry.Name))" }
        }
        foreach ($path in @($lib.FullName, ($lib.FullName + '.sha256'), ($lib.FullName + '.build.json'))) {
            Copy-Item -LiteralPath $path -Destination $engineStage
        }
    }
    $source = Join-Path $stage 'native-source'
    foreach ($name in @('native-engine/src','native-engine/patches','src','tools')) {
        New-Item -ItemType Directory -Force -Path (Join-Path $source $name) | Out-Null
    }
    Copy-Item -Path (Join-Path $root 'native-engine/src/*.rs') -Destination (Join-Path $source 'native-engine/src')
    Copy-Item -Path (Join-Path $root 'native-engine/patches/*.patch') -Destination (Join-Path $source 'native-engine/patches')
    foreach ($name in @('native-engine/Cargo.toml','native-engine/README.md','native-engine/revision','src/json_numbers.rs','tools/build-native-engine.ps1','tools/engine_id.py','tools/strip_card_text.py')) {
        Copy-Item -LiteralPath (Join-Path $root $name) -Destination (Join-Path $source $name)
    }
}
$suffix = if ($WithNativeEngine) { '-native' } else { '' }
if ($win) {
    $out = Join-Path $dist "simfleet-$ver-windows-x64$suffix.zip"
    Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $out -Force
} else {
    $plat = if ($IsMacOS) { "macos-arm64" } else { "linux-x64" }
    $out = Join-Path $dist "simfleet-$ver-$plat$suffix.tar.gz"
    tar -C $stageRoot -czf $out simfleet
    if ($LASTEXITCODE -ne 0) { exit 1 }
}
$hash = (Get-FileHash -LiteralPath $out -Algorithm SHA256).Hash.ToLower()
[IO.File]::WriteAllText(($out + '.sha256'), ($hash + '  ' + [IO.Path]::GetFileName($out) + "`n"), [Text.UTF8Encoding]::new($false))
Write-Host "done: $out"
