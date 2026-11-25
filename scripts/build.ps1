# Build the desktop client and drop the exe into a folder Windows treats normally.
#
# The repository tree can carry a low-integrity label from tooling. Executables
# inherit it, and a low-integrity process is not allowed to talk to the taskbar,
# so its tray icon silently never appears. Copying the binary out of the tree
# sidesteps that entirely.

[CmdletBinding()]
param(
    [string]$OutDir = 'D:\Software\now-doing',
    [switch]$SkipTest
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$desktop = Join-Path $root 'apps\desktop'

# A running instance holds the exe open and the linker cannot replace it.
Stop-Process -Name now-doing -Force -ErrorAction SilentlyContinue

# ring's C build overflows the default PDB debug record on this toolchain.
$env:CARGO_PROFILE_DEV_DEBUG = '0'
$env:CARGO_PROFILE_DEV_INCREMENTAL = 'false'

Push-Location $desktop
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    if (-not $SkipTest) {
        cargo test
        if ($LASTEXITCODE -ne 0) { throw 'cargo test failed' }
    }
}
finally {
    Pop-Location
}

$out = [IO.Path]::GetFullPath($OutDir)
New-Item -ItemType Directory -Force -Path $out | Out-Null
$target = Join-Path $out 'now-doing.exe'
Copy-Item (Join-Path $desktop 'target\release\now-doing.exe') $target -Force

$size = [math]::Round((Get-Item $target).Length / 1MB, 2)
Write-Host "built $target ($size MB)"
Write-Host 'run it, then look for the tray icon; config lives next to the exe in data\'
