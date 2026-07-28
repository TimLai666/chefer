#requires -version 5.1
<#
.SYNOPSIS
One-shot validation of the chefer `whp` backend on a real Windows machine.

.DESCRIPTION
Runs every whp check that is gated on real hardware: VM boot, exit-code
propagation, guest userspace stdout actually reaching the host, TCP port
forwarding, and helper anti-orphan behavior.

Why this script exists (do not delete): GitHub's windows runners cannot do
nested virtualization, so CI can only compile-check whp-helper. The appliance
QEMU E2E uses virtio-console (hvc0) and therefore never exercises the WHP 8250
serial path. That path once shipped without a THRE interrupt, which silently
dropped every byte of guest userspace stdout and looked exactly like "the
services never started" (see DESIGN section 6, whp, item 4). Check 2 below is
the regression guard for that fix.

.PARAMETER KitDir
Kit directory holding the appliance (chefer-vmlinuz-<arch>,
chefer-initramfs-<arch>) and guest-agent-<arch>. Defaults to
$env:CHEFER_KIT_DIR, then <repo>/kit.

.PARAMETER Keep
Keep the work directory (dist/whp-smoke) for inspection.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts/whp-smoke.ps1
#>
[CmdletBinding()]
param(
    [string]$KitDir = "",
    [switch]$Keep
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Fail {
    param([string]$Message)
    Write-Error "whp-smoke: $Message"
    exit 1
}

function Note {
    param([string]$Message)
    Write-Host "==> $Message"
}

if ($env:OS -ne "Windows_NT") { Fail "this script only runs on Windows (whp = Windows Hypervisor Platform)" }
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Fail "cargo not found; install Rust (https://rustup.rs)" }
if (-not (Test-Path "$env:SystemRoot\System32\WinHvPlatform.dll")) {
    Fail "WinHvPlatform.dll not found; enable 'Windows Hypervisor Platform' in Windows Features and reboot"
}

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { $arch = "x86_64" }
    "ARM64" { $arch = "aarch64" }
    default { Fail "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}
$triple = "$arch-pc-windows-msvc"
$platform = if ($arch -eq "aarch64") { "linux/arm64" } else { "linux/amd64" }

if (-not $KitDir) {
    $KitDir = if ($env:CHEFER_KIT_DIR) { $env:CHEFER_KIT_DIR } else { Join-Path $root "kit" }
}
foreach ($f in @("chefer-vmlinuz-$arch", "chefer-initramfs-$arch", "guest-agent-$arch")) {
    if (-not (Test-Path (Join-Path $KitDir $f))) {
        Fail "kit is missing $f (looked in $KitDir); use a release kit/ or run scripts/build-appliance.sh"
    }
}

$work = Join-Path $root "dist\whp-smoke"
if (Test-Path $work) { Remove-Item $work -Recurse -Force }
New-Item -ItemType Directory -Path $work | Out-Null

Note "1/4 building chefer-cli + chefer-runtime + chefer-whp-helper (release)"
& cargo build --release -p chefer-cli -p chefer-runtime -p whp-helper --manifest-path (Join-Path $root "Cargo.toml")
if ($LASTEXITCODE -ne 0) { Fail "cargo build failed" }

# Smoke kit: appliance / guest-agent / pasta come from the supplied kit, but the
# runtime and helper are always the ones just built from source -- never validate
# a fresh fix against a stale binary.
$smokeKit = Join-Path $work "kit"
New-Item -ItemType Directory -Path $smokeKit | Out-Null
foreach ($f in @("chefer-vmlinuz-$arch", "chefer-initramfs-$arch", "guest-agent-$arch", "pasta-$arch")) {
    $src = Join-Path $KitDir $f
    if (Test-Path $src) { Copy-Item $src $smokeKit }
}
Copy-Item (Join-Path $root "target\release\chefer-runtime.exe") (Join-Path $smokeKit "chefer-runtime-$triple.exe")
Copy-Item (Join-Path $root "target\release\chefer-whp-helper.exe") (Join-Path $smokeKit "chefer-whp-helper-$arch.exe")
$cli = Join-Path $root "target\release\chefer-cli.exe"

function Build-App {
    param([string]$Name, [string]$Yaml)
    $ymlPath = Join-Path $work "$Name.yml"
    # Windows PowerShell 5.1's `-Encoding utf8` writes a BOM, which the YAML parser
    # rejects ("missing field `name` at line 1 column 2"). Write UTF-8 without one.
    [System.IO.File]::WriteAllText($ymlPath, $Yaml, (New-Object System.Text.UTF8Encoding($false)))
    & $cli build $ymlPath --out (Join-Path $work "dist") --kit-dir $smokeKit | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "chefer build failed for $Name" }
    $exe = Join-Path $work "dist\$Name\${Name}_$triple.exe"
    if (-not (Test-Path $exe)) { Fail "build did not produce $exe" }
    return $exe
}

