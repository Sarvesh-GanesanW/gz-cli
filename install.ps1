<#
.SYNOPSIS
  Installs gz (Groundzero CLI) on Windows.
.EXAMPLE
  # with gh authenticated (repo is private):
  irm https://raw.githubusercontent.com/Sarvesh-GanesanW/gz-cli/main/install.ps1 | iex
  # ...otherwise set $env:GITHUB_TOKEN first, or pass -Token.
#>
param(
  [string]$Version = "latest",
  [string]$InstallDir = (Join-Path $env:USERPROFILE ".local\bin"),
  [string]$Token = $env:GITHUB_TOKEN
)

$ErrorActionPreference = "Stop"
$Repo = "Sarvesh-GanesanW/gz-cli"

$arch = if ([Environment]::Is64BitOperatingSystem) { "x86_64" } else { throw "gz: 32-bit Windows is not supported" }
$Asset = "gz-windows-$arch.exe"
$Base = if ($Version -eq "latest") {
  "https://github.com/$Repo/releases/latest/download"
} else {
  "https://github.com/$Repo/releases/download/$Version"
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  $out = Join-Path $tmp "gz.exe"
  $useGh = $false
  if (Get-Command gh -ErrorAction SilentlyContinue) {
    & gh auth status 2>$null >$null
    if ($LASTEXITCODE -eq 0) { $useGh = $true }
  }

  function Get-Asset($Name, $OutFile) {
    if ($useGh) {
      if ($Version -eq "latest") { gh release download -R $Repo -p $Name -O $OutFile --clobber }
      else { gh release download $Version -R $Repo -p $Name -O $OutFile --clobber }
      return
    }
    if ($Token) {
      # Private repos reject tokens on github.com download URLs; use the API.
      $rel = if ($Version -eq "latest") { "latest" } else { "tags/$Version" }
      $meta = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/$rel" `
        -Headers @{Authorization = "Bearer $Token"; Accept = "application/vnd.github+json" }
      $asset = $meta.assets | Where-Object { $_.name -eq $Name } | Select-Object -First 1
      if (-not $asset) { throw "gz: asset $Name not found in release $Version" }
      Invoke-WebRequest -Uri $asset.url -OutFile $OutFile `
        -Headers @{Authorization = "Bearer $Token"; Accept = "application/octet-stream" } -UseBasicParsing
      return
    }
    Invoke-WebRequest -Uri "$Base/$Name" -OutFile $OutFile -UseBasicParsing
  }

  try {
    Get-Asset $Asset $out
  } catch {
    throw "gz: download failed. The repo is private: set `$env:GITHUB_TOKEN or run 'gh auth login' first. ($($_.Exception.Message))"
  }

  try {
    $sumOut = Join-Path $tmp "gz.sha256"
    Get-Asset "$Asset.sha256" $sumOut
    $expected = ((Get-Content $sumOut -Raw) -split '\s+')[0].ToLower()
    $actual = (Get-FileHash $out -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { throw "gz: checksum mismatch, aborting" }
    Write-Host "gz: checksum ok"
  } catch {
    if ($_.Exception.Message -like "gz: checksum mismatch*") { throw }
    Write-Host "gz: checksum unavailable, skipping verification"
  }

  New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
  Move-Item $out (Join-Path $InstallDir "gz.exe") -Force

  $path = [Environment]::GetEnvironmentVariable("Path", "User")
  if ($path -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$path;$InstallDir", "User")
    Write-Host "gz: added $InstallDir to your user PATH (reopen the terminal to use it)"
  }
  Write-Host "gz: installed to $(Join-Path $InstallDir 'gz.exe')"
  & (Join-Path $InstallDir "gz.exe") --version
} finally {
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
