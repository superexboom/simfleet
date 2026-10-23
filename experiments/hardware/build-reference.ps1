param(
    [Parameter(Mandatory=$true)][string]$Source,
    [Parameter(Mandatory=$true)][string]$Out,
    [string]$Cpu = 'x86-64-v3'
)
$ErrorActionPreference = 'Stop'
$src = (Resolve-Path -LiteralPath $Source).Path
$destination = [IO.Path]::GetFullPath($Out)
if ($Cpu -notmatch '^[A-Za-z0-9_-]+$') { throw 'Invalid CPU target' }
if (Test-Path -LiteralPath $destination) { throw 'Out must be a new dedicated directory' }
New-Item -ItemType Directory -Path $destination | Out-Null
$entry = (Join-Path $PSScriptRoot 'cpu-reference.rs').Replace('\','/')
$engine = (Join-Path $src 'engine').Replace('\','/')
$manifest = @"
[package]
name = "simfleet-hardware-reference"
version = "0.1.0"
edition = "2021"
[lib]
crate-type = ["cdylib"]
path = "$entry"
[dependencies]
wfsim-engine = { path = "$engine" }
[profile.release]
codegen-units = 1
[workspace]
"@
[IO.File]::WriteAllText((Join-Path $destination 'Cargo.toml'), $manifest)
Copy-Item -LiteralPath (Join-Path $src 'Cargo.lock') -Destination (Join-Path $destination 'Cargo.lock')
$previous = @{}
$names = @('RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','CARGO_TARGET_DIR')
foreach ($name in $names) { $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
try {
    [Environment]::SetEnvironmentVariable('CARGO_ENCODED_RUSTFLAGS',$null,'Process')
    $env:RUSTFLAGS = "-C target-cpu=$Cpu -C target-feature=+crt-static"
    $env:CARGO_TARGET_DIR = Join-Path $destination 'target'
    & cargo build --release --manifest-path (Join-Path $destination 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw 'Reference build failed' }
    $dll = Join-Path $destination 'cpu-reference.dll'
    Copy-Item -LiteralPath (Join-Path $env:CARGO_TARGET_DIR 'release/simfleet_hardware_reference.dll') -Destination $dll
    @{ cpu=$Cpu; source=$src; sha256=(Get-FileHash -LiteralPath $dll -Algorithm SHA256).Hash.ToLower(); rustc=(& rustc -V) } |
        ConvertTo-Json | Set-Content -LiteralPath ($dll + '.json') -Encoding utf8
    Write-Output $dll
} finally {
    foreach ($name in $names) { [Environment]::SetEnvironmentVariable($name,$previous[$name],'Process') }
}