$env:CHEFER_BACKEND = "whp"
# Deliberately tiny: --timeout is a *boot* watchdog that disarms once the guest
# reaches userspace. If it ever regresses back into a whole-run cap, the dwell in
# check 2 below will outlive it and the script fails.
$env:CHEFER_WHP_TIMEOUT = "20"

Note "2/4 check 1: exit-code propagation (fail_fast non-zero -> single file exits with the same code)"
$exitExe = Build-App -Name "whp-exit" -Yaml @"
version: "0.1"
name: whp-exit
network: internal
services:
  one:
    image:
      source: image
      file: alpine:3.20
      platform: $platform
    interface_mode: none
    cmd: ["sh", "-c", "echo WHP_SMOKE_ONESHOT; exit 7"]
"@
$exitLog = Join-Path $work "exit.log"
$oneShot = Start-Process -FilePath $exitExe -ArgumentList "--extract-dir", (Join-Path $work "extract-exit") `
    -RedirectStandardOutput $exitLog -RedirectStandardError (Join-Path $work "exit.err.log") `
    -NoNewWindow -PassThru -Wait
if (-not (Select-String -Path $exitLog -Pattern "CHEFER_GUEST_EXIT=" -Quiet)) {
    Get-Content $exitLog -Tail 40 | Write-Host
    Fail "no CHEFER_GUEST_EXIT marker on the console; log: $exitLog"
}
if ($oneShot.ExitCode -ne 7) {
    Fail "exit-code propagation failed: expected 7, got $($oneShot.ExitCode); log: $exitLog"
}
Note "check 1 passed: VM boots, CHEFER_GUEST_EXIT marker present, exit code 7 propagated"

Note "3/4 check 2: guest userspace stdout reaches the host (8250 THRE regression guard) + service stays up + TCP forward"
$smokeExe = Build-App -Name "whp-smoke" -Yaml @"
version: "0.1"
name: whp-smoke
services:
  web:
    image:
      source: image
      file: alpine:3.20
      platform: $platform
    interface_mode: none
    ports: ["18080:8080"]
    # The alpine base has no httpd (it lives in busybox-extras); busybox nc is
    # enough for a minimal HTTP responder.
    cmd: ["sh", "-c", "echo WHP_SMOKE_SERVICE_UP; while true; do printf 'HTTP/1.0 200 OK\r\n\r\nok\n' | nc -l -p 8080; done"]
"@
$runLog = Join-Path $work "run.log"
$app = Start-Process -FilePath $smokeExe -ArgumentList "--extract-dir", (Join-Path $work "extract") `
    -RedirectStandardOutput $runLog -RedirectStandardError (Join-Path $work "run.err.log") `
    -NoNewWindow -PassThru

