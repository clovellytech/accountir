$ErrorActionPreference = "Stop"

$Repo = "zakvdm/accountir"
$Target = "x86_64-pc-windows-msvc"
$InstallDir = "$env:LOCALAPPDATA\Programs\accountir"

Write-Host "Fetching latest release..."
$Release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
$Version = $Release.tag_name
Write-Host "Latest version: $Version"

$Archive = "accountir-$Version-$Target.zip"
$Url = "https://github.com/$Repo/releases/download/$Version/$Archive"

$TmpDir = New-TemporaryFile | ForEach-Object {
    Remove-Item $_
    New-Item -ItemType Directory -Path $_
}

try {
    Write-Host "Downloading $Url..."
    $ArchivePath = Join-Path $TmpDir $Archive
    Invoke-WebRequest -Uri $Url -OutFile $ArchivePath

    Write-Host "Installing to $InstallDir..."
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Expand-Archive -Path $ArchivePath -DestinationPath $InstallDir -Force

    # Add to user PATH if not already present
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$InstallDir;$UserPath", "User")
        Write-Host "Added $InstallDir to user PATH (restart your terminal to use)"
    }

    Write-Host "Installed accountir $Version to $InstallDir\accountir.exe"
}
finally {
    Remove-Item -Recurse -Force $TmpDir
}
