[CmdletBinding()]
param(
    [string] $InnoCompiler
)

$ErrorActionPreference = 'Stop'

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repositoryRoot 'Cargo.toml'
$setupScript = Join-Path $repositoryRoot 'setup.iss'

$manifest = Get-Content -LiteralPath $manifestPath -Raw
$packageSection = [regex]::Match(
    $manifest,
    '(?ms)^\[package\]\s*(.*?)(?=^\[|\z)'
)
$versionMatch = [regex]::Match(
    $packageSection.Groups[1].Value,
    '(?m)^version\s*=\s*"([^"]+)"\s*$'
)
if (-not $versionMatch.Success) {
    throw "Cannot read the package version from $manifestPath"
}
$version = $versionMatch.Groups[1].Value

if (-not $InnoCompiler) {
    $candidates = @(
        (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'),
        (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe'),
        (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe')
    )
    $InnoCompiler = $candidates |
        Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) } |
        Select-Object -First 1
}
if (-not $InnoCompiler -or -not (Test-Path -LiteralPath $InnoCompiler -PathType Leaf)) {
    throw 'Inno Setup 6 compiler not found. Install it or pass -InnoCompiler PATH.'
}

Push-Location $repositoryRoot
try {
    & cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release failed with exit code $LASTEXITCODE"
    }

    & $InnoCompiler "/DMyAppVersion=$version" $setupScript
    if ($LASTEXITCODE -ne 0) {
        throw "Inno Setup compilation failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
}

Write-Host "Built ion-win $version installer in $(Join-Path $repositoryRoot 'Output')"
