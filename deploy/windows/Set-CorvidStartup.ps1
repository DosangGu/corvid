#Requires -Version 5.1
[CmdletBinding(SupportsShouldProcess)]
param(
    [ValidateSet('Enable', 'Disable', 'Status')]
    [string] $Mode = 'Status'
)

$ErrorActionPreference = 'Stop'
$startup = [Environment]::GetFolderPath([Environment+SpecialFolder]::Startup)
if (-not $startup -or -not (Test-Path -LiteralPath $startup -PathType Container)) {
    throw 'The current user Startup folder is unavailable.'
}

$path = Join-Path $startup 'Corvid Companion.lnk'
$description = 'Corvid companion logon startup (Set-CorvidStartup.ps1)'
$arguments = 'companion run'
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($path)
$enabled = Test-Path -LiteralPath $path -PathType Leaf
if ($enabled -and ($shortcut.Description -cne $description -or $shortcut.Arguments -cne $arguments)) {
    throw 'A different Corvid Companion shortcut already exists in the Startup folder.'
}

switch ($Mode) {
    'Enable' {
        $binary = Join-Path $PSScriptRoot 'corvid.exe'
        $versions = Split-Path -Parent $PSScriptRoot
        $installation = Split-Path -Parent $versions
        if ((Split-Path -Leaf $versions) -eq 'versions' -and
            (Test-Path -LiteralPath (Join-Path $installation 'install.json'))) {
            $binary = Join-Path $installation 'bin/corvid.exe'
        }
        if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
            throw 'Extract the Windows package and run this script beside corvid.exe.'
        }
        if ($PSCmdlet.ShouldProcess($path, 'Enable Corvid companion startup for the current user')) {
            $shortcut.TargetPath = $binary
            $shortcut.WorkingDirectory = Split-Path -Parent $binary
            $shortcut.Description = $description
            $shortcut.Arguments = $arguments
            $shortcut.WindowStyle = 7
            $shortcut.Save()
            Write-Output "Corvid startup enabled at the next logon. Target: $binary"
        }
    }
    'Disable' {
        if ($enabled -and $PSCmdlet.ShouldProcess($path, 'Disable Corvid companion startup')) {
            Remove-Item -LiteralPath $path
            Write-Output 'Corvid startup disabled.'
        } elseif (-not $enabled) {
            Write-Output 'Corvid startup is already disabled.'
        }
    }
    'Status' {
        if ($enabled) {
            Write-Output "Corvid startup target: $($shortcut.TargetPath)"
            if (-not (Test-Path -LiteralPath $shortcut.TargetPath -PathType Leaf)) {
                Write-Output 'The startup target is missing. Run -Mode Enable from the extracted package.'
            }
        } else {
            Write-Output 'Corvid startup is disabled.'
        }
    }
}
