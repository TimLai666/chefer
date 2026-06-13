#requires -version 5.1
[CmdletBinding()]
param(
    [string]$OutputTarget = "",
    [ValidateSet("", "linux/amd64", "linux/arm64")]
    [string]$ImagePlatform = "",
    [switch]$Keep,
    [switch]$SkipRustupTargetAdd
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Fail {
    param([string]$Message)
    Write-Error $Message
    exit 1
}

function Note {
    param([string]$Message)
    Write-Host "==> $Message"
}

function Require-Command {
    param([string]$Name)
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        Fail "missing required command: $Name"
    }
}

function Invoke-Checked {
    param(
        [string]$File,
        [string[]]$Arguments
    )
    Note "$File $($Arguments -join ' ')"
    & $File @Arguments
    if ($LASTEXITCODE -ne 0) {
        Fail "command failed with exit code ${LASTEXITCODE}: $File $($Arguments -join ' ')"
    }
}

function Get-RepoRoot {
    return (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
}

function Get-RustHostTarget {
    $hostLine = (& rustc -vV | Select-String -Pattern "^host: " | Select-Object -First 1)
    if (-not $hostLine) {
        Fail "unable to read rustc host target"
    }
    return ($hostLine.ToString() -replace "^host: ", "").Trim()
}

function ConvertTo-YamlSingleQuoted {
    param([string]$Value)
    return "'" + ($Value -replace "'", "''") + "'"
}

function Get-WindowTitles {
    if (-not ("CheferWindowSearch" -as [type])) {
        Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class CheferWindowSearch {
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int count);

    [DllImport("user32.dll")]
    public static extern bool IsWindowVisible(IntPtr hWnd);

    public static string[] Titles() {
        List<string> titles = new List<string>();
        EnumWindows(delegate(IntPtr hWnd, IntPtr lParam) {
            if (IsWindowVisible(hWnd)) {
                StringBuilder text = new StringBuilder(512);
                GetWindowText(hWnd, text, text.Capacity);
                string title = text.ToString();
                if (!String.IsNullOrWhiteSpace(title)) {
                    titles.Add(title);
                }
            }
            return true;
        }, IntPtr.Zero);
        return titles.ToArray();
    }
}
"@
    }
    return [CheferWindowSearch]::Titles()
}

function Wait-WindowTitle {
    param(
        [string]$Title,
        [System.Diagnostics.Process]$Process,
        [int]$TimeoutSeconds
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $titles = Get-WindowTitles
        if ($titles | Where-Object { $_ -like "*$Title*" }) {
            return $true
        }
        if ($Process.HasExited) {
            return $false
        }
        Start-Sleep -Milliseconds 250
    }
    return $false
}

function Show-LogIfExists {
    param([string]$Path)
    if (Test-Path -LiteralPath $Path) {
        Write-Host "----- $Path -----"
        Get-Content -LiteralPath $Path | Write-Host
    }
}

if ($env:OS -ne "Windows_NT") {
    Fail "Windows WSLg E2E must run on Windows"
}

Require-Command cargo
Require-Command docker
Require-Command rustc
Require-Command rustup
Require-Command wsl.exe

& wsl.exe --status *> $null
if ($LASTEXITCODE -ne 0) {
    Fail "wsl.exe --status failed; enable WSL2 and WSLg before running this E2E"
}

$root = Get-RepoRoot
Set-Location $root

$hostTarget = if ($OutputTarget) { $OutputTarget } else { Get-RustHostTarget }
if ($hostTarget -notlike "*windows*") {
    Fail "OutputTarget must be a Windows Rust target, got: $hostTarget"
}

if ($hostTarget -like "x86_64-*") {
    $guestArch = "x86_64"
    $guestTarget = "x86_64-unknown-linux-musl"
    if (-not $ImagePlatform) { $ImagePlatform = "linux/amd64" }
} elseif ($hostTarget -like "aarch64-*") {
    $guestArch = "aarch64"
    $guestTarget = "aarch64-unknown-linux-musl"
    if (-not $ImagePlatform) { $ImagePlatform = "linux/arm64" }
} else {
    Fail "unsupported Windows target architecture: $hostTarget"
}

$tempRoot = if ($env:TEMP) { $env:TEMP } else { "C:\tmp" }
$work = Join-Path $tempRoot ("chefer-wslg-e2e." + [guid]::NewGuid().ToString("N"))
$image = "chefer-e2e-wslg:$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())-$PID"
$process = $null
New-Item -ItemType Directory -Force -Path $work | Out-Null

