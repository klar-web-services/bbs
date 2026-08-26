$ErrorActionPreference = 'Stop'
$Repository = if ($env:BBS_REPOSITORY) { $env:BBS_REPOSITORY } else { 'klar-web-services/bbs' }
$InstallDir = if ($env:BBS_INSTALL_DIR) { $env:BBS_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\bbs' }
$Version = $env:BBS_VERSION
if (-not $Version) {
  $Version = (Invoke-RestMethod "https://api.github.com/repos/$Repository/releases/latest").tag_name.TrimStart('v')
}
$Arch = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
  'X64' { 'x86_64' }
  'Arm64' { 'aarch64' }
  default { throw "Unsupported architecture: $_" }
}
$Asset = "bbs-$Arch-pc-windows-msvc.zip"
$Base = "https://github.com/$Repository/releases/download/v$Version"
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("bbs-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $TempDir | Out-Null
try {
  Invoke-WebRequest "$Base/$Asset" -OutFile (Join-Path $TempDir $Asset)
  Invoke-WebRequest "$Base/checksums.txt" -OutFile (Join-Path $TempDir 'checksums.txt')
  $Expected = ((Get-Content (Join-Path $TempDir 'checksums.txt')) | Where-Object { $_ -match [regex]::Escape($Asset) } | Select-Object -First 1).Split()[0]
  $Actual = (Get-FileHash (Join-Path $TempDir $Asset) -Algorithm SHA256).Hash.ToLowerInvariant()
  if ($Actual -ne $Expected.ToLowerInvariant()) { throw 'Checksum verification failed' }
  Expand-Archive (Join-Path $TempDir $Asset) -DestinationPath $TempDir -Force
  New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
  Copy-Item (Join-Path $TempDir 'bbs.exe') (Join-Path $InstallDir 'bbs.exe') -Force
  Write-Host "Installed bbs $Version to $InstallDir\bbs.exe"
  if (($env:PATH -split ';') -notcontains $InstallDir) { Write-Host "Add $InstallDir to your user PATH." }
} finally {
  Remove-Item -Recurse -Force $TempDir -ErrorAction SilentlyContinue
}

