# Drives install.ps1 on Windows against the bundle from desktop/bundle.sh, with copies for downloads,
# then opens the installed program.
$ErrorActionPreference = 'Stop'
$bundle = Join-Path (Split-Path $PSScriptRoot) 'target\bundle'
$installer = Join-Path (Split-Path $PSScriptRoot) 'install.ps1'
$app = Join-Path $env:LOCALAPPDATA 'Programs\Kalipso'
$badChecksum = $false # Read by the stub below, in a scope install.ps1 is calling it from.

function Invoke-RestMethod { [pscustomobject]@{ tag_name = 'dev' } }
function Invoke-WebRequest {
    param([Parameter(Position = 0)][string]$Uri, [string]$OutFile, [switch]$UseBasicParsing)
    if ($badChecksum -and $Uri -like '*.sha256') {
        Set-Content $OutFile '0000  bad.zip'
    } else {
        Copy-Item (Join-Path $bundle $Uri.Split('/')[-1]) $OutFile
    }
}

$script:failed = $false
function Check([string]$name, [bool]$ok) {
    Write-Host "$(if ($ok) { 'ok   ' } else { 'FAIL ' }) $name"
    if (-not $ok) { $script:failed = $true }
}
function UserPath { (Get-Item HKCU:\Environment).GetValue('Path', '', 'DoNotExpandEnvironmentNames') -split ';' }

& $installer
Check 'installs the program' (Test-Path (Join-Path $app 'kalipso.exe'))
Check 'adds a Start menu entry' (Test-Path (Join-Path ([Environment]::GetFolderPath('Programs')) 'Kalipso.lnk'))
Check 'adds the folder to PATH' ((UserPath) -contains $app)
Check 'leaves no download behind' (-not (Test-Path "$app.new") -and -not (Test-Path "$app.new.zip"))

$kept = Join-Path $app 'kept'
New-Item $kept | Out-Null
$badChecksum = $true
$refused = $false
try { & $installer } catch { $refused = "$_" -like '*Checksum mismatch*' }
$badChecksum = $false
Check "refuses a bad checksum and keeps what's installed" ($refused -and (Test-Path $kept))

& $installer
Check 'replaces an older install' (-not (Test-Path $kept))
Check "doesn't add it to PATH twice" (@((UserPath) | Where-Object { $_ -eq $app }).Count -eq 1)

# GitHub's runner has no OpenGL driver, which the config asks for, so WezTerm couldn't make a window.
# Only this throwaway install uses WebGpu instead, which can run on Windows' software adapter.
$config = Join-Path $app 'config\kalipso.lua'
(Get-Content -Raw $config).Replace('return config', "config.front_end = 'WebGpu'`nreturn config") | Set-Content $config

$launcher = Start-Process (Join-Path $app 'kalipso.exe') -PassThru
Start-Sleep 8
$gui = Get-Process wezterm-gui -ErrorAction SilentlyContinue
Check 'the window stays open' ($null -ne $gui)
if ($gui) {
    $shell = Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe' AND ParentProcessId = $($gui.Id)"
    Check 'it opens PowerShell' ($null -ne $shell)
} else { # Why, from WezTerm's own log.
    Write-Host "The launcher exited with $($launcher.ExitCode)."
    Get-ChildItem "$HOME\.local\share\wezterm" -Filter '*log*' -ErrorAction SilentlyContinue |
        ForEach-Object { Write-Host "--- $_"; Get-Content $_.FullName -Tail 40 | Write-Host }
}
$gui | Stop-Process -Force

if ($script:failed) { exit 1 }
