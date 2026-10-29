# Runs one CI gate and makes its failure readable from the public run page.
#
# GitHub only shows "Process completed with exit code 1" for a failed step, and
# the full log needs authentication. Emitting the tail of the output as
# `::error` workflow commands turns it into annotations, which anyone can read.
param(
    [Parameter(Mandatory = $true)][string]$Label,
    [Parameter(Mandatory = $true)][string]$Log,
    [Parameter(Mandatory = $true)][string]$CommandLine
)

Write-Host "::group::$Label"
Invoke-Expression $CommandLine 2>&1 | Tee-Object -FilePath $Log
$code = $LASTEXITCODE
Write-Host "::endgroup::"

if ($code -ne 0) {
    $tail = Get-Content -Path $Log -Tail 60
    foreach ($line in $tail) {
        $text = $line -replace '%', '%25' -replace "`r", '' -replace "`n", '%0A'
        if ($text.Trim().Length -eq 0) { continue }
        Write-Host "::error title=$Label::$text"
    }
    Write-Host "::error title=$Label::$CommandLine exited with $code"
    exit $code
}

Write-Host "$Label ok"
