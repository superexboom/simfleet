# Package a verified optional GPU engine, separate from the five-file client.
param([Parameter(Mandatory=$true)][string]$Source, [string]$Out='')
$ErrorActionPreference='Stop'
$root=Split-Path -Parent $PSScriptRoot
$src=(Resolve-Path -LiteralPath $Source).Path
$info=Get-Content -Raw -LiteralPath (Join-Path $src 'engine.gpu.json') | ConvertFrom-Json
if ($info.schema -ne 1 -or $info.revision -notin @(1,2) -or $info.host -ne 'windows-x64-v3' -or $info.architecture -notmatch '^compute_[0-9]{2,3}$' -or $info.engine -notmatch '^[a-f0-9]{12}$') {throw 'Unsupported GPU package identity'}
$sm = $info.architecture -replace '^compute_', 'sm'
$validation = if ($info.device_validated -eq $false) { 'Cross-compiled only; target GPU runtime validation is still required. Automatic per-task WASM checks remain enabled.' } else { 'Validated on the matching local GPU; each task retains its WASM correctness check.' }
$minimumClient = if ($info.revision -eq 2) { '0.9.0-preview.12' } else { '0.9.0-preview.7' }
foreach($name in @('engine.cubin','bridge.wasm','wire.dll')) {
    $actual=(Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $src $name)).Hash.ToLowerInvariant()
    if ($actual -ne $info.files.$name) {throw "GPU hash mismatch: $name"}
}
if(-not $Out){$Out=Join-Path $root 'dist'}
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$stage=Join-Path $Out ('.gpu-stage-'+[guid]::NewGuid().ToString('N'))
$leaf="$($info.engine)-sfg$($info.revision)-$sm"
$payload=Join-Path $stage "engines/gpu/$leaf"
New-Item -ItemType Directory -Force -Path $payload | Out-Null
foreach($name in @('engine.cubin','bridge.wasm','wire.dll','engine.gpu.json')){Copy-Item -LiteralPath (Join-Path $src $name) -Destination $payload}
Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination $payload
$notice=@"
simfleet optional NVIDIA board engine. Experimental; Windows x64-v3 / $sm.
Engine: $($info.engine); GPU ABI/revision: sfg$($info.revision).
Requires simfleet $minimumClient or later and a compatible NVIDIA driver.
$validation
Source: https://github.com/superexboom/simfleet (AGPL-3.0-or-later).
Based on WFSim: https://github.com/magenie33/wfsim (AGPL-3.0-or-later).
WABT wasm2c generation: Copyright WebAssembly Community Group participants,
Apache-2.0. Its license follows. CUDA/NVIDIA driver is not redistributed.

"@
$notice+=Get-Content -Raw -LiteralPath (Join-Path $root 'experiments/gpu-board/WABT-LICENSE.txt')
$notice+="`n`n"+(Get-Content -Raw -LiteralPath (Join-Path $root 'NOTICE.md'))
$notice+="`n`n"+(Get-Content -Raw -LiteralPath (Join-Path $root 'THIRD-PARTY-LICENSES.md'))
[IO.File]::WriteAllText((Join-Path $payload 'NOTICE.txt'),$notice,[Text.UTF8Encoding]::new($false))
$zip=Join-Path $Out "simfleet-gpu-$leaf-windows-x64.zip"
Compress-Archive -Path (Join-Path $stage 'engines') -DestinationPath $zip -Force
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive=[IO.Compression.ZipFile]::OpenRead($zip)
try {
    $files=@($archive.Entries | Where-Object {$_.Name})
    if($files.Count -ne 6){throw 'Unexpected GPU package contents'}
    $files | ForEach-Object {$_.FullName}
} finally {$archive.Dispose()}
Get-FileHash -Algorithm SHA256 -LiteralPath $zip
