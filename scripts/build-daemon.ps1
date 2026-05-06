#!/usr/bin/env pwsh
# Builds yeet-daemon in release mode. The VS Code extension spawns the binary
# from `yeet-daemon/target/release/yeet-daemon.exe`, so a plain `cargo build`
# (which writes to `target/debug/`) is NOT picked up by the extension. After
# changing daemon code, run this script and then restart the daemon process
# (close the running window or kill it from Task Manager) — the extension
# will respawn the new binary on the next IDE action.

$ErrorActionPreference = "Stop"

$RepoRoot  = Split-Path -Parent $PSScriptRoot
$DaemonDir = Join-Path $RepoRoot "yeet-daemon"

Push-Location $DaemonDir
try {
    & cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}

$Binary = Join-Path $DaemonDir "target\release\yeet-daemon.exe"
Write-Host "Built $Binary"
Write-Host "Stop the running daemon (close its window) so the extension picks up the new binary."
