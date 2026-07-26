<#
  Dependency-free regression tests for check-installer.ps1. These exercise the
  exact real installer plus mutations that must fail closed.
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$lint = Join-Path $PSScriptRoot 'check-installer.ps1'
$installer = Join-Path $root 'packaging\installer.iss'
$scratch = Join-Path (
    [IO.Path]::GetTempPath()
) ("st2k-installer-lint-" + [guid]::NewGuid().ToString('N'))
$script:passed = 0

function Invoke-InstallerLint {
    param(
        [Parameter(Mandatory)]
        [string]$IssPath,

        [string]$ManagedPayloadPath,

        [string]$CorePolicyPath
    )

    $arguments = @('-NoProfile', '-File', $lint, '-IssPath', $IssPath)
    if ($ManagedPayloadPath) {
        $arguments += @('-ManagedPayloadPath', $ManagedPayloadPath)
    }
    if ($CorePolicyPath) {
        $arguments += @('-CorePolicyPath', $CorePolicyPath)
    }
    & pwsh @arguments *> $null
    return $LASTEXITCODE
}

function Assert-LintPasses([string]$Name, [scriptblock]$Body) {
    $code = & $Body
    if ($code -ne 0) {
        throw "expected installer lint PASS for '$Name', got exit $code"
    }
    Write-Host "  PASS  $Name" -ForegroundColor Green
    $script:passed++
}

function Assert-LintFails([string]$Name, [scriptblock]$Body) {
    $code = & $Body
    if ($code -eq 0) {
        throw "expected installer lint FAILURE for '$Name'"
    }
    Write-Host "  PASS  $Name (failed closed)" -ForegroundColor Green
    $script:passed++
}

New-Item -ItemType Directory -Path $scratch | Out-Null
try {
    $source = Get-Content -LiteralPath $installer -Raw

    Assert-LintPasses 'real installer exact cleanup allowlist' {
        Invoke-InstallerLint -IssPath $installer
    }

    $payload = Join-Path $scratch 'managed-payload'
    New-Item -ItemType Directory -Path (Join-Path $payload 'modules') -Force | Out-Null
    foreach ($name in @(
            'magick.exe',
            'CORE_RL_test_.dll',
            'mfc140u.dll',
            'msvcp140.dll',
            'vcomp140.dll',
            'vcruntime140_1.dll',
            'colors.xml',
            'configure.xml',
            'delegates.xml',
            'english.xml',
            'locale.xml',
            'log.xml',
            'mime.xml',
            'policy.xml',
            'thresholds.xml',
            'type-ghostscript.xml',
            'type.xml',
            'License.txt',
            'NOTICE.txt'
        )) {
        [IO.File]::WriteAllBytes((Join-Path $payload $name), [byte[]](1))
    }
    $corePolicy = Join-Path $scratch 'core-policy.xml'
    Copy-Item -LiteralPath (Join-Path $payload 'policy.xml') -Destination $corePolicy
    Assert-LintPasses 'staged payload coverage and identical core policy' {
        Invoke-InstallerLint `
            -IssPath $installer `
            -ManagedPayloadPath $payload `
            -CorePolicyPath $corePolicy
    }

    $missing = Join-Path $scratch 'missing-cleanup.iss'
    $needle = 'Type: files; Name: "{app}\policy.xml"'
    $mutated = $source.Replace($needle, '')
    if ($mutated -ceq $source) { throw 'test mutation did not remove policy.xml cleanup' }
    Set-Content -LiteralPath $missing -Value $mutated -Encoding utf8
    Assert-LintFails 'missing managed cleanup entry' {
        Invoke-InstallerLint -IssPath $missing
    }

    $broad = Join-Path $scratch 'broad-cleanup.iss'
    $mutated = $source.Replace(
        '[Files]',
        "Type: filesandordirs; Name: `"{app}\*`"`r`n`r`n[Files]"
    )
    if ($mutated -ceq $source) { throw 'test mutation did not add broad cleanup' }
    Set-Content -LiteralPath $broad -Value $mutated -Encoding utf8
    Assert-LintFails 'broad application-directory cleanup entry' {
        Invoke-InstallerLint -IssPath $broad
    }

    $missingCorePolicy = Join-Path $scratch 'missing-core-policy.iss'
    $needle =
        'Source: "stage\policy.xml"; DestDir: "{app}"; Flags: ignoreversion; Components: core'
    $mutated = $source.Replace($needle, '')
    if ($mutated -ceq $source) { throw 'test mutation did not remove core policy mapping' }
    Set-Content -LiteralPath $missingCorePolicy -Value $mutated -Encoding utf8
    Assert-LintFails 'Compact/core hardened policy mapping removed' {
        Invoke-InstallerLint -IssPath $missingCorePolicy
    }

    $duplicatePolicy = Join-Path $scratch 'duplicate-policy.iss'
    $needle =
        'Source: "stage\magick\*"; DestDir: "{app}"; Excludes: "policy.xml"; Flags: ignoreversion recursesubdirs createallsubdirs; Components: magick'
    $replacement =
        'Source: "stage\magick\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs; Components: magick'
    $mutated = $source.Replace($needle, $replacement)
    if ($mutated -ceq $source) { throw 'test mutation did not remove policy exclusion' }
    Set-Content -LiteralPath $duplicatePolicy -Value $mutated -Encoding utf8
    Assert-LintFails 'bundled Magick row no longer excludes duplicate policy' {
        Invoke-InstallerLint -IssPath $duplicatePolicy
    }

    $unexpected = Join-Path $payload 'unexpected-third-party.dat'
    [IO.File]::WriteAllBytes($unexpected, [byte[]](1))
    Assert-LintFails 'staged basename outside cleanup allowlist' {
        Invoke-InstallerLint `
            -IssPath $installer `
            -ManagedPayloadPath $payload `
            -CorePolicyPath $corePolicy
    }
    Remove-Item -LiteralPath $unexpected -Force

    [IO.File]::WriteAllBytes($corePolicy, [byte[]](2))
    Assert-LintFails 'core and bundled hardened policies diverge' {
        Invoke-InstallerLint `
            -IssPath $installer `
            -ManagedPayloadPath $payload `
            -CorePolicyPath $corePolicy
    }

    $unsafeForm = Join-Path $scratch 'unsafe-form.iss'
    Set-Content -LiteralPath $unsafeForm -Value (
        $source + "`r`nprocedure LintRegression;`r`nbegin`r`n" +
        "  F := TSetupForm.Create(nil);`r`nend;`r`n"
    ) -Encoding utf8
    Assert-LintFails 'resource-dependent uninstaller form constructor' {
        Invoke-InstallerLint -IssPath $unsafeForm
    }

    Write-Host "[installer-lint-test] ALL GREEN ($script:passed cases)" -ForegroundColor Green
} finally {
    if (Test-Path -LiteralPath $scratch) {
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}

# Assert-LintFails deliberately ends by running a native command that must fail. GitHub's pwsh
# step observes that expected command's LASTEXITCODE even though all assertions passed, so make the
# test harness's successful result explicit.
exit 0
