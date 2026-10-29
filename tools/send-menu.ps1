param(
    [int]$Command,
    [string]$WindowClass = 'DshLauncherTrayWindow'
)

Add-Type -Namespace Probe -Name User32 -MemberDefinition @'
public delegate bool EnumProc(System.IntPtr h, System.IntPtr p);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool EnumWindows(EnumProc cb, System.IntPtr p);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern uint GetWindowThreadProcessId(System.IntPtr h, out uint pid);
[System.Runtime.InteropServices.DllImport("user32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern int GetClassNameW(System.IntPtr h, System.Text.StringBuilder sb, int n);
[System.Runtime.InteropServices.DllImport("user32.dll")]
public static extern bool PostMessageW(System.IntPtr hWnd, uint msg, System.IntPtr wParam, System.IntPtr lParam);
'@

$script:found = [System.Collections.ArrayList]::new()
$callback = [Probe.User32+EnumProc] {
    param($handle, $unused)
    $builder = [System.Text.StringBuilder]::new(256)
    [void][Probe.User32]::GetClassNameW($handle, $builder, 256)
    if ($builder.ToString() -eq $WindowClass) {
        [void]$script:found.Add($handle)
    }
    return $true
}
[void][Probe.User32]::EnumWindows($callback, [System.IntPtr]::Zero)

if ($script:found.Count -eq 0) {
    Write-Output 'window not found'
    exit 1
}

foreach ($handle in $script:found) {
    $ok = [Probe.User32]::PostMessageW($handle, 0x0111, [System.IntPtr]$Command, [System.IntPtr]::Zero)
    Write-Output ('window=' + $handle.ToString() + ' posted=' + $ok)
}
