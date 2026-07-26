<#
  check-release-size.ps1 — fail closed when a release artifact exceeds the
  tracked installer, raw Rust-payload, or raw ImageMagick-payload budget.

  Usage:
    pwsh scripts/check-release-size.ps1 -InstallerPath dist/SageThumbs2K-Setup-<version>.exe
    pwsh scripts/check-release-size.ps1 -InstallerPath ... -StagePath packaging/stage

  The installer limit catches download growth. The optional staged-payload limit
  catches duplicated static code/data that solid compression can partially hide.
  There is deliberately no environment-variable or force bypass: an intentional
  budget increase must change packaging/size-budget.json in reviewable source.
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$InstallerPath,

    [string]$PolicyPath,

    [string]$StagePath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
if (-not $PolicyPath) {
    $PolicyPath = Join-Path $root 'packaging\size-budget.json'
}

function Read-RequiredText([object]$object, [string]$name) {
    $property = $object.PSObject.Properties[$name]
    if ($null -eq $property -or $property.Value -isnot [string] -or
        [string]::IsNullOrWhiteSpace($property.Value)) {
        throw "size policy '$PolicyPath' requires a non-empty string '$name'"
    }
    return $property.Value
}

function Read-RequiredInt64([object]$object, [string]$name) {
    $property = $object.PSObject.Properties[$name]
    if ($null -eq $property) {
        throw "size policy '$PolicyPath' is missing '$name'"
    }

    $value = $property.Value
    if ($value -is [bool] -or $value -is [string] -or $null -eq $value) {
        throw "size policy '$PolicyPath' requires integer '$name'"
    }

    try {
        $integer = [Convert]::ToInt64($value, [Globalization.CultureInfo]::InvariantCulture)
        $decimal = [Convert]::ToDecimal($value, [Globalization.CultureInfo]::InvariantCulture)
    } catch {
        throw "size policy '$PolicyPath' has invalid integer '$name': $value"
    }

    if ($integer -lt 0 -or $decimal -ne [decimal]$integer) {
        throw "size policy '$PolicyPath' requires non-negative integer '$name'"
    }
    return [int64]$integer
}

function Format-Size([int64]$bytes) {
    return ('{0:N0} bytes ({1:N3} MiB)' -f $bytes, ($bytes / 1MB))
}

if (-not (Test-Path -LiteralPath $PolicyPath -PathType Leaf)) {
    throw "release size policy not found: $PolicyPath"
}

try {
    $policy = Get-Content -LiteralPath $PolicyPath -Raw | ConvertFrom-Json
} catch {
    throw "release size policy is not valid JSON: $PolicyPath`n$($_.Exception.Message)"
}
if ($null -eq $policy) {
    throw "release size policy is empty: $PolicyPath"
}

$schemaVersion = Read-RequiredInt64 $policy 'schemaVersion'
if ($schemaVersion -ne 2) {
    throw "unsupported release size policy schemaVersion $schemaVersion (expected 2)"
}

$referenceVersion = Read-RequiredText $policy 'referenceVersion'
$referenceSha256 = Read-RequiredText $policy 'referenceInstallerSha256'
$rationale = Read-RequiredText $policy 'rationale'
if ($referenceSha256 -notmatch '^[0-9a-fA-F]{64}$') {
    throw "size policy '$PolicyPath' has invalid referenceInstallerSha256"
}

$referenceInstaller = Read-RequiredInt64 $policy 'referenceInstallerBytes'
$installerAllowance = Read-RequiredInt64 $policy 'installerGrowthAllowanceBytes'
$maxInstaller = Read-RequiredInt64 $policy 'maxInstallerBytes'
$referenceRust = Read-RequiredInt64 $policy 'referenceRustPayloadBytes'
$rustAllowance = Read-RequiredInt64 $policy 'rustPayloadGrowthAllowanceBytes'
$maxRust = Read-RequiredInt64 $policy 'maxRustPayloadBytes'
$referenceMagick = Read-RequiredInt64 $policy 'referenceMagickPayloadBytes'
$magickAllowance = Read-RequiredInt64 $policy 'magickPayloadGrowthAllowanceBytes'
$maxMagick = Read-RequiredInt64 $policy 'maxMagickPayloadBytes'

if ($referenceInstaller -gt [int64]::MaxValue - $installerAllowance -or
    $maxInstaller -ne $referenceInstaller + $installerAllowance) {
    throw "size policy installer maximum must equal referenceInstallerBytes + installerGrowthAllowanceBytes"
}
if ($referenceRust -gt [int64]::MaxValue - $rustAllowance -or
    $maxRust -ne $referenceRust + $rustAllowance) {
    throw "size policy Rust maximum must equal referenceRustPayloadBytes + rustPayloadGrowthAllowanceBytes"
}
if ($referenceMagick -gt [int64]::MaxValue - $magickAllowance -or
    $maxMagick -ne $referenceMagick + $magickAllowance) {
    throw "size policy ImageMagick maximum must equal referenceMagickPayloadBytes + magickPayloadGrowthAllowanceBytes"
}

