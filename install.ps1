<#
.SYNOPSIS
  Installs, or removes, gz (the Groundzero CLI) on Windows.
.EXAMPLE
  irm https://raw.githubusercontent.com/Sarvesh-GanesanW/gz-cli/main/install.ps1 | iex
.EXAMPLE
  # One version, or remove gz again (add -Purge to delete its settings and sign-in too):
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/Sarvesh-GanesanW/gz-cli/main/install.ps1))) -Version v0.8.1
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/Sarvesh-GanesanW/gz-cli/main/install.ps1))) -Uninstall
#>
param(
  [string]$Version = "latest",
  [string]$InstallDir = (Join-Path $env:USERPROFILE ".local\bin"),
  [switch]$Uninstall,
  [switch]$Purge
)

$ErrorActionPreference = "Stop"
# Windows PowerShell 5.1 may still default to TLS 1.0, which GitHub refuses.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Repo = "Sarvesh-GanesanW/gz-cli"
$ScriptUrl = "https://raw.githubusercontent.com/$Repo/main/install.ps1"
$DefaultInstallDir = Join-Path $env:USERPROFILE ".local\bin"
$Exe = Join-Path $InstallDir "gz.exe"
# The folder gz keeps its settings in (see config_path in src/config.rs).
$ConfigDir = if ($env:XDG_CONFIG_HOME) { Join-Path $env:XDG_CONFIG_HOME "gz" } else { Join-Path $env:APPDATA "Groundzero\gz" }

function Write-Step($Number, $Text) { Write-Host ""; Write-Host "[$Number] $Text" }
function Write-Detail($Text) { Write-Host "      $Text" }

# The command that runs this script again with other options, for the hints
# printed at the end. It carries a custom install folder along.
function Get-RerunCommand($Options) {
  $call = "& ([scriptblock]::Create((irm $ScriptUrl)))"
  if ($InstallDir -ne $DefaultInstallDir) { $call += " -InstallDir `"$InstallDir`"" }
  "$call $Options"
}

function Get-InstalledVersion($Path) {
  # All output is read before taking the first line: stopping the pipeline
  # early (Select-Object -First) leaves $LASTEXITCODE at 1 on PowerShell 5.1.
  try { $lines = @(& $Path --version 2>$null) } catch { return "" }
  if ($lines.Count -gt 0) { $lines[0] } else { "" }
}

function Get-UserPathEntries {
  $path = [Environment]::GetEnvironmentVariable("Path", "User")
  if (-not $path) { return @() }
  @($path -split ';' | Where-Object { $_ })
}

function Test-SameFolder($A, $B) { $A.TrimEnd('\') -ieq $B.TrimEnd('\') }

function Test-OnUserPath($Dir) {
  @(Get-UserPathEntries | Where-Object { Test-SameFolder $_ $Dir }).Count -gt 0
}

# Windows keeps a program that just ran locked for a moment (antivirus scans,
# the process winding down), so replacing or deleting gz.exe right after
# `gz --version` can fail once or twice before it succeeds.
function Invoke-WithRetry([scriptblock]$Action) {
  for ($attempt = 1; ; $attempt++) {
    try { & $Action; return }
    catch {
      if ($attempt -ge 10) { throw }
      Start-Sleep -Milliseconds 500
    }
  }
}

function Get-Download($Url, $OutFile, [switch]$Quiet) {
  # curl.exe ships with Windows 10 and later and draws a fast progress bar;
  # Invoke-WebRequest's own bar slows Windows PowerShell 5.1 downloads a lot.
  $curl = Get-Command curl.exe -ErrorAction SilentlyContinue
  if ($curl) {
    $progress = if ($Quiet -or [Console]::IsErrorRedirected) { "-sS" } else { "--progress-bar" }
    & $curl.Source --proto "=https" --tlsv1.2 -fL $progress -o $OutFile $Url
    if ($LASTEXITCODE -ne 0) { throw "curl.exe exited with $LASTEXITCODE" }
    return
  }
  if ($Quiet) { $ProgressPreference = "SilentlyContinue" } else { $ProgressPreference = "Continue" }
  Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing
}

function Invoke-Uninstall {
  Write-Host "Removing gz"
  $removed = $false
  if (Test-Path $Exe) {
    Write-Detail "program   $Exe ($(Get-InstalledVersion $Exe))"
    try { Invoke-WithRetry { Remove-Item $Exe -Force } }
    catch { throw "gz: cannot remove $Exe; close every running gz and try again ($($_.Exception.Message))" }
    $removed = $true
  } else {
    Write-Detail "program   not found in $InstallDir"
    $other = Get-Command gz -ErrorAction SilentlyContinue
    if ($other) {
      Write-Detail "          the gz on your PATH is $($other.Source); to remove that one, run again"
      Write-Detail "          with -InstallDir `"$(Split-Path $other.Source)`""
    }
  }
  # A copy `gz update` keeps only while it swaps in a new version.
  Remove-Item (Join-Path $InstallDir "gz.old") -Force -ErrorAction SilentlyContinue

  # The installer put the folder on the user PATH. Take it off only once
  # nothing else lives there, since other tools share ~/.local/bin.
  $leftEmpty = (Test-Path $InstallDir) -and -not (Get-ChildItem $InstallDir -Force | Select-Object -First 1)
  if ($leftEmpty) {
    Remove-Item $InstallDir -Force
    if (Test-OnUserPath $InstallDir) {
      $kept = @(Get-UserPathEntries | Where-Object { -not (Test-SameFolder $_ $InstallDir) })
      [Environment]::SetEnvironmentVariable("Path", ($kept -join ';'), "User")
      Write-Detail "PATH      took $InstallDir off your user PATH"
    }
  }

  if ($Purge) {
    if ((Split-Path $ConfigDir -Leaf) -ne "gz") { throw "gz: refusing to delete '$ConfigDir': it is not a gz settings folder" }
    if (Test-Path $ConfigDir) {
      Write-Detail "settings  $ConfigDir (deleted: sign-in, profiles, gz code history)"
      Remove-Item $ConfigDir -Recurse -Force
      $removed = $true
      $parent = Split-Path $ConfigDir
      if ((Split-Path $parent -Leaf) -eq "Groundzero" -and -not (Get-ChildItem $parent -Force | Select-Object -First 1)) {
        Remove-Item $parent -Force
      }
    }
  } elseif (Test-Path $ConfigDir) {
    Write-Detail "settings  $ConfigDir (kept: sign-in, profiles, gz code history)"
    Write-Detail "          delete them too with: $(Get-RerunCommand '-Uninstall -Purge')"
  }

  Write-Host ""
  if ($removed) { Write-Host "gz is removed." } else { Write-Host "Nothing to remove." }
  Write-Host "If you connected AI tools with 'gz mcp', remove the groundzero server from them as well"
  Write-Host "(for example: claude mcp remove groundzero)."
}

