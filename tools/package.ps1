param(
    [switch]$SkipBuild,
    [switch]$KeepStaging
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $manifest = Get-Content 'Cargo.toml' -Raw
    if ($manifest -notmatch '(?m)^version\s*=\s*"([^"]+)"') { throw 'version not found in Cargo.toml' }
    $version = $Matches[1]

    if (-not $SkipBuild) {
        Write-Host "building dsh-launcher $version (static CRT) ..." -ForegroundColor Cyan
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed: $LASTEXITCODE" }
    }

    $exe = Join-Path $root 'target/release/dsh-launcher.exe'
    if (-not (Test-Path $exe)) { throw "missing $exe" }

    $name = "dsh-launcher-$version-win64"
    # Stage outside the repo: an installed copy may be the working directory of a
    # running service, and Windows refuses to delete (or lock-free) such a folder.
    $stageRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("dsh-launcher-pack-{0}" -f $PID)
    $stage = Join-Path $stageRoot $name
    if (Test-Path $stageRoot) { Remove-Item $stageRoot -Recurse -Force -ErrorAction SilentlyContinue }
    New-Item -ItemType Directory -Path $stage -Force | Out-Null

    Copy-Item $exe (Join-Path $stage 'dsh-launcher.exe')
    Copy-Item (Join-Path $root 'packaging/README.txt') (Join-Path $stage 'README.txt')
    $preview = Join-Path $root 'docs/preview.png'
    if (Test-Path $preview) { Copy-Item $preview (Join-Path $stage 'preview.png') }

    $zip = Join-Path $root "dist/$name.zip"
    if (Test-Path $zip) { Remove-Item $zip -Force }
    Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
    $mirror = Join-Path $root "dist/$name"
    if (-not (Test-Path $mirror)) { Copy-Item $stage $mirror -Recurse -ErrorAction SilentlyContinue }

    $exeSize = [math]::Round((Get-Item $exe).Length / 1MB, 2)
    $zipSize = [math]::Round((Get-Item $zip).Length / 1MB, 2)
    Write-Host ""
    Write-Host ("exe : {0}  ({1} MB)" -f $exe, $exeSize) -ForegroundColor Green
    Write-Host ("zip : {0}  ({1} MB)" -f $zip, $zipSize) -ForegroundColor Green
    Get-ChildItem $stage | ForEach-Object { Write-Host ("  {0,-20} {1,8} B" -f $_.Name, $_.Length) }
    if (-not $KeepStaging) {
        Write-Host "(staging folder kept for inspection: $stage)" -ForegroundColor DarkGray
    }
}
finally {
    Pop-Location
}