if (-not (Test-Path -LiteralPath $InstallerPath -PathType Leaf)) {
    throw "release installer not found: $InstallerPath"
}
$installer = Get-Item -LiteralPath $InstallerPath
$installerBytes = [int64]$installer.Length
$installerDelta = $installerBytes - $referenceInstaller
$installerHeadroom = $maxInstaller - $installerBytes

Write-Host "[size] policy reference: $referenceVersion ($referenceSha256)" -ForegroundColor DarkGray
Write-Host "[size] installer: $(Format-Size $installerBytes)" -ForegroundColor Cyan
Write-Host "[size] installer limit: $(Format-Size $maxInstaller)" -ForegroundColor DarkGray

$violations = @()
if ($installerHeadroom -lt 0) {
    $violations += "installer exceeds its limit by $(Format-Size (-$installerHeadroom))"
    Write-Host "[size] installer OVER by $(Format-Size (-$installerHeadroom))" -ForegroundColor Red
} else {
    Write-Host "[size] installer headroom: $(Format-Size $installerHeadroom) (delta from reference: $installerDelta bytes)" -ForegroundColor Green
}

if ($StagePath) {
    if (-not (Test-Path -LiteralPath $StagePath -PathType Container)) {
        throw "release stage directory not found: $StagePath"
    }

    $rustNames = @('sagethumbs2k.dll', 'SageThumbs2K.exe', 'st2k.exe')
    $rustBytes = [int64]0
    foreach ($name in $rustNames) {
        $path = Join-Path $StagePath $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "release stage is missing required Rust artifact: $path"
        }
        $length = [int64](Get-Item -LiteralPath $path).Length
        if ($rustBytes -gt [int64]::MaxValue - $length) {
            throw 'staged Rust payload byte count overflowed Int64'
        }
        $rustBytes += $length
        Write-Host ("[size]   {0,-18} {1}" -f $name, (Format-Size $length)) -ForegroundColor DarkGray
    }

    $rustDelta = $rustBytes - $referenceRust
    $rustHeadroom = $maxRust - $rustBytes
    Write-Host "[size] Rust payload: $(Format-Size $rustBytes)" -ForegroundColor Cyan
    Write-Host "[size] Rust payload limit: $(Format-Size $maxRust)" -ForegroundColor DarkGray
    if ($rustHeadroom -lt 0) {
        $violations += "Rust payload exceeds its limit by $(Format-Size (-$rustHeadroom))"
        Write-Host "[size] Rust payload OVER by $(Format-Size (-$rustHeadroom))" -ForegroundColor Red
    } else {
        Write-Host "[size] Rust payload headroom: $(Format-Size $rustHeadroom) (delta from reference: $rustDelta bytes)" -ForegroundColor Green
    }

    $magickPath = Join-Path $StagePath 'magick'
    if (Test-Path -LiteralPath $magickPath -PathType Container) {
        $magickBytes = [int64]0
        Get-ChildItem -LiteralPath $magickPath -Recurse -File | ForEach-Object {
            if ($magickBytes -gt [int64]::MaxValue - $_.Length) {
                throw 'staged ImageMagick payload byte count overflowed Int64'
            }
            $magickBytes += $_.Length
        }
        $magickDelta = $magickBytes - $referenceMagick
        $magickHeadroom = $maxMagick - $magickBytes
        Write-Host "[size] ImageMagick payload: $(Format-Size $magickBytes)" -ForegroundColor Cyan
        Write-Host "[size] ImageMagick payload limit: $(Format-Size $maxMagick)" -ForegroundColor DarkGray
        if ($magickHeadroom -lt 0) {
            $violations += "ImageMagick payload exceeds its limit by $(Format-Size (-$magickHeadroom))"
            Write-Host "[size] ImageMagick payload OVER by $(Format-Size (-$magickHeadroom))" -ForegroundColor Red
        } else {
            Write-Host "[size] ImageMagick payload headroom: $(Format-Size $magickHeadroom) (delta from reference: $magickDelta bytes)" -ForegroundColor Green
        }
    } else {
        Write-Host '[size] ImageMagick payload: not staged (Compact build)' -ForegroundColor DarkGray
    }
}

if ($violations.Count -gt 0) {
    throw "release size budget FAILED: $($violations -join '; '). Policy rationale: $rationale"
}

Write-Host '[size] release size budget passed.' -ForegroundColor Green
