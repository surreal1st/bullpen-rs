# Idempotent: copy latest client.exe to Programs and refresh Desktop shortcut.
# Run from repo root: powershell -File scripts/install-desktop-windows.ps1
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$Source = Join-Path $RepoRoot 'target\dx\client\release\windows\app\client.exe'
$DestDir = Join-Path $env:LOCALAPPDATA 'Programs\Bullpen-rs'
$DestExe = Join-Path $DestDir 'Bullpen.exe'

if (-not (Test-Path -LiteralPath $Source)) {
    Write-Error "Missing build output: $Source`nRun: bash scripts/build-desktop.sh"
}

New-Item -ItemType Directory -Force -Path $DestDir | Out-Null
Copy-Item -LiteralPath $Source -Destination $DestExe -Force

$Desktop = [Environment]::GetFolderPath('Desktop')
$ShortcutPath = Join-Path $Desktop 'Bullpen.lnk'
$Wsh = New-Object -ComObject WScript.Shell
$Lnk = $Wsh.CreateShortcut($ShortcutPath)
$Lnk.TargetPath = $DestExe
$Lnk.WorkingDirectory = $DestDir
$Lnk.Description = 'Bullpen (rust native desktop)'
$Lnk.Save()

Write-Host "Installed: $DestExe"
Write-Host "Shortcut:  $ShortcutPath"
