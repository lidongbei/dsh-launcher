<#
.SYNOPSIS
  构建 DSH Launcher（单个 exe，图标已内嵌）。

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File .\build.ps1
  powershell -ExecutionPolicy Bypass -File .\build.ps1 -Run
#>
param(
    [switch]$Run
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Push-Location $root
try {
    Write-Host '==> cargo build --release'
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（exit $LASTEXITCODE）" }

    $built = Join-Path $root 'target/release/dsh-launcher.exe'
    if (-not (Test-Path -LiteralPath $built)) { throw "没有找到构建产物：$built" }

    $size = (Get-Item -LiteralPath $built).Length
    Write-Host ("==> 构建完成：{0}（{1:N1} MB）" -f $built, ($size / 1MB))

    if ($Run) {
        Write-Host '==> 启动托盘'
        Start-Process -FilePath $built
    }
}
finally {
    Pop-Location
}
