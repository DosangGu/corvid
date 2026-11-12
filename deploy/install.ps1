#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Version,
    [ValidateSet('tui', 'desktop')][string]$Product = 'tui',
    [string]$InstallRoot,
    [string]$SourceDirectory,
    [string]$ArchiveSha256
)
$ErrorActionPreference = 'Stop'
# PowerShell 7 callers may pass a module path incompatible with Windows PowerShell.
$env:PSModulePath = Join-Path $PSHOME 'Modules'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
$repository = 'DosangGu/corvid'
if (![Environment]::Is64BitOperatingSystem -or $env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
    throw 'Corvid requires Windows x64.'
}
if (!$Version) {
    if ($SourceDirectory) { throw '-SourceDirectory requires -Version' }
    $release = Invoke-RestMethod "https://api.github.com/repos/$repository/releases/latest"
    $Version = $release.tag_name
}
$Version = $Version -creplace '^v', ''
if ($Version -cnotmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$') {
    throw 'Invalid release version'
}
if (!$InstallRoot) { $InstallRoot = Join-Path $env:LOCALAPPDATA "Corvid\$Product" }
if (![IO.Path]::IsPathRooted($InstallRoot)) { throw 'InstallRoot must be absolute' }
$InstallRoot = [IO.Path]::GetFullPath($InstallRoot)
if ($InstallRoot.Contains("`n") -or $InstallRoot.Contains("`r")) { throw 'Installation paths cannot contain newlines' }
$bin = Join-Path $InstallRoot 'bin'
$base = "https://github.com/$repository/releases/download/v$Version"
New-Item -ItemType Directory -Path $InstallRoot -Force | Out-Null
$lock = $null
$stage = $null

function Get-Asset([string]$name, [string]$destination) {
    if ($SourceDirectory) {
        Copy-Item -LiteralPath (Join-Path $SourceDirectory $name) -Destination $destination
    } else {
        & curl.exe --fail --silent --show-error --location --proto '=https' --proto-redir '=https' --connect-timeout 15 --max-time 300 --retry 2 --max-filesize 1073741824 --output $destination "$base/$name"
        if ($LASTEXITCODE -ne 0) { throw "Download failed: $name" }
    }
}

function Assert-Package([string]$directory) {
    $prefix = [IO.Path]::GetFullPath($directory) + [IO.Path]::DirectorySeparatorChar
    foreach ($line in [IO.File]::ReadAllLines((Join-Path $directory 'files.sha256'))) {
        if ($line -cnotmatch '^([0-9a-f]{64})  (.+)$') { throw 'Invalid file checksum record' }
        $digest = $Matches[1]
        $path = [IO.Path]::GetFullPath((Join-Path $directory $Matches[2]))
        if (!$path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Unsafe package path' }
        if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -ine $digest) { throw "File checksum mismatch: $path" }
    }
}

function Set-AtomicFile([string]$source, [string]$destination) {
    if (Test-Path -LiteralPath $destination) {
        $backup = Join-Path $stage ('previous-' + [guid]::NewGuid())
        [IO.File]::Replace($source, $destination, $backup)
    } else {
        [IO.File]::Move($source, $destination)
    }
}

