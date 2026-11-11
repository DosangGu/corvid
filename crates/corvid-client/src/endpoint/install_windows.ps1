if (PathDaemon) { exit 74 }
RequireX64

function Restrict($acl) {
    $acl.SetOwner($sid)
    $acl.SetAccessRuleProtection($true, $false)
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', 'Allow'))
}

function Matches($p) {
    Private $p $false
    $file = [IO.File]::OpenRead($p)
    $hasher = [Security.Cryptography.SHA256]::Create()
    try {
        $actual = [BitConverter]::ToString($hasher.ComputeHash($file)).Replace('-', '')
        return $file.Length -eq $size -and $actual -eq $sha
    } finally {
        $hasher.Dispose()
        $file.Dispose()
    }
}

if ($size -lt 1 -or $size -gt 33554432 -or $sha -cnotmatch '^[0-9a-f]{64}$') { exit 72 }
$tar = Join-Path ([Environment]::SystemDirectory) 'tar.exe'
if (![IO.File]::Exists($tar)) { exit 75 }

$dir = ManagedPath
$acl = [Security.AccessControl.DirectorySecurity]::new()
Restrict $acl
[void][IO.Directory]::CreateDirectory($dir, $acl)
Private $dir $true
$exe = Join-Path $dir 'corvid.exe'
$extension = if ($version) { '.exe' } else { '.tmp' }
$stage = Join-Path $dir ('.corvid-' + [Guid]::NewGuid().ToString('N') + $extension)
$acl = [Security.AccessControl.FileSecurity]::new()
Restrict $acl
try {
    $file = [IO.FileStream]::new($stage, 'CreateNew', 'Write', 'None', 65536, 'None', $acl)
    try {
        # Native tar consumes the SSH pipe. Stream only the named member and
        # write it ourselves, so archive paths never select a destination.
        $start = [Diagnostics.ProcessStartInfo]::new($tar, '-xOf - -- corvid.exe')
        $start.UseShellExecute = $false
        $start.RedirectStandardOutput = $true
        $process = [Diagnostics.Process]::Start($start)
        try {
            $stream = $process.StandardOutput.BaseStream
            $left = $size
            $buffer = [byte[]]::new(65536)
            while ($left -gt 0) {
                $count = $stream.Read($buffer, 0, [Math]::Min($left, $buffer.Length))
                if (!$count) { exit 72 }
                $file.Write($buffer, 0, $count)
                $left -= $count
            }
            if ($stream.ReadByte() -ne -1) { exit 72 }
            $process.WaitForExit()
            if ($process.ExitCode -ne 0) { exit 72 }
            $file.Flush($true)
        } finally {
            if (!$process.HasExited) { $process.Kill() }
            $process.Dispose()
        }
    } finally { $file.Dispose() }
    if (!(Matches $stage)) { exit 72 }
    if ($version) {
        if (!(ManagedDaemon)) { exit 74 }
        $arguments = "upgrade --ssh --managed-ssh-from $version"
        if ($hostId) { $arguments += " --expected-host $hostId" }
        RunDaemon $stage $arguments
    }
    try { [IO.File]::Move($stage, $exe) }
    catch [IO.IOException] {
        if (!(Exists $exe)) { throw }
        if (!(Matches $exe)) { exit 74 }
    }
} finally {
    [IO.File]::Delete($stage)
}
