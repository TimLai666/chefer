#requires -version 5.1
[CmdletBinding()]
param(
    [string]$OutputTarget = "",
    [ValidateSet("", "linux/amd64", "linux/arm64")]
    [string]$ImagePlatform = "",
    [switch]$Keep,
    [switch]$CleanExisting,
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

function Set-Utf8NoBomContent {
    param(
        [string]$Path,
        [string]$Value
    )
    $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($Path, $Value, $utf8NoBom)
}

function Get-WslDistroNames {
    $out = & wsl.exe -l -q 2>$null
    if ($LASTEXITCODE -ne 0) {
        return @()
    }

    $names = @()
    foreach ($line in $out) {
        $name = ($line -replace "`0", "").Trim()
        if ($name) {
            $names += $name
        }
    }
    return $names
}

function Test-WslDistroExists {
    param([string]$Name)
    return [bool](Get-WslDistroNames | Where-Object { $_ -eq $Name } | Select-Object -First 1)
}

function Remove-WslDistro {
    param([string]$Name)
    Note "Removing WSL distro $Name"
    & wsl.exe --terminate $Name *> $null
    $out = & wsl.exe --unregister $Name 2>&1
    if ($LASTEXITCODE -ne 0) {
        $clean = (($out -join "`n") -replace "`0", "").Trim()
        Fail "failed to unregister WSL distro ${Name}: $clean"
    }
}

function Invoke-App {
    param(
        [string]$App,
        [string]$Log,
        [bool]$KeepDistro
    )

    $oldKeep = $env:CHEFER_KEEP_WSL_DISTRO
    $process = $null
    try {
        if ($KeepDistro) {
            $env:CHEFER_KEEP_WSL_DISTRO = "1"
        } else {
            Remove-Item Env:\CHEFER_KEEP_WSL_DISTRO -ErrorAction SilentlyContinue
        }

        Note "Running $App (CHEFER_KEEP_WSL_DISTRO=$($env:CHEFER_KEEP_WSL_DISTRO))"
        $psi = [System.Diagnostics.ProcessStartInfo]::new()
        $psi.FileName = $App
        $psi.WorkingDirectory = Split-Path -Parent $App
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $process = [System.Diagnostics.Process]::Start($psi)
        if (-not $process.WaitForExit(120000)) {
            $process.Kill()
            $process.WaitForExit(5000) | Out-Null
            Fail "app did not exit within 120 seconds"
        }

        $stdout = $process.StandardOutput.ReadToEnd()
        $stderr = $process.StandardError.ReadToEnd()
        @(
            "----- stdout -----"
            $stdout
            "----- stderr -----"
            $stderr
        ) | Set-Content -LiteralPath $Log -Encoding UTF8

        if ($process.ExitCode -ne 0) {
            if (Test-Path -LiteralPath $Log) {
                Write-Host "----- $Log -----"
                Get-Content -LiteralPath $Log | Write-Host
            }
            Fail "app exited with code $($process.ExitCode)"
        }
    } finally {
        if ($process -and -not $process.HasExited) {
            $process.Kill()
            $process.WaitForExit(5000) | Out-Null
        }
        if ($null -eq $oldKeep) {
            Remove-Item Env:\CHEFER_KEEP_WSL_DISTRO -ErrorAction SilentlyContinue
        } else {
            $env:CHEFER_KEEP_WSL_DISTRO = $oldKeep
        }
    }
}

if ($env:OS -ne "Windows_NT") {
    Fail "Windows WSL cleanup E2E must run on Windows"
}

Require-Command cargo
Require-Command docker
Require-Command rustc
Require-Command rustup
Require-Command wsl.exe

& wsl.exe --status *> $null
if ($LASTEXITCODE -ne 0) {
    Fail "wsl.exe --status failed; enable WSL2 before running this E2E"
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
$work = Join-Path $tempRoot ("chefer-wsl-cleanup-e2e." + [guid]::NewGuid().ToString("N"))
$image = "chefer-e2e-wsl-cleanup:$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())-$PID"
New-Item -ItemType Directory -Force -Path $work | Out-Null

try {
    $imagesDir = Join-Path $work "images"
    $appImageDir = Join-Path $work "image"
    $kit = Join-Path $work "kit"
    New-Item -ItemType Directory -Force -Path $imagesDir, $appImageDir, $kit | Out-Null

    $dockerfile = Join-Path $appImageDir "Dockerfile"
    Set-Utf8NoBomContent -Path $dockerfile -Value @"
FROM alpine:3.20
CMD ["sh", "-c", "echo CheferWslCleanupE2E; mkdir -p /data && echo ok > /data/result.txt"]
"@

    $imageTar = Join-Path $imagesDir "app.tar"
    Note "Building real Docker image for $ImagePlatform"
    Invoke-Checked docker @("build", "--platform", $ImagePlatform, "-t", $image, $appImageDir)
    Invoke-Checked docker @("save", $image, "-o", $imageTar)

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

    $agentHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $agentSrc).Hash.ToLowerInvariant().Substring(0, 8)
    $distro = "chefer-rt-$agentHash"
    Note "Expected runtime distro: $distro"

    if (Test-WslDistroExists $distro) {
        if ($CleanExisting) {
            Remove-WslDistro $distro
        } else {
            Fail "pre-existing WSL distro $distro would make cleanup evidence ambiguous; unregister it first or rerun with -CleanExisting"
        }
    }

    $appName = "WindowsWslCleanupE2E"
    $appDir = Join-Path $work "app"
    $dataDir = Join-Path $work "data"
    $outDir = Join-Path $work "out"
    New-Item -ItemType Directory -Force -Path $appDir, $dataDir, $outDir | Out-Null
    $appcipe = Join-Path $appDir "appcipe.yml"
    $dataYaml = ConvertTo-YamlSingleQuoted $dataDir
    $tarYaml = ConvertTo-YamlSingleQuoted $imageTar
    Set-Utf8NoBomContent -Path $appcipe -Value @"
version: "0.1"
name: $appName
app_version: "e2e"
data_dir: $dataYaml
crash: fail_fast
services:
  app:
    image:
      source: tar
      file: $tarYaml
      platform: $ImagePlatform
    cmd: ["sh", "-c", "echo CheferWslCleanupE2E; mkdir -p /data && echo ok > /data/result.txt"]
    persist_path: /data
"@

    $cli = Join-Path $root "target\debug\chefer-cli.exe"
    if (-not (Test-Path -LiteralPath $cli)) {
        Fail "missing built CLI: $cli"
    }
    Note "Building Windows WSL cleanup single-file app"
    Invoke-Checked $cli @("build", $appcipe, "--out", $outDir, "--kit-dir", $kit, "--target", $hostTarget)

    $app = Join-Path (Join-Path $outDir $appName) "${appName}_${hostTarget}.exe"
    if (-not (Test-Path -LiteralPath $app)) {
        Fail "missing built app: $app"
    }

    $keepLog = Join-Path $work "keep.log"
    Invoke-App -App $app -Log $keepLog -KeepDistro $true
    if (-not (Test-WslDistroExists $distro)) {
        if (Test-Path -LiteralPath $keepLog) {
            Write-Host "----- $keepLog -----"
            Get-Content -LiteralPath $keepLog | Write-Host
        }
        Fail "expected $distro to remain when CHEFER_KEEP_WSL_DISTRO=1"
    }
    Note "Verified CHEFER_KEEP_WSL_DISTRO=1 keeps $distro"
    Remove-WslDistro $distro

    $cleanupLog = Join-Path $work "cleanup.log"
    Invoke-App -App $app -Log $cleanupLog -KeepDistro $false
    if (Test-WslDistroExists $distro) {
        if (Test-Path -LiteralPath $cleanupLog) {
            Write-Host "----- $cleanupLog -----"
            Get-Content -LiteralPath $cleanupLog | Write-Host
        }
        Fail "expected runtime to remove $distro after app exit"
    }
    Note "Verified runtime removes $distro after app exit"

    $resultFile = Join-Path $dataDir "data\app\result.txt"
    if (-not (Test-Path -LiteralPath $resultFile)) {
        Fail "expected persisted result file: $resultFile"
    }

    Note "Windows WSL cleanup E2E passed: keep mode retains distro; normal mode unregisters it after exit"
} finally {
    docker image rm $image *> $null
    if ($Keep) {
        Write-Host "keeping E2E work dir: $work"
    } elseif ((Test-Path -LiteralPath $work) -and ((Split-Path -Leaf $work) -like "chefer-wsl-cleanup-e2e.*")) {
        Remove-Item -LiteralPath $work -Recurse -Force
    }
}
