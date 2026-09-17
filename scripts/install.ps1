param(
    [string]$Prefix = "$HOME/.local"
)

$ErrorActionPreference = "Stop"
$Root = Resolve-Path (Join-Path $PSScriptRoot "..")
Push-Location $Root
try {
    cargo build --locked --release `
        -p golutra-agent-cli `
        -p golutra-agent-tui `
        -p golutra-agent-app-server `
        -p golutra-agent-vis `
        -p golutra-agent-supervisor `
        -p golutra-agent-release `
        -p golutra-agent-eval-worker

    # 原生命令失败不一定触发 PowerShell ErrorAction，避免安装旧产物。
    if ($LASTEXITCODE -ne 0) { throw "Golutra Agent build failed: $LASTEXITCODE" }

    $Bin = Join-Path $Prefix "bin"
    New-Item -ItemType Directory -Force -Path $Bin | Out-Null
    Copy-Item "target/release/golutra-agent.exe" (Join-Path $Bin "golutra-agent.exe") -Force
    Copy-Item "target/release/golutra-agent-tui.exe" (Join-Path $Bin "golutra-agent-tui.exe") -Force
    Copy-Item "target/release/golutra-agent-app-server.exe" (Join-Path $Bin "golutra-agent-app-server.exe") -Force
    Copy-Item "target/release/golutra-agent-vis.exe" (Join-Path $Bin "golutra-agent-vis.exe") -Force
    Copy-Item "target/release/golutra-agent-supervisor.exe" (Join-Path $Bin "golutra-agent-supervisor.exe") -Force
    Copy-Item "target/release/golutra-agent-launcher.exe" (Join-Path $Bin "golutra-agent-launcher.exe") -Force
    Copy-Item "target/release/golutra-agent-eval-worker.exe" (Join-Path $Bin "golutra-agent-eval-worker.exe") -Force
    Write-Output "Golutra Agent installed in $Bin"
} finally {
    Pop-Location
}
