#Requires -Version 5.1
[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string]$LinuxDaemonDirectory,
    [ValidateNotNullOrEmpty()]
    [string]$WslDistribution
)

$packager = Join-Path $PSScriptRoot '..\..\desktop\packaging\windows\Package-Corvid.ps1'
& $packager @PSBoundParameters
