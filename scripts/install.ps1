# Chefer one-line installer (Windows).
#
#   irm https://raw.githubusercontent.com/TimLai666/chefer/main/scripts/install.ps1 | iex
#
# Detects arch, downloads the matching platform package from GitHub Releases, verifies
# its sha256, extracts it, and adds it to the user PATH. After install run `chefer version`;
# later update with `chefer upgrade`.
#
# Variables you can set before running:
#   $env:CHEFER_VERSION      pin a version tag (default: latest release)
#   $env:CHEFER_INSTALL_DIR  install directory (default: %LOCALAPPDATA%\chefer)
#
# NOTE: end users who only *run* a chefer-packaged single-file app need nothing installed;
# this script installs the CLI + kit for developers who want to *package* apps.
#
# ASCII-only on purpose: Windows PowerShell 5.1 reads a BOM-less .ps1 as the ANSI codepage,
# which would corrupt non-ASCII text when the file is run from disk. Keep this file ASCII.
$ErrorActionPreference = 'Stop'
$repo = 'TimLai666/chefer'
$installDir = if ($env:CHEFER_INSTALL_DIR) { $env:CHEFER_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'chefer' }

# arch -> target triple
$osArch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
$cpu = if ($osArch -eq [System.Runtime.InteropServices.Architecture]::Arm64) { 'aarch64' } else { 'x86_64' }
$target = "$cpu-pc-windows-msvc"

# version: ask GitHub for the latest release unless pinned
$ver = $env:CHEFER_VERSION
if (-not $ver) {
    try {
        $rel = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest" -Headers @{ 'User-Agent' = 'chefer-install' }
        $ver = $rel.tag_name
    } catch {
        throw "Could not fetch the latest release (none published yet?). Set `$env:CHEFER_VERSION to pin a version and retry. Underlying error: $($_.Exception.Message)"
    }
}
if (-not $ver) { throw "Could not determine the latest release version" }

$asset = "chefer_${ver}_${target}.zip"
$base = "https://github.com/$repo/releases/download/$ver"
$tmp = Join-Path $env:TEMP ("chefer-install-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    Write-Host "chefer-install: downloading $asset ($ver) ..."
    Invoke-WebRequest "$base/$asset" -OutFile (Join-Path $tmp $asset)

    # sha256 verification (skip if the .sha256 sidecar is absent)
    $haveSha = $true
    try {
        Invoke-WebRequest "$base/$asset.sha256" -OutFile (Join-Path $tmp "$asset.sha256")
    } catch {
        $haveSha = $false
    }
    if ($haveSha) {
        $want = ((Get-Content (Join-Path $tmp "$asset.sha256") -Raw).Trim() -split '\s+')[0]
        $got = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash
        if ($want -and ($got -ne $want.ToUpper())) {
            throw "sha256 mismatch (corrupt download): want=$want got=$got"
        }
    }

    Expand-Archive (Join-Path $tmp $asset) -DestinationPath $tmp -Force
    $src = Join-Path $tmp "chefer_${ver}_${target}"
    if (-not (Test-Path (Join-Path $src 'chefer.exe'))) { throw "unexpected archive layout (missing chefer.exe)" }

    # install: replace only chefer.exe and kit (kit must sit next to chefer.exe)
    New-Item -ItemType Directory -Force $installDir | Out-Null
    $kitDst = Join-Path $installDir 'kit'
    if (Test-Path $kitDst) { Remove-Item -Recurse -Force $kitDst }
    Copy-Item (Join-Path $src 'kit') $kitDst -Recurse -Force
    Copy-Item (Join-Path $src 'chefer.exe') (Join-Path $installDir 'chefer.exe') -Force
    Write-Host "chefer-install: installed to $installDir"

    # PATH (User scope, idempotent)
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = if ($userPath) { $userPath -split ';' } else { @() }
    if ($parts -notcontains $installDir) {
        $newPath = if ($userPath) { "$userPath;$installDir" } else { $installDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Host "chefer-install: added $installDir to your user PATH (open a new terminal for it to take effect)."
    }
    $env:Path = "$env:Path;$installDir"
    Write-Host "chefer-install: done. Run 'chefer version' to confirm; update later with 'chefer upgrade'."
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
