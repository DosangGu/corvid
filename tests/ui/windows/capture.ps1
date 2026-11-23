param([long]$Window, [string]$Destination)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class Capture {
    [StructLayout(LayoutKind.Sequential)]
    public struct Rect { public int Left, Top, Right, Bottom; }

    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr window, out Rect rect);

    [DllImport("user32.dll")]
    public static extern bool PrintWindow(IntPtr window, IntPtr device, uint flags);
}
'@

$handle = [IntPtr]::new($Window)
$rect = [Capture+Rect]::new()
if (![Capture]::GetWindowRect($handle, [ref]$rect)) { throw 'Fixture window is gone' }
$bitmap = [Drawing.Bitmap]::new(($rect.Right - $rect.Left), ($rect.Bottom - $rect.Top))
try {
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    try {
        $device = $graphics.GetHdc()
        try {
            if (![Capture]::PrintWindow($handle, $device, 2)) { throw 'PrintWindow failed' }
        } finally {
            $graphics.ReleaseHdc($device)
        }
    } finally {
        $graphics.Dispose()
    }
    $bitmap.Save($Destination, [Drawing.Imaging.ImageFormat]::Png)
} finally {
    $bitmap.Dispose()
}
