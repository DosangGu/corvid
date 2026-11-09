#Requires -Version 5.1
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path
$target = 'x86_64-pc-windows-msvc'

Push-Location -LiteralPath $repo
try {
    $metadataJson = cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $metadata = $metadataJson | ConvertFrom-Json

    cargo build --locked --release --target $target -p corvid -p corvid-desktop
    if ($LASTEXITCODE -ne 0) { throw 'Corvid release build failed' }
    cargo build --locked --release --target $target --target-dir $metadata.target_directory --manifest-path 'crates/corvid-desktop/gui/Cargo.toml' --bin corvid-desktop
    if ($LASTEXITCODE -ne 0) { throw 'Corvid Desktop release build failed' }
    cargo build --locked --release --target $target --target-dir $metadata.target_directory --manifest-path 'spikes/deck/Cargo.toml' --bin corvid-deck
    if ($LASTEXITCODE -ne 0) { throw 'Corvid Deck release build failed' }

    $release = Join-Path $metadata.target_directory "$target\release"
    $output = Join-Path $metadata.target_directory 'windows-dist'
    New-Item -ItemType Directory -Path $output -Force | Out-Null
    $archive = Join-Path $output 'corvid-windows-x64.zip'
    $temporary = Join-Path $output ('.corvid-' + [guid]::NewGuid() + '.zip')
    $files = @(
        (Join-Path $release 'corvid.exe')
        (Join-Path $release 'corvid-desktop.exe')
        (Join-Path $release 'corvid-deck.exe')
        (Join-Path $release 'corvid-slot-board.exe')
        (Join-Path $PSScriptRoot 'Set-CorvidStartup.ps1')
        (Join-Path $repo 'LICENSE')
    )
    try {
        Compress-Archive -LiteralPath $files -DestinationPath $temporary
        Move-Item -LiteralPath $temporary -Destination $archive -Force
    } finally {
        if (Test-Path -LiteralPath $temporary -PathType Leaf) {
            Remove-Item -LiteralPath $temporary
        }
    }
    Get-Item -LiteralPath $archive
} finally {
    Pop-Location
}
