#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$MacosDaemonDirectory,
    [string]$LinuxDaemonDirectory,
    [string]$WindowsDaemonDirectory,
    [string]$LinuxArm64DaemonDirectory,
    [string]$WindowsArm64DaemonDirectory,
    [string]$WslDistribution,
    [switch]$WithoutAgentDeck
)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path
$architecture = [Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITECTURE', 'Machine')
$target = switch ($architecture) {
    'AMD64' { 'x86_64-pc-windows-msvc' }
    'ARM64' { 'aarch64-pc-windows-msvc' }
    default { throw 'Build on Windows x64 or ARM64.' }
}
Push-Location -LiteralPath $repo
try {
    $nativeLinux = if ($architecture -eq 'ARM64') { $LinuxArm64DaemonDirectory } else { $LinuxDaemonDirectory }
    if ($nativeLinux -and $WslDistribution) { throw 'Choose a native Linux daemon directory or WslDistribution' }
    $metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $build = Join-Path $metadata.target_directory "$target/release"
    if (!$nativeLinux) {
        if (!$WslDistribution) { $WslDistribution = 'Ubuntu' }
        $previousTarget = $env:CARGO_TARGET_DIR
        $previousWslEnv = $env:WSLENV
        try {
            $env:CARGO_TARGET_DIR = Join-Path $metadata.target_directory 'wsl-linux'
            $entries = @($previousWslEnv -split ':' | Where-Object { $_ -and $_ -notmatch '^CARGO_TARGET_DIR(?:/|$)' })
            $env:WSLENV = ($entries + 'CARGO_TARGET_DIR/pu') -join ':'
            wsl.exe --distribution $WslDistribution --cd $repo --exec bash -l deploy/linux/package-corvid.sh
            if ($LASTEXITCODE -ne 0) { throw 'Linux runtime build failed' }
            if ($architecture -eq 'ARM64') {
                $LinuxArm64DaemonDirectory = Join-Path $env:CARGO_TARGET_DIR 'linux-dist/aarch64-unknown-linux-musl'
            } else {
                $LinuxDaemonDirectory = Join-Path $env:CARGO_TARGET_DIR 'linux-dist/x86_64-unknown-linux-musl'
            }
        } finally { $env:CARGO_TARGET_DIR = $previousTarget; $env:WSLENV = $previousWslEnv }
    }
    $nativeRuntime = if ($architecture -eq 'ARM64') { $WindowsArm64DaemonDirectory } else { $WindowsDaemonDirectory }
    rustup target add $target
    if ($LASTEXITCODE -ne 0) { throw 'Rust target installation failed' }
    if (!$nativeRuntime) {
        cargo build --locked --release --target $target -p corvid-tui -p corvid-update
        if ($LASTEXITCODE -ne 0) { throw 'Windows runtime build failed' }
        $nativeRuntime = Join-Path $metadata.target_directory "runtime/$target"
        python deploy/package.py runtime --target $target --binary (Join-Path $build 'corvid.exe') --output $nativeRuntime
        if ($LASTEXITCODE -ne 0) { throw 'Windows runtime packaging failed' }
    }
    if ($architecture -eq 'ARM64') { $WindowsArm64DaemonDirectory = $nativeRuntime } else { $WindowsDaemonDirectory = $nativeRuntime }
    if (!$LinuxDaemonDirectory -or !$LinuxArm64DaemonDirectory -or !$WindowsDaemonDirectory -or !$WindowsArm64DaemonDirectory) {
        throw 'Supply revision-matched Linux and Windows daemon directories for both x64 and ARM64. Only native runtimes can be built here.'
    }
    $packages = @('-p', 'corvid-desktop', '-p', 'corvid-deck', '-p', 'corvid-update')
    if (!$WithoutAgentDeck) { $packages += @('-p', 'corvid-agent-deck') }
    cargo build --locked --release --target $target @packages
    if ($LASTEXITCODE -ne 0) { throw 'Desktop build failed' }
    $output = Join-Path $metadata.target_directory 'windows-dist'
    python deploy/package.py package --product tui --target $target --runtime $nativeRuntime --build $build --output $output
    if ($LASTEXITCODE -ne 0) { throw 'TUI packaging failed' }
    $options = @()
    if ($WithoutAgentDeck) { $options += '--without-agent-deck' }
    python deploy/package.py package --product desktop --target $target --runtime $nativeRuntime --linux-runtime $LinuxDaemonDirectory --windows-runtime $WindowsDaemonDirectory --linux-arm64-runtime $LinuxArm64DaemonDirectory --windows-arm64-runtime $WindowsArm64DaemonDirectory --macos-runtime $MacosDaemonDirectory --build $build --output $output @options
    if ($LASTEXITCODE -ne 0) { throw 'Desktop packaging failed' }
    Get-ChildItem -LiteralPath $output -Filter '*.zip'
} finally { Pop-Location }
