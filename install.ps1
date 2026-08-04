$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repository = if ($env:RONIN_REPOSITORY) { $env:RONIN_REPOSITORY } else { "Binary-Brawlers/ronin-cli" }
$Version = if ($env:RONIN_VERSION) { $env:RONIN_VERSION } else { "latest" }
$InstallDir = if ($env:RONIN_INSTALL_DIR) { $env:RONIN_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Ronin\bin" }

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "ronin: 32-bit Windows is not supported"
}
$Target = "x86_64-pc-windows-msvc"
$Archive = "ronin-$Target.zip"
$Base = if ($Version -eq "latest") {
    "https://github.com/$Repository/releases/latest/download"
} else {
    "https://github.com/$Repository/releases/download/$Version"
}

$TempDir = Join-Path ([IO.Path]::GetTempPath()) ("ronin-install-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $TempDir | Out-Null
try {
    $ArchivePath = Join-Path $TempDir $Archive
    $ManifestPath = Join-Path $TempDir "SHA256SUMS"
    Invoke-WebRequest -UseBasicParsing "$Base/$Archive" -OutFile $ArchivePath
    Invoke-WebRequest -UseBasicParsing "$Base/SHA256SUMS" -OutFile $ManifestPath

    $Entries = @(Get-Content $ManifestPath | ForEach-Object {
        if ($_ -match '^([0-9a-fA-F]{64})\s+\*?(.+)$' -and $Matches[2] -eq $Archive) {
            $Matches[1].ToLowerInvariant()
        }
    })
    if ($Entries.Count -ne 1) {
        throw "ronin: checksum manifest has no unique entry for $Archive"
    }
    $Actual = (Get-FileHash $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($Actual -ne $Entries[0]) {
        throw "ronin: downloaded archive failed checksum verification"
    }

    Expand-Archive -Path $ArchivePath -DestinationPath $TempDir -Force
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item (Join-Path $TempDir "ronin.exe") (Join-Path $InstallDir "ronin.exe") -Force
} finally {
    Remove-Item $TempDir -Recurse -Force -ErrorAction SilentlyContinue
}

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
$PathEntries = @($UserPath -split ';' | Where-Object { $_ })
if ($PathEntries -notcontains $InstallDir) {
    $NewPath = (@($PathEntries) + $InstallDir) -join ';'
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
}
if (($env:Path -split ';') -notcontains $InstallDir) {
    $env:Path = "$InstallDir;$env:Path"
}

Write-Host "Installed ronin to $(Join-Path $InstallDir 'ronin.exe')"
Write-Host "Open a new terminal, then run: ronin login"
