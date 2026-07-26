<#
  CI gate for the exact three production Rust artifacts. Unlike the installer
  gate, this needs no Inno Setup/ImageMagick and therefore runs on every PR.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$ArtifactDirectory,

    [string]$PolicyPath,

    [string]$ExpectedVersion
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. (Join-Path $PSScriptRoot 'release-manifest-lib.ps1')

if (-not $PolicyPath) {
    $PolicyPath = Join-Path $root 'packaging\size-budget.json'
}
if (-not $ExpectedVersion) {
    $ExpectedVersion = ([regex]::Match(
        (Get-Content (Join-Path $root 'Cargo.toml') -Raw),
        '(?m)^\s*version\s*=\s*"([^"]+)"'
    )).Groups[1].Value
}
if (-not $ExpectedVersion) {
    throw 'could not determine expected release version'
}
if (-not (Test-Path -LiteralPath $ArtifactDirectory -PathType Container)) {
    throw "release artifact directory not found: $ArtifactDirectory"
}
if (-not (Test-Path -LiteralPath $PolicyPath -PathType Leaf)) {
    throw "release size policy not found: $PolicyPath"
}

try {
    $policy = Get-Content -LiteralPath $PolicyPath -Raw | ConvertFrom-Json
} catch {
    throw "release size policy is not valid JSON: $PolicyPath`n$($_.Exception.Message)"
}
$maxProperty = $policy.PSObject.Properties['maxRustPayloadBytes']
if ($null -eq $maxProperty -or $maxProperty.Value -is [bool] -or
    $maxProperty.Value -is [string]) {
    throw "release size policy requires integer 'maxRustPayloadBytes'"
}
$maxRust = [Convert]::ToInt64($maxProperty.Value, [Globalization.CultureInfo]::InvariantCulture)
if ($maxRust -lt 0) {
    throw "release size policy requires non-negative integer 'maxRustPayloadBytes'"
}

$total = [int64]0
foreach ($name in 'sagethumbs2k.dll', 'SageThumbs2K.exe', 'st2k.exe') {
    $path = Join-Path $ArtifactDirectory $name
    Assert-ReleasePeMetadata -Path $path -Version $ExpectedVersion
    $length = [int64](Get-Item -LiteralPath $path).Length
    if ($total -gt [int64]::MaxValue - $length) {
        throw 'release Rust payload byte count overflowed Int64'
    }
    $total += $length
    Write-Host ("[size-ci] {0,-18} {1:N0} bytes" -f $name, $length) -ForegroundColor DarkGray
}

Write-Host ("[size-ci] Rust payload: {0:N0} bytes; limit: {1:N0} bytes" -f $total, $maxRust) -ForegroundColor Cyan
if ($total -gt $maxRust) {
    throw ("production Rust payload exceeds its limit by {0:N0} bytes" -f ($total - $maxRust))
}
Write-Host ("[size-ci] Rust payload headroom: {0:N0} bytes" -f ($maxRust - $total)) -ForegroundColor Green