try {
    try {
        $lock = [IO.File]::Open((Join-Path $InstallRoot '.install-lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    } catch { throw 'Another installation is running.' }
    $receiptPath = Join-Path $InstallRoot 'install.json'
    if (Test-Path -LiteralPath $receiptPath) {
        $receipt = Get-Content -LiteralPath $receiptPath -Raw | ConvertFrom-Json
        if ($receipt.schema_version -ne 1 -or $receipt.product -cne $Product) { throw 'Installation receipt mismatch' }
    } elseif ((Test-Path -LiteralPath (Join-Path $InstallRoot 'versions')) -or (Test-Path -LiteralPath $bin)) {
        throw 'Unrecognized installation directory'
    }
    $stage = Join-Path $InstallRoot ('.stage-' + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $stage | Out-Null
    $archiveName = "corvid-$Product-$Version-x86_64-pc-windows-msvc.zip"
    $archive = Join-Path $stage 'archive.zip'
    $checksum = Join-Path $stage 'checksum'
    Get-Asset "$archiveName.sha256" $checksum
    $digest = ((Get-Content -LiteralPath $checksum -Raw) -split '\s+')[0]
    if ($digest -cnotmatch '^[0-9a-f]{64}$') { throw 'Invalid archive checksum' }
    if ($ArchiveSha256 -and $digest -cne $ArchiveSha256) { throw 'Release manifest and archive checksum disagree' }
    Get-Asset $archiveName $archive
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ine $digest) { throw 'Archive SHA-256 mismatch' }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $package = Join-Path $stage 'package'
    New-Item -ItemType Directory -Path $package | Out-Null
    $prefix = [IO.Path]::GetFullPath($package) + [IO.Path]::DirectorySeparatorChar
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    try {
        foreach ($entry in $zip.Entries) {
            $path = [IO.Path]::GetFullPath((Join-Path $package $entry.FullName))
            if (!$path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase) -or $entry.FullName.Contains(':')) { throw 'Unsafe archive path' }
            if ((($entry.ExternalAttributes -shr 16) -band 0xF000) -eq 0xA000) { throw 'Archive links are not supported' }
        }
    } finally { $zip.Dispose() }
    [IO.Compression.ZipFile]::ExtractToDirectory($archive, $package)
    Assert-Package $package
    $metadata = Get-Content -LiteralPath (Join-Path $package 'package.json') -Raw | ConvertFrom-Json
    if ($metadata.schema_version -ne 1 -or $metadata.version -cne $Version -or $metadata.product -cne $Product -or $metadata.target -cne 'x86_64-pc-windows-msvc') { throw 'Package metadata mismatch' }
    $binaryVersion = & (Join-Path $package 'corvid.exe') --version
    if ($LASTEXITCODE -ne 0 -or $binaryVersion -cne "corvid $Version") { throw 'Executable version mismatch' }
    $versions = Join-Path $InstallRoot 'versions'
    New-Item -ItemType Directory -Path $versions -Force | Out-Null
    $destination = Join-Path $versions $Version
    if (Test-Path -LiteralPath $destination) {
        $old = (Get-FileHash -LiteralPath (Join-Path $destination 'files.sha256') -Algorithm SHA256).Hash
        $new = (Get-FileHash -LiteralPath (Join-Path $package 'files.sha256') -Algorithm SHA256).Hash
        if ($old -ne $new) { throw 'An installed version has different contents; refusing to replace it' }
        Assert-Package $destination
    } else { [IO.Directory]::Move($package, $destination) }
    New-Item -ItemType Directory -Path $bin -Force | Out-Null
    $icon = Join-Path $destination 'corvid.ico'
    if (Test-Path -LiteralPath $icon) {
        Copy-Item -LiteralPath $icon -Destination (Join-Path $stage 'corvid.ico')
        Set-AtomicFile (Join-Path $stage 'corvid.ico') (Join-Path $InstallRoot 'corvid.ico')
    }
    # Stable launchers are installed once. Their v1 pointer contract is independent of app versions.
    $launchers = @{ 'corvid.exe' = 'corvid-launch.exe' }
    if ($Product -eq 'desktop') { $launchers['corvid-desktop.exe'] = 'corvid-desktop-launch.exe' }
    foreach ($entry in $launchers.GetEnumerator()) {
        $path = Join-Path $bin $entry.Key
        if (!(Test-Path -LiteralPath $path)) { Copy-Item -LiteralPath (Join-Path $destination $entry.Value) -Destination $path }
    }
    $utf8 = New-Object Text.UTF8Encoding($false)
    [IO.File]::WriteAllText((Join-Path $stage 'install.json'), (@{ schema_version = 1; product = $Product } | ConvertTo-Json), $utf8)
    [IO.File]::WriteAllText((Join-Path $stage 'bin-dir'), $bin, $utf8)
    [IO.File]::WriteAllText((Join-Path $stage 'current.txt'), $Version, $utf8)
    Set-AtomicFile (Join-Path $stage 'install.json') $receiptPath
    Set-AtomicFile (Join-Path $stage 'bin-dir') (Join-Path $InstallRoot 'bin-dir')
    Set-AtomicFile (Join-Path $stage 'current.txt') (Join-Path $InstallRoot 'current.txt')
    Write-Output "Installed Corvid $Version ($Product)."
    Write-Output "CORVID_EXECUTABLE=$(Join-Path $destination 'corvid.exe')"
    Write-Output "Add $bin to PATH. Desktop users can launch $(Join-Path $bin 'corvid-desktop.exe')."
    Write-Output 'Running daemons were left unchanged. Use corvid upgrade for an automatically started daemon; use the service manager for a supervised daemon.'
} finally {
    if ($stage -and (Test-Path -LiteralPath $stage)) {
        $resolvedStage = [IO.Path]::GetFullPath($stage)
        $expectedPrefix = [IO.Path]::GetFullPath($InstallRoot) + [IO.Path]::DirectorySeparatorChar + '.stage-'
        if (!$resolvedStage.StartsWith($expectedPrefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Refusing cleanup outside installer staging directory' }
        Remove-Item -LiteralPath $resolvedStage -Recurse -Force
    }
    if ($lock) { $lock.Dispose() }
}
