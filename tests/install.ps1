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

Start-Process (Join-Path $app 'kalipso.exe')
Start-Sleep 8
$gui = Get-Process wezterm-gui -ErrorAction SilentlyContinue
Check 'the window stays open' ($null -ne $gui)
$gui | Stop-Process -Force

if ($script:failed) { exit 1 }
