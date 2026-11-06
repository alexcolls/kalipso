# Installs the latest Kalipso program on Windows, with a Start menu entry and the kalipso command.
#
#   irm https://raw.githubusercontent.com/alexcolls/kalipso/main/install.ps1 | iex

# In a script block, so `iex` leaves no variables or preferences behind in the user's session.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue' # Its progress bar slows downloads a lot.
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

    $repo = 'alexcolls/kalipso'
    $tag = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
    $name = "kalipso-$tag-x86_64-windows"
    $url = "https://github.com/$repo/releases/download/$tag/$name.zip"
    $app = Join-Path $env:LOCALAPPDATA 'Programs\Kalipso'
    $unpacked = "$app.new"
    $zip = "$unpacked.zip"

    New-Item -ItemType Directory -Force (Split-Path $app) | Out-Null
    try {
        try {
            Invoke-WebRequest $url -OutFile $zip -UseBasicParsing
            Invoke-WebRequest "$url.sha256" -OutFile "$zip.sha256" -UseBasicParsing
        } catch {
            # Each OS's build is uploaded a few minutes after the release appears.
            throw "Couldn't download $name.zip. If $tag came out in the last few minutes, try again soon. ($_)"
        }
        $expected = (Get-Content "$zip.sha256" -Raw).Trim().Split(' ')[0]
        if ((Get-FileHash $zip -Algorithm SHA256).Hash -ne $expected) {
            throw "Checksum mismatch for $name.zip."
        }

        if (Test-Path $unpacked) { Remove-Item -Recurse -Force $unpacked }
        Expand-Archive $zip $unpacked
        if (Test-Path $app) {
            try {
                Remove-Item -Recurse -Force $app
            } catch {
                throw "Couldn't replace $app. Close any open Kalipso windows, then run this again. ($_)"
            }
        }
        Move-Item (Join-Path $unpacked $name) $app
    } finally {
        foreach ($leftover in $unpacked, $zip, "$zip.sha256") {
            if (Test-Path $leftover) { Remove-Item -Recurse -Force $leftover }
        }
    }

    $shortcut = (New-Object -ComObject WScript.Shell).CreateShortcut(
        (Join-Path ([Environment]::GetFolderPath('Programs')) 'Kalipso.lnk'))
    $shortcut.TargetPath = Join-Path $app 'kalipso.exe'
    $shortcut.IconLocation = (Join-Path $app 'wezterm\wezterm-gui.exe') + ',0'
    $shortcut.WorkingDirectory = $env:USERPROFILE
    $shortcut.Description = 'Futuristic open-source terminal'
    $shortcut.Save()

    # Read and written raw: expanding it here would bake in values like %USERPROFILE%.
    $path = (Get-Item HKCU:\Environment).GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    if (($path -split ';') -notcontains $app) {
        $path = ($path.TrimEnd(';') + ";$app").TrimStart(';')
        Set-ItemProperty HKCU:\Environment Path $path -Type ExpandString
        # Setting any variable this way tells running programs that the environment changed.
        [Environment]::SetEnvironmentVariable('KALIPSO_INSTALLED', '1', 'User')
        [Environment]::SetEnvironmentVariable('KALIPSO_INSTALLED', $null, 'User')
        Write-Host "Added $app to your PATH. Open a new terminal to use it."
    }

    Write-Host "Installed Kalipso $tag to $app"
    Write-Host 'Open it from the Start menu, or run: kalipso [directory]'
}