try {
    $imagesDir = Join-Path $work "images"
    $guiImageDir = Join-Path $work "gui-image"
    $kit = Join-Path $work "kit"
    New-Item -ItemType Directory -Force -Path $imagesDir, $guiImageDir, $kit | Out-Null

    $dockerfile = Join-Path $guiImageDir "Dockerfile"
    @"
FROM debian:bookworm-slim
RUN apt-get update \
  && apt-get install -y --no-install-recommends x11-apps x11-utils \
  && rm -rf /var/lib/apt/lists/*
CMD ["xmessage", "-timeout", "20", "-title", "CheferWslgE2E", "-name", "CheferWslgE2E", "-center", "Chefer WSLg E2E"]
"@ | Set-Content -LiteralPath $dockerfile -Encoding UTF8

    $guiTar = Join-Path $imagesDir "gui.tar"
    Note "Building real GUI Docker image for $ImagePlatform"
    Invoke-Checked docker @("build", "--platform", $ImagePlatform, "-t", $image, $guiImageDir)
    Invoke-Checked docker @("save", $image, "-o", $guiTar)

    Note "Building Chefer CLI, Windows runtime, and Linux guest-agent"
    Invoke-Checked cargo @("build", "-p", "chefer-cli")
    Invoke-Checked cargo @("build", "-p", "chefer-runtime", "--target", $hostTarget)
    if (-not $SkipRustupTargetAdd) {
        Invoke-Checked rustup @("target", "add", $guestTarget)
    }
    Invoke-Checked cargo @("build", "-p", "guest-agent", "--target", $guestTarget)

    $runtimeSrc = Join-Path $root "target\$hostTarget\debug\chefer-runtime.exe"
    $agentSrc = Join-Path $root "target\$guestTarget\debug\guest-agent"
    if (-not (Test-Path -LiteralPath $runtimeSrc)) {
        Fail "missing built runtime: $runtimeSrc"
    }
    if (-not (Test-Path -LiteralPath $agentSrc)) {
        Fail "missing built guest-agent: $agentSrc"
    }
    Copy-Item -LiteralPath $runtimeSrc -Destination (Join-Path $kit "chefer-runtime-$hostTarget.exe")
    Copy-Item -LiteralPath $agentSrc -Destination (Join-Path $kit "guest-agent-$guestArch")

    $appName = "WindowsWslgGuiE2E"
    $appDir = Join-Path $work "app"
    $dataDir = Join-Path $work "data"
    $outDir = Join-Path $work "out"
    New-Item -ItemType Directory -Force -Path $appDir, $dataDir, $outDir | Out-Null
    $appcipe = Join-Path $appDir "appcipe.yml"
    $dataYaml = ConvertTo-YamlSingleQuoted $dataDir
    $tarYaml = ConvertTo-YamlSingleQuoted $guiTar
    @"
version: "0.1"
name: $appName
app_version: "e2e"
data_dir: $dataYaml
crash: fail_fast
services:
  gui:
    image:
      source: tar
      file: $tarYaml
      format: docker-archive
      platform: $ImagePlatform
    cmd: ["xmessage", "-timeout", "20", "-title", "CheferWslgE2E", "-name", "CheferWslgE2E", "-center", "Chefer WSLg E2E"]
    interface_mode: gui
"@ | Set-Content -LiteralPath $appcipe -Encoding UTF8

    $cli = Join-Path $root "target\debug\chefer-cli.exe"
    if (-not (Test-Path -LiteralPath $cli)) {
        Fail "missing built CLI: $cli"
    }
    Note "Building Windows WSLg single-file app"
    Invoke-Checked $cli @("build", $appcipe, "--out", $outDir, "--kit-dir", $kit, "--target", $hostTarget)

    $app = Join-Path (Join-Path $outDir $appName) "${appName}_${hostTarget}.exe"
    if (-not (Test-Path -LiteralPath $app)) {
        Fail "missing built app: $app"
    }

    $stdout = Join-Path $work "app.stdout.log"
    $stderr = Join-Path $work "app.stderr.log"
    Note "Running single-file app and waiting for WSLg window title CheferWslgE2E"
    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $app
    $psi.WorkingDirectory = Split-Path -Parent $app
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $process = [System.Diagnostics.Process]::Start($psi)

    $found = Wait-WindowTitle -Title "CheferWslgE2E" -Process $process -TimeoutSeconds 45
    if (-not $found) {
        if (-not $process.HasExited) {
            $process.Kill()
            $process.WaitForExit(5000) | Out-Null
        }
        $outText = $process.StandardOutput.ReadToEnd()
        $errText = $process.StandardError.ReadToEnd()
        $outText | Set-Content -LiteralPath $stdout -Encoding UTF8
        $errText | Set-Content -LiteralPath $stderr -Encoding UTF8
        Show-LogIfExists $stdout
        Show-LogIfExists $stderr
        Fail "timed out waiting for WSLg window CheferWslgE2E"
    }

    if (-not $process.WaitForExit(45000)) {
        $process.Kill()
        $process.WaitForExit(5000) | Out-Null
        Fail "app did not exit after WSLg window was detected"
    }
    $stdoutText = $process.StandardOutput.ReadToEnd()
    $stderrText = $process.StandardError.ReadToEnd()
    $stdoutText | Set-Content -LiteralPath $stdout -Encoding UTF8
    $stderrText | Set-Content -LiteralPath $stderr -Encoding UTF8
    if ($process.ExitCode -ne 0) {
        Show-LogIfExists $stdout
        Show-LogIfExists $stderr
        Fail "expected app exit code 0, got $($process.ExitCode)"
    }

    Note "Windows WSLg E2E passed: docker save -> chefer build -> Windows single-file run -> visible WSLg window"
} finally {
    if ($process -and -not $process.HasExited) {
        $process.Kill()
        $process.WaitForExit(5000) | Out-Null
    }
    docker image rm $image *> $null
    if ($Keep) {
        Write-Host "keeping E2E work dir: $work"
    } elseif ((Test-Path -LiteralPath $work) -and ((Split-Path -Leaf $work) -like "chefer-wslg-e2e.*")) {
        Remove-Item -LiteralPath $work -Recurse -Force
    }
}
