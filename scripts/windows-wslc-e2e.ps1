#requires -version 5.1
<#
.SYNOPSIS
Build and run a Chefer app with WSL Containers, without Docker.
.DESCRIPTION
Requires a physical Windows host with wslc, WSL2, Rust and native MSVC tools.
Checks spaced paths/build args, export, packaging, guest stdout and persisted
output. Preserves logs on failure and restores environment variables.
.PARAMETER Keep
Preserve the temporary workspace after a successful run.
.PARAMETER SkipRustupTargetAdd
Use the already installed Linux musl Rust target.
.PARAMETER RepositoryRoot
Repository to build. Defaults to the parent of this script's directory.
.EXAMPLE
powershell -File scripts/windows-wslc-e2e.ps1
#>
[CmdletBinding()]
param(
    [switch]$Keep,
    [switch]$SkipRustupTargetAdd,
    [string]$RepositoryRoot = (Join-Path $PSScriptRoot "..")
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if ($env:OS -ne "Windows_NT") { throw "This smoke test requires Windows with WSL Containers." }
$root = (Resolve-Path -LiteralPath $RepositoryRoot).ProviderPath
$originalLocation = Get-Location
$originalPath = $env:PATH
$originalBuilder = $env:CHEFER_DOCKERFILE_BUILDER
$originalBackend = $env:CHEFER_BACKEND
$work = Join-Path $env:TEMP ("chefer-wslc-e2e." + [guid]::NewGuid().ToString("N"))
$utf8 = [System.Text.UTF8Encoding]::new($false)
$passed = $false

function Invoke-Checked {
    param([string]$File, [string[]]$Arguments)
    & $File @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$File failed with exit code $LASTEXITCODE" }
}

try {
    Set-Location $root
    if (-not (Get-Command wslc -ErrorAction SilentlyContinue)) {
        $installedWslc = Join-Path $env:ProgramFiles "WSL\wslc.exe"
        if (Test-Path -LiteralPath $installedWslc) {
            $env:PATH = (Split-Path $installedWslc) + ";" + $env:PATH
        } else { throw "wslc is unavailable. Run wsl --update before this test." }
    }
    Invoke-Checked wslc @("--version")
    $hostLine = & rustc -vV | Select-String '^host: '
    if (-not $hostLine) { throw "Cannot determine the Rust host target." }
    $hostTarget = ($hostLine.ToString() -replace '^host: ', '').Trim()
    switch ($hostTarget) {
        "x86_64-pc-windows-msvc" { $arch = "x86_64"; $platform = "linux/amd64" }
        "aarch64-pc-windows-msvc" { $arch = "aarch64"; $platform = "linux/arm64" }
        default { throw "Unsupported Windows host target: $hostTarget" }
    }
    $guestTarget = "$arch-unknown-linux-musl"
    if (-not $SkipRustupTargetAdd) { Invoke-Checked rustup @("target", "add", $guestTarget) }
    Invoke-Checked cargo @("build", "-p", "chefer-cli", "-p", "chefer-runtime")
    Invoke-Checked cargo @("build", "-p", "guest-agent", "--target", $guestTarget)
    $metadataText = & cargo metadata --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed." }
    $targetDir = ($metadataText | ConvertFrom-Json).target_directory
    $cli = Join-Path $targetDir "debug\chefer-cli.exe"
    $runtime = Join-Path $targetDir "debug\chefer-runtime.exe"
    $agent = Join-Path $targetDir "$guestTarget\debug\guest-agent"
    $context = Join-Path $work "context with spaces"
    $kit = Join-Path $work "kit"
    New-Item -ItemType Directory -Path $context, $kit | Out-Null
    Copy-Item -LiteralPath $runtime -Destination (Join-Path $kit "chefer-runtime-$hostTarget.exe")
    Copy-Item -LiteralPath $agent -Destination (Join-Path $kit "guest-agent-$arch")
    $dockerfile = @'
FROM alpine:3.20
ARG CHEFER_TEST_MARKER
RUN test "$CHEFER_TEST_MARKER" = "WSLC build args with spaces"
CMD ["sh", "-c", "echo CheferWslcE2E; mkdir -p /data; printf WSLC_OK > /data/result.txt"]
'@
    [System.IO.File]::WriteAllText((Join-Path $context "Dockerfile"), $dockerfile, $utf8)
    $dataDir = Join-Path $work "data"
    $dataYaml = "'" + ($dataDir -replace "'", "''") + "'"
    $recipe = @"
version: "0.1"
name: WindowsWslcE2E
network: internal
data_dir: $dataYaml
services:
  app:
    image:
      source: dockerfile
      file: './context with spaces/Dockerfile'
      context: './context with spaces'
      platform: $platform
      build_args:
        CHEFER_TEST_MARKER: 'WSLC build args with spaces'
    persist_path: /data
"@
    $recipePath = Join-Path $work "appcipe.yml"
    [System.IO.File]::WriteAllText($recipePath, $recipe, $utf8)
    $env:CHEFER_DOCKERFILE_BUILDER = "wslc"
    $env:CHEFER_BACKEND = "wsl2"
    Invoke-Checked $cli @("check", $recipePath)
    Invoke-Checked $cli @("build", $recipePath, "--out", (Join-Path $work "out"), "--kit-dir", $kit, "--target", $hostTarget)
    $app = Join-Path $work "out\WindowsWslcE2E\WindowsWslcE2E_$hostTarget.exe"
    Invoke-Checked $cli @("inspect", $app)
    $stdout = Join-Path $work "run.stdout.log"
    $stderr = Join-Path $work "run.stderr.log"
    $run = Start-Process -FilePath $app -ArgumentList @("--no-cache") -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    if (-not $run.WaitForExit(180000)) {
        $run.Kill()
        throw "Packaged app timed out. Logs: $stdout and $stderr"
    }
    $run.Refresh()
    if ($run.ExitCode -ne 0) {
        Get-Content -LiteralPath $stderr | Write-Host
        throw "Packaged app exited with code $($run.ExitCode). Logs: $stdout and $stderr"
    }
    if (-not ((Get-Content -LiteralPath $stdout -Raw) -match 'CheferWslcE2E')) {
        throw "Guest stdout did not reach the Windows host. Log: $stdout"
    }
    $result = Join-Path $dataDir "data\app\result.txt"
    if (-not (Test-Path -LiteralPath $result) -or (Get-Content -LiteralPath $result -Raw) -ne "WSLC_OK") {
        throw "Persisted guest output was not created correctly: $result"
    }
    $passed = $true
    Write-Host "WSLC E2E passed: Dockerfile -> image archive -> executable -> WSL runtime -> persisted output."
} finally {
    $env:PATH = $originalPath
    $env:CHEFER_DOCKERFILE_BUILDER = $originalBuilder
    $env:CHEFER_BACKEND = $originalBackend
    Set-Location $originalLocation
    if ($Keep -or -not $passed) {
        Write-Host "E2E work dir: $work"
    } elseif (Test-Path -LiteralPath $work) {
        $resolvedWork = [System.IO.Path]::GetFullPath($work)
        $resolvedTemp = [System.IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
        if (-not $resolvedWork.StartsWith($resolvedTemp, [System.StringComparison]::OrdinalIgnoreCase) -or (Split-Path $resolvedWork -Leaf) -notlike 'chefer-wslc-e2e.*') {
            throw "Refusing to clean a path outside this test's temporary workspace: $resolvedWork"
        }
        Remove-Item -LiteralPath $resolvedWork -Recurse -Force
    }
}
