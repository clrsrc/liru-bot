#requires -Version 7
<#
.SYNOPSIS
    Zwei-Stufen-PGO-Build (Profile-Guided Optimization) für liru-bot.

.DESCRIPTION
    Stage 1: baut Test-Binaries instrumentiert und führt sie aus
             (`cargo pgo test`). Profile landen in target/pgo-profiles/.
    Stage 2: baut den Release-Binary mit den gesammelten Profilen
             (`cargo pgo optimize`).

    Voraussetzungen:
      - cargo-pgo (cargo install cargo-pgo)
      - rustup component add llvm-tools

.PARAMETER Clean
    Vor Stage 1 alte Profile und Build-Artefakte löschen
    (`cargo pgo clean`). Stellt sicher, dass Profile-Daten nicht
    von einer früheren Toolchain stammen.

.EXAMPLE
    .\scripts\build-pgo.ps1
    Standardlauf: Stage 1 + Stage 2 ohne Clean.

.EXAMPLE
    .\scripts\build-pgo.ps1 -Clean
    Frischer Lauf nach Toolchain-Update oder Profilwechsel.

.NOTES
    Workload aktuell = `cargo test`. Sobald der Bot vollständig
    portiert ist, sollte hier ein realistischeres Szenario laufen
    (z.B. `examples/pgo_workload.rs` mit NDJSON-Fixtures).
#>

[CmdletBinding()]
param(
    [switch]$Clean
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

if ($Clean) {
    Write-Host '==> cargo pgo clean' -ForegroundColor Cyan
    cargo pgo clean
    if ($LASTEXITCODE -ne 0) { throw "cargo pgo clean fehlgeschlagen ($LASTEXITCODE)" }
}

Write-Host '==> Stage 1: instrumentierter Test-Build + Workload' -ForegroundColor Cyan
cargo pgo test
if ($LASTEXITCODE -ne 0) { throw "Stage 1 (cargo pgo test) fehlgeschlagen ($LASTEXITCODE)" }

$profileDir = Join-Path $repoRoot 'target\pgo-profiles'
$profRaw = Get-ChildItem -Path $profileDir -Filter '*.profraw' -ErrorAction SilentlyContinue
if (-not $profRaw) {
    throw "Keine .profraw-Dateien in $profileDir gefunden — Stage 2 würde keinen Nutzen bringen."
}
Write-Host ("    {0} .profraw-Datei(en) gesammelt" -f $profRaw.Count) -ForegroundColor DarkGray

Write-Host '==> Stage 2: optimierter Release-Build mit Profilen' -ForegroundColor Cyan
cargo pgo optimize
if ($LASTEXITCODE -ne 0) { throw "Stage 2 (cargo pgo optimize) fehlgeschlagen ($LASTEXITCODE)" }

$binary = Join-Path $repoRoot 'target\x86_64-pc-windows-msvc\release\liru-bot.exe'
if (Test-Path $binary) {
    $size = (Get-Item $binary).Length / 1KB
    Write-Host ("==> PGO-optimierter Binary: {0} ({1:N0} KB)" -f $binary, $size) -ForegroundColor Green
} else {
    Write-Warning "Erwarteter Binary $binary nicht gefunden."
}
