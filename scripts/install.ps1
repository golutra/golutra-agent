param(
    [string]$Prefix = "$HOME/.local"
)

$ErrorActionPreference = "Stop"
$Root = Resolve-Path (Join-Path $PSScriptRoot "..")
Push-Location $Root
try {
    cargo build --locked --release `
        -p golutra-cli `
        -p golutra-tui `
        -p golutra-app-server `
        -p golutra-vis `
        -p golutra-supervisor `
        -p golutra-release `
        -p golutra-eval-worker

    $Bin = Join-Path $Prefix "bin"
    New-Item -ItemType Directory -Force -Path $Bin | Out-Null
    Copy-Item "target/release/golutra-cli.exe" (Join-Path $Bin "golutra.exe") -Force
    Copy-Item "target/release/golutra-tui.exe" (Join-Path $Bin "golutra-tui.exe") -Force
    Copy-Item "target/release/golutra-app-server.exe" (Join-Path $Bin "golutra-app-server.exe") -Force
    Copy-Item "target/release/golutra-vis.exe" (Join-Path $Bin "golutra-vis.exe") -Force
    Copy-Item "target/release/golutra-supervisor.exe" (Join-Path $Bin "golutra-supervisor.exe") -Force
    Copy-Item "target/release/golutra-launcher.exe" (Join-Path $Bin "golutra-launcher.exe") -Force
    Copy-Item "target/release/golutra-eval-worker.exe" (Join-Path $Bin "golutra-eval-worker.exe") -Force
    Write-Output "Golutra installed in $Bin"
} finally {
    Pop-Location
}
