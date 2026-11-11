#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$LinuxDaemonDirectory,
    [string]$WindowsDaemonDirectory,
    [string]$WslDistribution,
    [switch]$WithoutAgentDeck
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path
$target = 'x86_64-pc-windows-msvc'
$linuxTarget = 'x86_64-unknown-linux-musl'
Push-Location -LiteralPath $repo
try {
    if ($LinuxDaemonDirectory -and $WslDistribution) { throw 'Choose LinuxDaemonDirectory or WslDistribution' }
    $metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $build = Join-Path $metadata.target_directory "$target/release"
    if (!$LinuxDaemonDirectory) {
        if (!$WslDistribution) { $WslDistribution = 'Ubuntu' }
        $previousTarget = $env:CARGO_TARGET_DIR
        $previousWslEnv = $env:WSLENV
        try {
            $env:CARGO_TARGET_DIR = Join-Path $metadata.target_directory 'wsl-linux'
            $entries = @($previousWslEnv -split ':' | Where-Object { $_ -and $_ -notmatch '^CARGO_TARGET_DIR(?:/|$)' })
            $env:WSLENV = ($entries + 'CARGO_TARGET_DIR/pu') -join ':'
            wsl.exe --distribution $WslDistribution --cd $repo --exec bash -l deploy/linux/package-corvid.sh
            if ($LASTEXITCODE -ne 0) { throw 'Linux runtime build failed' }
            $LinuxDaemonDirectory = Join-Path $env:CARGO_TARGET_DIR "linux-dist/$linuxTarget"
        } finally { $env:CARGO_TARGET_DIR = $previousTarget; $env:WSLENV = $previousWslEnv }
    }
    if (!$WindowsDaemonDirectory) {
        cargo build --locked --release --target $target -p corvid-tui -p corvid-update
        if ($LASTEXITCODE -ne 0) { throw 'Windows runtime build failed' }
        $WindowsDaemonDirectory = Join-Path $metadata.target_directory "runtime/$target"
        python deploy/package.py runtime --target $target --binary (Join-Path $build 'corvid.exe') --output $WindowsDaemonDirectory
        if ($LASTEXITCODE -ne 0) { throw 'Windows runtime packaging failed' }
    }
    $packages = @('-p', 'corvid-desktop', '-p', 'corvid-deck', '-p', 'corvid-update')
    if (!$WithoutAgentDeck) { $packages += @('-p', 'corvid-agent-deck') }
    cargo build --locked --release --target $target @packages
    if ($LASTEXITCODE -ne 0) { throw 'Desktop build failed' }
    $output = Join-Path $metadata.target_directory 'windows-dist'
    python deploy/package.py package --product tui --target $target --runtime $WindowsDaemonDirectory --build $build --output $output
    if ($LASTEXITCODE -ne 0) { throw 'TUI packaging failed' }
    $options = @()
    if ($WithoutAgentDeck) { $options += '--without-agent-deck' }
    python deploy/package.py package --product desktop --target $target --runtime $WindowsDaemonDirectory --linux-runtime $LinuxDaemonDirectory --windows-runtime $WindowsDaemonDirectory --build $build --output $output @options
    if ($LASTEXITCODE -ne 0) { throw 'Desktop packaging failed' }
    Get-ChildItem -LiteralPath $output -Filter '*.zip'
} finally { Pop-Location }
