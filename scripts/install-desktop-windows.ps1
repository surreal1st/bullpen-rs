# Idempotent: copy latest client.exe to Programs and refresh Desktop shortcut.
# Run from repo root: powershell -File scripts/install-desktop-windows.ps1
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$SourceDir = Join-Path $RepoRoot 'target\dx\client\release\windows\app'
$Source = Join-Path $SourceDir 'client.exe'
$SourceAssets = Join-Path $SourceDir 'assets'
$DestDir = Join-Path $env:LOCALAPPDATA 'Programs\Bullpen-rs'
$DestExe = Join-Path $DestDir 'Bullpen.exe'
$DestAssets = Join-Path $DestDir 'assets'

if (-not (Test-Path -LiteralPath $Source)) {
    Write-Error "Missing build output: $Source`nRun: bash scripts/build-desktop.sh"
}
if (-not (Test-Path -LiteralPath $SourceAssets)) {
    Write-Error "Missing bundled assets: $SourceAssets`nRun: bash scripts/build-desktop.sh"
}

New-Item -ItemType Directory -Force -Path $DestDir | Out-Null
Copy-Item -LiteralPath $Source -Destination $DestExe -Force
# Dioxus desktop serves /assets/* from $exeDir/assets/ (see dioxus-asset-resolver
# get_asset_root). Copying only the exe leaves WebView2 unstyled — PR #4 CSS never loads.
if (Test-Path -LiteralPath $DestAssets) {
    Remove-Item -LiteralPath $DestAssets -Recurse -Force
}
Copy-Item -LiteralPath $SourceAssets -Destination $DestAssets -Recurse -Force

$Desktop = [Environment]::GetFolderPath('Desktop')
$ShortcutPath = Join-Path $Desktop 'Bullpen.lnk'
$Wsh = New-Object -ComObject WScript.Shell
$Lnk = $Wsh.CreateShortcut($ShortcutPath)
$Lnk.TargetPath = $DestExe
$Lnk.WorkingDirectory = $DestDir
$Lnk.Description = 'Bullpen (rust native desktop)'
$Lnk.Save()

Write-Host "Installed: $DestExe"
Write-Host "Assets:    $DestAssets"
Write-Host "Shortcut:  $ShortcutPath"
