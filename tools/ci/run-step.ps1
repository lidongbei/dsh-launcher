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

# A failing command may print nothing at all, so never assume the log exists.
New-Item -ItemType File -Path $Log -Force | Out-Null

Write-Host "::group::$Label"
$code = 0
try {
    Invoke-Expression $CommandLine 2>&1 | Tee-Object -FilePath $Log -Append
    $code = $LASTEXITCODE
    if ($null -eq $code) { $code = 0 }
}
catch {
    $message = $_ | Out-String
    Write-Host $message
    Add-Content -Path $Log -Value $message
    if ($code -eq 0) { $code = 1 }
}
Write-Host "::endgroup::"

if ($code -ne 0) {
    foreach ($line in (Get-Content -Path $Log -Tail 60)) {
        $text = $line -replace '%', '%25' -replace "`r", '' -replace "`n", '%0A'
        if ($text.Trim().Length -eq 0) { continue }
        Write-Host "::error title=$Label::$text"
    }
    Write-Host "::error title=$Label::$CommandLine exited with $code"
    exit $code
}

Write-Host "$Label ok"
