$code = @"
using System;
using System.Runtime.InteropServices;
public class IconProbe {
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern uint PrivateExtractIcons(string file, int index, int cx, int cy, IntPtr[] icons, uint[] ids, uint nIcons, uint flags);
    [DllImport("user32.dll")]
    public static extern bool DestroyIcon(IntPtr handle);
}
"@
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition $code
$exe = 'D:\Workspace\gitee.com\dsh-launcher\target\release\dsh-launcher.exe'
foreach ($size in 16, 32, 48, 256) {
    $icons = New-Object IntPtr[] 1
    $ids = New-Object uint32[] 1
    $count = [IconProbe]::PrivateExtractIcons($exe, 0, $size, $size, $icons, $ids, 1, 0)
    if ($count -gt 0) {
        $bitmap = [System.Drawing.Icon]::FromHandle($icons[0]).ToBitmap()
        $center = $bitmap.GetPixel([int]($bitmap.Width/2), [int]($bitmap.Height/2))
        $corner = $bitmap.GetPixel(0, 0)
        "size=$size loaded=$($bitmap.Width)x$($bitmap.Height) center=$($center.R),$($center.G),$($center.B),$($center.A) cornerAlpha=$($corner.A)"
        [IconProbe]::DestroyIcon($icons[0]) | Out-Null
    } else {
        "size=$size MISSING"
    }
}