function Invoke-Install {
  if (-not [Environment]::Is64BitOperatingSystem) { throw "gz: 32-bit Windows is not supported" }
  $asset = "gz-windows-x86_64.exe"
  Write-Host "Installing gz for Windows (x86_64)"

  Write-Step "1/4" "Finding the release"
  $tag = $Version
  if ($tag -eq "latest") {
    try { $tag = (Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -UseBasicParsing).tag_name }
    catch { throw "gz: cannot reach GitHub to find the latest release; check your connection and try again ($($_.Exception.Message))" }
    Write-Detail "latest is $tag"
  } else {
    if ($tag -notlike "v*") { $tag = "v$tag" }
    Write-Detail "$tag, as asked"
  }
  $base = "https://github.com/$Repo/releases/download/$tag"

  $tmp = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
  New-Item -ItemType Directory -Path $tmp | Out-Null
  try {
    Write-Step "2/4" "Downloading $asset"
    Write-Detail "from $base/$asset"
    $out = Join-Path $tmp $asset
    try { Get-Download "$base/$asset" $out }
    catch { throw "gz: download failed: $base/$asset (is $tag a published release? see https://github.com/$Repo/releases) ($($_.Exception.Message))" }
    Write-Detail ("{0:N1} MB downloaded" -f ((Get-Item $out).Length / 1MB))

    Write-Step "3/4" "Verifying the download"
    $sumFile = Join-Path $tmp "$asset.sha256"
    $haveSum = $true
    try { Get-Download "$base/$asset.sha256" $sumFile -Quiet } catch { $haveSum = $false }
    if (-not $haveSum) {
      Write-Detail "skipped: $tag has no checksum file"
    } else {
      $expected = ((Get-Content $sumFile -Raw).Trim() -split '\s+')[0].ToLower()
      $actual = (Get-FileHash $out -Algorithm SHA256).Hash.ToLower()
      if ($expected -ne $actual) { throw "gz: checksum mismatch for $asset (expected $expected, got $actual); nothing was installed" }
      Write-Detail "sha256 matches the release ($($actual.Substring(0, 12))...)"
    }

    Write-Step "4/4" "Installing"
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    if (Test-Path $Exe) { Write-Detail "replacing $(Get-InstalledVersion $Exe) at $Exe" }
    try { Invoke-WithRetry { Move-Item $out $Exe -Force } }
    catch { throw "gz: cannot replace $Exe; close every running gz and try again ($($_.Exception.Message))" }
    $now = Get-InstalledVersion $Exe
    if (-not $now) { throw "gz: $Exe was written but does not run; try again, or download it by hand from https://github.com/$Repo/releases" }
    Write-Detail "placed at $Exe"
  } finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
  }

  $addedToPath = $false
  if (-not (Test-OnUserPath $InstallDir)) {
    $entries = @(Get-UserPathEntries) + $InstallDir
    [Environment]::SetEnvironmentVariable("Path", ($entries -join ';'), "User")
    $addedToPath = $true
  }

  Write-Host ""
  Write-Host "$now is installed."
  Write-Host "  program   $Exe"
  Write-Host "  settings  $ConfigDir (made when you first sign in)"
  if ($addedToPath) { Write-Host "  PATH      added $InstallDir to your user PATH; open a new terminal to use 'gz'" }
  Write-Host ""
  Write-Host "Next:    gz configure, then gz auth login"
  Write-Host "Update:  gz update"
  Write-Host "Remove:  $(Get-RerunCommand '-Uninstall')"
  Write-Host "         (add -Purge to delete the settings and sign-in too)"
}

if ($Purge -and -not $Uninstall) { throw "gz: -Purge only goes with -Uninstall" }
if ($Uninstall) { Invoke-Uninstall } else { Invoke-Install }
