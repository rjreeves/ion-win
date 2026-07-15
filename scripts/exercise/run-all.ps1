$ErrorActionPreference = "Stop"

$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$bin = Join-Path $root "target\debug\ion-win.exe"
$appData = Join-Path $root "target\exercise-appdata"

Set-Location $root
New-Item -ItemType Directory -Force -Path (Join-Path $root "target") | Out-Null
New-Item -ItemType Directory -Force -Path $appData | Out-Null

cargo build

$env:APPDATA = $appData
$scripts = Get-ChildItem -Path $PSScriptRoot -Filter "*.ion" | Sort-Object Name

foreach ($script in $scripts) {
    Write-Host ""
    Write-Host "===== $($script.Name) ====="
    if ($script.Name -eq "09_read_args_env.ion") {
        "first-token rest of the input line" | & $bin $script.FullName arg-one arg-two
    } else {
        & $bin $script.FullName
    }
}
