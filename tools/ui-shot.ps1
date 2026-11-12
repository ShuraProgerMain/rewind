# Test aid: screenshots the Rewind settings window (with a margin, to see its edge).
# The window is put above everything for the moment of the shot, so it does not matter
# what the user is doing in other windows.
# Usage: ui-shot.ps1 -Out file.png [-Margin 30]
param([Parameter(Mandatory)] [string]$Out, [int]$Margin = 30, [switch]$Activate, [string]$Class = 'RewindSettings')
Add-Type -AssemblyName System.Drawing
Add-Type -Namespace W -Name U -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern System.IntPtr FindWindowW(string cls, string title);
[DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr h);
[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(System.IntPtr h);
[DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, System.UIntPtr extra);
[DllImport("user32.dll")] public static extern bool SetWindowPos(System.IntPtr h, System.IntPtr after, int x, int y, int w, int hh, uint flags);
public struct RECT { public int L, T, R, B; }
'@
[void][W.U]::SetProcessDPIAware()
$h = [W.U]::FindWindowW($Class, [NullString]::Value)
if ($h -eq [IntPtr]::Zero) { 'settings window not found'; exit 1 }
$flags = 0x0001 -bor 0x0002 -bor 0x0010   # no size, no move, no activate
[void][W.U]::SetWindowPos($h, [IntPtr](-1), 0, 0, 0, 0, $flags)   # topmost
if ($Activate) {
    # A tap on Alt lifts the rule that only the foreground program may take the focus.
    [W.U]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero); [W.U]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero)
    [void][W.U]::SetForegroundWindow($h)
}
Start-Sleep -Milliseconds 350
$r = New-Object W.U+RECT
[void][W.U]::GetWindowRect($h, [ref]$r)
$w = $r.R - $r.L + 2 * $Margin; $hh = $r.B - $r.T + 2 * $Margin
$bmp = New-Object System.Drawing.Bitmap $w, $hh
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.L - $Margin, $r.T - $Margin, 0, 0, (New-Object System.Drawing.Size $w, $hh))
$bmp.Save($Out); $g.Dispose(); $bmp.Dispose()
[void][W.U]::SetWindowPos($h, [IntPtr](-2), 0, 0, 0, 0, $flags)   # back to normal
"window {0}x{1} at {2},{3}, visible={4}" -f ($r.R - $r.L), ($r.B - $r.T), $r.L, $r.T, [W.U]::IsWindowVisible($h)
