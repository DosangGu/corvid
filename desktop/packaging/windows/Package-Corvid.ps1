#Requires -Version 5.1
[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string]$LinuxDaemonDirectory,
    [ValidateNotNullOrEmpty()]
    [string]$WslDistribution
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..\..')).Path
$target = 'x86_64-pc-windows-msvc'
$linuxTarget = 'x86_64-unknown-linux-musl'

function Invoke-WslDaemonBuild([string]$distribution, [string]$buildDirectory) {
    $previousTargetDirectory = $env:CARGO_TARGET_DIR
    $previousWslEnv = $env:WSLENV
    try {
        $entries = @($previousWslEnv -split ':' | Where-Object { $_ -and $_ -notmatch '^CARGO_TARGET_DIR(?:/|$)' })
        $env:WSLENV = ($entries + 'CARGO_TARGET_DIR/pu') -join ':'
        $env:CARGO_TARGET_DIR = $buildDirectory

        wsl.exe --distribution $distribution --cd $repo --exec bash -l deploy/linux/package-corvid.sh
        if ($LASTEXITCODE -ne 0) { throw "Linux daemon build failed in WSL distribution '$distribution'" }
    } finally {
        $env:CARGO_TARGET_DIR = $previousTargetDirectory
        $env:WSLENV = $previousWslEnv
    }
}

function Read-LinuxDaemonAssets([string]$directory) {
    $revision = git rev-parse HEAD
    if ($LASTEXITCODE -ne 0) { throw 'Could not read the Windows source revision' }
    $buildInputs = @('Cargo.lock', 'Cargo.toml', 'rust-toolchain.toml', 'crates', 'spikes',
        'desktop/Cargo.lock', 'desktop/Cargo.toml', 'desktop/app')
    $changes = git -c core.safecrlf=false status --porcelain --untracked-files=normal -- @buildInputs
    if ($LASTEXITCODE -ne 0) { throw 'Could not check the Windows build inputs' }
    if ($changes) { throw 'Commit the build inputs before packaging with the Linux daemon' }

    $manifestName = "daemon/$linuxTarget/manifest.json"
    $binaryName = "daemon/$linuxTarget/corvid"
    $assets = @{
        $manifestName = [IO.File]::ReadAllBytes((Join-Path $directory 'manifest.json'))
        $binaryName = [IO.File]::ReadAllBytes((Join-Path $directory 'corvid'))
    }
    $manifest = [Text.Encoding]::UTF8.GetString($assets[$manifestName]) | ConvertFrom-Json
    if ($manifest.schema_version -ne 1 -or $manifest.target -cne $linuxTarget -or
        $manifest.version -cnotmatch '^[0-9A-Za-z.+-]+$') {
        throw 'Unsupported Linux daemon manifest or target'
    }
    if ($manifest.source_revision -cne $revision) {
        throw 'The Windows and Linux builds must use the same source revision'
    }

    $binary = $assets[$binaryName]
    $stream = [IO.MemoryStream]::new($binary)
    try {
        $digest = (Get-FileHash -InputStream $stream -Algorithm SHA256).Hash
    } finally {
        $stream.Dispose()
    }
    if ($binary.Length -eq 0 -or $binary.Length -ne $manifest.size -or $digest -ne $manifest.sha256) {
        throw 'The Linux daemon size or SHA-256 does not match the manifest'
    }
    return $assets
}

if ($LinuxDaemonDirectory -and $WslDistribution) {
    throw 'Choose either -LinuxDaemonDirectory or -WslDistribution'
}
if ($LinuxDaemonDirectory) {
    $LinuxDaemonDirectory = (Resolve-Path -LiteralPath $LinuxDaemonDirectory).Path
}

Push-Location -LiteralPath $repo
try {
    $metadataJson = cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $metadata = $metadataJson | ConvertFrom-Json

    if ($WslDistribution) {
        $linuxBuildDirectory = Join-Path $metadata.target_directory 'wsl-linux'
        Invoke-WslDaemonBuild $WslDistribution $linuxBuildDirectory
        $LinuxDaemonDirectory = Join-Path $linuxBuildDirectory "linux-dist/$linuxTarget"
    }

    $archiveName = 'corvid-windows-x64.zip'
    $daemonAssets = $null
    if ($LinuxDaemonDirectory) {
        $daemonAssets = Read-LinuxDaemonAssets $LinuxDaemonDirectory
        $archiveName = 'corvid-windows-x64-with-daemon.zip'
    }

    cargo build --locked --release --target $target -p corvid -p corvid-desktop
    if ($LASTEXITCODE -ne 0) { throw 'Corvid release build failed' }
    cargo build --locked --release --target $target --target-dir $metadata.target_directory --manifest-path 'desktop/Cargo.toml' --bin corvid-desktop
    if ($LASTEXITCODE -ne 0) { throw 'Corvid Desktop release build failed' }
    cargo build --locked --release --target $target --target-dir $metadata.target_directory --manifest-path 'spikes/deck/Cargo.toml' --bin corvid-deck
    if ($LASTEXITCODE -ne 0) { throw 'Corvid Deck release build failed' }

    $release = Join-Path $metadata.target_directory "$target\release"
    $output = Join-Path $metadata.target_directory 'windows-dist'
    New-Item -ItemType Directory -Path $output -Force | Out-Null
    $archive = Join-Path $output $archiveName
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
        if ($daemonAssets) {
            Add-Type -AssemblyName System.IO.Compression.FileSystem
            $zip = [IO.Compression.ZipFile]::Open($temporary, [IO.Compression.ZipArchiveMode]::Update)
            try {
                foreach ($asset in $daemonAssets.GetEnumerator()) {
                    $entry = $zip.CreateEntry($asset.Key).Open()
                    try {
                        $entry.Write($asset.Value, 0, $asset.Value.Length)
                    } finally {
                        $entry.Dispose()
                    }
                }
            } finally {
                $zip.Dispose()
            }
        }
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
