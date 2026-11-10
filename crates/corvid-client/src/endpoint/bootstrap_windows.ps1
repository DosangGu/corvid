$ErrorActionPreference = 'Stop'
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
$base = [Environment]::GetFolderPath('LocalApplicationData')

function Exists($p) {
    try { [void][IO.File]::GetAttributes($p); return $true }
    catch [IO.FileNotFoundException], [IO.DirectoryNotFoundException] { return $false }
}

function SafeBase {
    if ($base -notmatch '^[A-Za-z]:\\') { exit 71 }
    $drive = [IO.DriveInfo]::new([IO.Path]::GetPathRoot($base))
    if ($drive.DriveType -ne 'Fixed') { exit 71 }
    for ($p = $base; $p; $p = [IO.Path]::GetDirectoryName($p)) {
        $flags = [IO.File]::GetAttributes($p)
        if (($flags -band [IO.FileAttributes]::ReparsePoint) -or !($flags -band [IO.FileAttributes]::Directory)) { exit 71 }
    }
}

function Private($p, $directory) {
    $flags = [IO.File]::GetAttributes($p)
    if (($flags -band [IO.FileAttributes]::ReparsePoint) -or ([bool]($flags -band [IO.FileAttributes]::Directory) -ne $directory)) { exit 71 }
    $acl = if ($directory) { [IO.Directory]::GetAccessControl($p) } else { [IO.File]::GetAccessControl($p) }
    $type = [Security.Principal.SecurityIdentifier]
    $rules = $acl.GetAccessRules($true, $true, $type)
    if (!$acl.AreAccessRulesProtected -or $acl.GetOwner($type).Value -ne $sid.Value -or
        $rules.Count -ne 1 -or $rules[0].IdentityReference.Value -ne $sid.Value -or
        $rules[0].AccessControlType -ne 'Allow' -or $rules[0].FileSystemRights -ne 'FullControl') { exit 71 }
}

function ManagedPath {
    SafeBase
    Join-Path $base 'Corvid-SSH'
}

function RequireX64 {
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
    if ($arch -ne 'AMD64') { exit 70 }
}

function PathDaemon {
    (Get-Command corvid.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1).Source
}

function ManagedDaemon {
    $dir = ManagedPath
    if (!(Exists $dir)) { return }
    Private $dir $true
    $exe = Join-Path $dir 'corvid.exe'
    if (!(Exists $exe)) { return }
    Private $exe $false
    $exe
}

function RunDaemon($exe, $arguments) {
    if (!$exe) {
        [Console]::Error.WriteLine('corvid: not found')
        exit 127
    }
    # Inherit the SSH streams directly to preserve framed bytes.
    $start = [Diagnostics.ProcessStartInfo]::new($exe, $arguments)
    $start.UseShellExecute = $false
    $process = [Diagnostics.Process]::Start($start)
    try { $process.WaitForExit(); exit $process.ExitCode }
    finally { $process.Dispose() }
}
