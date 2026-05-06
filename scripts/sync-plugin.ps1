#!/usr/bin/env pwsh
# Builds yeet-plugin with Rojo and copies the resulting .rbxm into the local
# Roblox Studio plugins folder, so Studio can load it on next reload. This is
# the dev loop *until* Yeet can sync itself (which is the whole point of the
# project). After running, reload plugins in Studio: Plugins -> Manage Plugins
# -> Reload, or close and reopen the place.

$ErrorActionPreference = "Stop"

$RepoRoot    = Split-Path -Parent $PSScriptRoot
$PluginDir   = Join-Path $RepoRoot "yeet-plugin"
$BuildDir    = Join-Path $PluginDir "build"
$OutputFile  = Join-Path $BuildDir "Yeet.rbxm"
$PluginsDir  = Join-Path $env:LOCALAPPDATA "Roblox\Plugins"
$Destination = Join-Path $PluginsDir "Yeet.rbxm"

if (-not (Test-Path $PluginsDir)) {
    throw "Roblox plugins folder not found at '$PluginsDir'. Confirm Studio is installed for this user; you can open Plugins -> Plugin Folder in Studio to locate it."
}

if (-not (Test-Path $BuildDir)) {
    New-Item -ItemType Directory -Path $BuildDir | Out-Null
}

Push-Location $PluginDir
try {
    & rojo build --output $OutputFile
    if ($LASTEXITCODE -ne 0) {
        throw "rojo build failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}

Copy-Item -Path $OutputFile -Destination $Destination -Force

Write-Host "Wrote $Destination"
Write-Host "Reload plugins in Studio: Plugins -> Manage Plugins -> Reload (or reopen the place)."
