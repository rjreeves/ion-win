[CmdletBinding()]
param(
    [switch] $Force,
    [string] $Source,
    [string] $Destination
)

$ErrorActionPreference = 'Stop'

$repositoryRoot = Split-Path -Parent $PSScriptRoot
if (-not $Source) {
    $Source = Join-Path $repositoryRoot 'certo\dist'
}
if (-not $Destination) {
    $Destination = Join-Path $env:LOCALAPPDATA 'Sym\packages\certo\latest'
}

$source = [System.IO.Path]::GetFullPath($Source)
$destination = [System.IO.Path]::GetFullPath($Destination)

if ($source.TrimEnd('\') -eq $destination.TrimEnd('\')) {
    throw 'Source and destination must be different directories.'
}
$sourcePrefix = $source.TrimEnd('\') + '\'
$destinationPrefix = $destination.TrimEnd('\') + '\'
if ($sourcePrefix.StartsWith($destinationPrefix, [System.StringComparison]::OrdinalIgnoreCase) -or
    $destinationPrefix.StartsWith($sourcePrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw 'Source and destination must not contain one another.'
}

function Get-PackageVersion {
    param([string[]] $Candidates)

    foreach ($candidate in $Candidates) {
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            continue
        }

        try {
            $value = (Get-Content -LiteralPath $candidate -Raw | ConvertFrom-Json).version
            $parsedVersion = $null
            if ($value -and [System.Version]::TryParse(($value -replace '^v', ''), [ref] $parsedVersion)) {
                return $parsedVersion
            }
        }
        catch {
            throw "Cannot read a valid version from '$candidate': $($_.Exception.Message)"
        }
    }

    return $null
}

function Get-NewestWriteTimeUtc {
    param([string] $Path)

    $item = Get-Item -LiteralPath $Path
    $newest = if ($item.PSIsContainer) { [datetime]::MinValue } else { $item.LastWriteTimeUtc }
    if ($item.PSIsContainer) {
        Get-ChildItem -LiteralPath $Path -Force -Recurse | ForEach-Object {
            if ($_.LastWriteTimeUtc -gt $newest) {
                $newest = $_.LastWriteTimeUtc
            }
        }
        if ($newest -eq [datetime]::MinValue) {
            $newest = $item.LastWriteTimeUtc
        }
    }
    return $newest
}

if (-not (Test-Path -LiteralPath $source -PathType Container)) {
    throw "Source directory does not exist: $source"
}

$sourceVersion = Get-PackageVersion @(
    (Join-Path $source 'package.json'),
    (Join-Path (Split-Path -Parent $source) 'package.json')
)

$reason = 'destination does not exist'

if (Test-Path -LiteralPath $destination) {
    if (-not (Test-Path -LiteralPath $destination -PathType Container)) {
        throw "Destination exists but is not a directory: $destination"
    }

    $destinationVersion = Get-PackageVersion @(
        (Join-Path $destination 'package.json')
    )

    if ($null -ne $sourceVersion -and $null -ne $destinationVersion) {
        $comparison = $sourceVersion.CompareTo($destinationVersion)
        if ($comparison -eq 0) {
            Write-Host "certo $sourceVersion is already installed at $destination"
            exit 0
        }
        if ($comparison -lt 0 -and -not $Force) {
            throw "Destination version $destinationVersion is newer than source version $sourceVersion. Re-run with --force to downgrade."
        }
        $reason = if ($comparison -gt 0) { "version $sourceVersion is newer than $destinationVersion" } else { "forced downgrade from $destinationVersion to $sourceVersion" }
    }
    else {
        $sourceTime = Get-NewestWriteTimeUtc $source
        $destinationTime = Get-NewestWriteTimeUtc $destination
        if ($sourceTime -eq $destinationTime) {
            Write-Host "certo is already current at $destination (timestamps match)"
            exit 0
        }
        if ($sourceTime -lt $destinationTime -and -not $Force) {
            throw "Destination is newer than source by modification time. Re-run with --force to replace it."
        }
        $reason = if ($sourceTime -gt $destinationTime) { 'source has newer files' } else { 'forced replacement of newer destination' }
    }
}

$destinationParent = Split-Path -Parent $destination
New-Item -ItemType Directory -Path $destinationParent -Force | Out-Null
$staging = Join-Path $destinationParent ('.latest.ion-staging-' + [guid]::NewGuid().ToString('N'))
$backup = Join-Path $destinationParent ('.latest.ion-backup-' + [guid]::NewGuid().ToString('N'))
$destinationMoved = $false

try {
    Copy-Item -LiteralPath $source -Destination $staging -Recurse -Force

    if (Test-Path -LiteralPath $destination) {
        Move-Item -LiteralPath $destination -Destination $backup
        $destinationMoved = $true
    }

    Move-Item -LiteralPath $staging -Destination $destination

    if ($destinationMoved) {
        Remove-Item -LiteralPath $backup -Recurse -Force
        $destinationMoved = $false
    }
}
catch {
    if ($destinationMoved -and -not (Test-Path -LiteralPath $destination)) {
        Move-Item -LiteralPath $backup -Destination $destination
        $destinationMoved = $false
    }
    throw
}
finally {
    if (Test-Path -LiteralPath $staging) {
        Remove-Item -LiteralPath $staging -Recurse -Force
    }
}

$deployedVersion = if ($null -ne $sourceVersion) { " version $sourceVersion" } else { '' }
Write-Host "Deployed certo$deployedVersion to $destination ($reason)."