try {
    # guest-agent prefixes service stdout with the service name. Seeing "[web] ..."
    # on the host proves guest userspace output crossed the 8250 tty path -- kernel
    # printk uses a polled write and would show up even with the interrupt missing.
    $sawStdout = $false
    for ($i = 0; $i -lt 180; $i++) {
        if ((Test-Path $runLog) -and (Select-String -Path $runLog -Pattern "\[web\] WHP_SMOKE_SERVICE_UP" -Quiet)) {
            $sawStdout = $true
            break
        }
        if ($app.HasExited) { break }
        Start-Sleep -Seconds 1
    }
    if (-not $sawStdout) {
        Get-Content $runLog -Tail 40 | Write-Host
        Fail "no guest service stdout within 180s. A console that stops after the kernel/init messages usually means the 8250 THRE interrupt regressed (DESIGN section 6, whp, item 4); console tail above"
    }

    # Grab the helper PID while the VM is demonstrably up; check 3 needs it later and
    # a lookup done only at kill time has proven flaky.
    $helperId = $null
    for ($i = 0; $i -lt 20; $i++) {
        $helper = Get-Process -ErrorAction SilentlyContinue |
            Where-Object { $_.ProcessName -like "chefer-whp-helper*" } | Select-Object -First 1
        if ($helper) { $helperId = [int]$helper.Id; break }
        Start-Sleep -Milliseconds 500
    }
    if (-not $helperId) { Fail "no chefer-whp-helper process while the guest is running; cannot validate anti-orphan" }

    # Raw TCP rather than Invoke-WebRequest: busybox nc serves one connection and
    # waits for the peer to hang up, while Invoke-WebRequest asks for keep-alive and
    # waits for the server -- they deadlock until the timeout, once per attempt.
    $sawPort = $false
    for ($i = 0; $i -lt 60; $i++) {
        try {
            $client = New-Object System.Net.Sockets.TcpClient
            $client.Connect("127.0.0.1", 18080)
            $stream = $client.GetStream()
            $stream.ReadTimeout = 3000
            $req = [System.Text.Encoding]::ASCII.GetBytes("GET / HTTP/1.0`r`n`r`n")
            $stream.Write($req, 0, $req.Length)
            $buf = New-Object byte[] 256
            $read = $stream.Read($buf, 0, $buf.Length)
            $client.Close()
            if ($read -gt 0 -and [System.Text.Encoding]::ASCII.GetString($buf, 0, $read) -match "200 OK") {
                $sawPort = $true
                break
            }
        } catch {
            if ($client) { $client.Close() }
        }
        Start-Sleep -Seconds 1
    }
    if (-not $sawPort) { Fail "TCP port forwarding failed (127.0.0.1:18080 unreachable); log: $runLog" }

    # Outlive the boot watchdog on purpose: CHEFER_WHP_TIMEOUT is 20s above, so a
    # helper that still treats --timeout as a whole-run cap kills the VM here.
    $dwell = 40
    Note "dwelling ${dwell}s to prove the boot watchdog disarmed (CHEFER_WHP_TIMEOUT=$env:CHEFER_WHP_TIMEOUT)"
    Start-Sleep -Seconds $dwell
    if ($app.HasExited) {
        Get-Content $runLog -Tail 20 | Write-Host
        Fail "the app died while idling past the boot watchdog; --timeout is capping the whole run again, not just boot"
    }
    Note "check 2 passed: guest userspace stdout, service stays up past the watchdog, TCP forward"

    Note "4/4 check 3: helper anti-orphan (hard-kill the runtime; only the Job Object can save us)"
    if (-not (Get-Process -Id $helperId -ErrorAction SilentlyContinue)) {
        Fail "the helper (pid $helperId) disappeared before the anti-orphan check; the VM died early"
    }
    # taskkill /F is TerminateProcess: no handler runs and the stdin EOF has not
    # happened yet, so survival depends purely on KILL_ON_JOB_CLOSE.
    & taskkill /F /PID $app.Id | Out-Null
    $gone = $false
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Milliseconds 500
        if (-not (Get-Process -Id $helperId -ErrorAction SilentlyContinue)) {
            $gone = $true
            break
        }
    }
    if (-not $gone) {
        Stop-Process -Id $helperId -Force -ErrorAction SilentlyContinue
        Fail "helper was orphaned: runtime is dead but chefer-whp-helper is still alive after 10s (Job Object anti-orphan broken)"
    }
    Note "check 3 passed: no helper left behind after the runtime was hard-killed"
} finally {
    if (-not $app.HasExited) { Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue }
    Get-Process "chefer-whp-helper-$arch" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Remove-Item Env:\CHEFER_BACKEND -ErrorAction SilentlyContinue
    Remove-Item Env:\CHEFER_WHP_TIMEOUT -ErrorAction SilentlyContinue
    if (-not $Keep) { Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue }
}

Note "all checks passed."
