<#
Drives a real Windows console-control-event Ctrl+C at a spawned ion-win.exe
process, from outside the process itself (i.e. the same mechanism a real
terminal uses when a user presses Ctrl+C), rather than only unit-testing
jobctl's in-process flag/registry logic.

Usage: pwsh -File ctrlc_probe.ps1 <script.ion> <outputFile>
#>
param(
    [Parameter(Mandatory = $true)] [string]$ScriptName,
    [Parameter(Mandatory = $true)] [string]$OutFile
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSCommandPath
$exe = Join-Path $root '..\..\target\debug\ion-win.exe' | Resolve-Path
$scriptPath = Join-Path $root $ScriptName
$outPath = Join-Path $root $OutFile
if (Test-Path $outPath) { Remove-Item $outPath -Force }

Add-Type @'
using System;
using System.Runtime.InteropServices;

public static class ConsoleCtl {
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool AttachConsole(uint dwProcessId);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool FreeConsole();

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GenerateConsoleCtrlEvent(uint dwCtrlEvent, uint dwProcessGroupId);

    public delegate bool HandlerRoutine(uint dwCtrlType);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool SetConsoleCtrlHandler(HandlerRoutine HandlerRoutine, bool Add);

    public const uint CREATE_NEW_CONSOLE = 0x00000010;
    public const uint CTRL_C_EVENT = 0;
}
'@

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe.Path
$psi.Arguments = "`"$scriptPath`""
$psi.WorkingDirectory = $root
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $false
# Force a brand-new console so this process is a fresh console-owning group,
# just like a real interactive ion-win session would be — not sharing this
# PowerShell harness's own console.
$startInfoField = $psi.GetType().GetProperty('StandardOutputEncoding')
$proc = New-Object System.Diagnostics.Process
$proc.StartInfo = $psi

# .NET's ProcessStartInfo has no direct CREATE_NEW_CONSOLE knob, so fall back
# to cmd.exe's "start" which always spawns a new console window for a
# console subsystem exe.
$psi.FileName = 'cmd.exe'
$psi.Arguments = "/c start `"ion-win-probe`" /D `"$root`" `"$($exe.Path)`" `"$scriptPath`""
$proc.StartInfo = $psi
$proc.Start() | Out-Null

# Give the script time to reach the loop / spawn ping before we look for it.
Start-Sleep -Milliseconds 1200

$target = Get-Process -Name 'ion-win' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $target) {
    Write-Output "RESULT:NOPROC"
    exit 1
}
$targetPid = $target.Id

# Make this harness process immune to the Ctrl+C event we're about to
# broadcast — without this, GenerateConsoleCtrlEvent would also terminate
# this PowerShell process once it's attached to the target's console.
[ConsoleCtl]::SetConsoleCtrlHandler($null, $true) | Out-Null

[ConsoleCtl]::AttachConsole([uint32]$targetPid) | Out-Null
[ConsoleCtl]::GenerateConsoleCtrlEvent([ConsoleCtl]::CTRL_C_EVENT, 0) | Out-Null
Start-Sleep -Milliseconds 300
[ConsoleCtl]::FreeConsole() | Out-Null

$exited = $target.WaitForExit(5000)
if (-not $exited) {
    Write-Output "RESULT:TIMEOUT exitcode=n/a stillrunning=$true"
    Stop-Process -Id $targetPid -Force -ErrorAction SilentlyContinue
    exit 1
}

Start-Sleep -Milliseconds 200
$outContent = if (Test-Path $outPath) { Get-Content $outPath -Raw } else { '<missing>' }
Write-Output "RESULT:EXITED exitcode=$($target.ExitCode)"
Write-Output "OUTFILE:$outContent"

# Clean up any leaked external child (e.g. ping) still bound to this test.
Get-Process -Name 'PING' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
