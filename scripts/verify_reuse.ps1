$ErrorActionPreference = 'Continue'
$appExe = 'F:\Dev\SnapDrop\src-tauri\target\release\snapdrop.exe'
$folder = 'C:\Users\Izzy\Pictures\SnapDrop'
$uri = 'file:///' + $folder.Replace('\', '/')

Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class U32 {
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder sb, int max);
  [DllImport("user32.dll")] public static extern void keybd_event(byte bKey, byte bScan, uint dwFlags, IntPtr dwExtraInfo);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, int dx, int dy, uint dwData, IntPtr dwExtraInfo);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  public const byte VK_CONTROL = 0x11;
  public const uint KEYEVENTF_KEYUP = 0x0002;
  public const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
  public const uint MOUSEEVENTF_LEFTUP = 0x0004;
}
"@

function Count-WindowsOnFolder {
  $sw = New-Object -ComObject Shell.Application
  $n = 0
  foreach ($w in $sw.Windows()) {
    try { if ($w.LocationURL -eq $uri) { $n++ } } catch {}
  }
  return $n
}

Write-Output "=== Stop app ==="
Stop-Process -Name snapdrop -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 800

Write-Output "=== Open exactly 1 Explorer on folder ==="
$sw = New-Object -ComObject Shell.Application
foreach ($w in @($sw.Windows())) {
  try { if ($w.LocationURL -eq $uri) { $w.Quit() } } catch {}
}
Start-Sleep -Milliseconds 800
Start-Process explorer.exe -ArgumentList "`"$folder`""
Start-Sleep -Milliseconds 1800
$before = Count-WindowsOnFolder
Write-Output "  before: $before"
if ($before -ne 1) { Write-Output "  FAIL setup"; exit 1 }

Write-Output "=== Launch app ==="
Start-Process $appExe
Start-Sleep -Seconds 3

Write-Output "=== Trigger capture hotkey ==="
$hk = [U32]::FindWindow('global_hotkey_app', $null)
[U32]::PostMessage($hk, 0x0312, [IntPtr]0x02080009, [IntPtr]::Zero)
Start-Sleep -Milliseconds 1200

Write-Output "=== Draw selection ==="
$ov = [U32]::FindWindow('SnapDropOverlayClass', $null)
if ($ov -eq [IntPtr]::Zero) { Write-Output "  FAIL: overlay not found"; exit 1 }
$lp1 = [IntPtr]( (300 -shl 16) -bor 300 )
$lp2 = [IntPtr]( (700 -shl 16) -bor 900 )
[U32]::PostMessage($ov, 0x0201, [IntPtr]1, $lp1)
Start-Sleep -Milliseconds 50
[U32]::PostMessage($ov, 0x0200, [IntPtr]1, $lp2)
Start-Sleep -Milliseconds 50
[U32]::PostMessage($ov, 0x0202, [IntPtr]0, $lp2)
Start-Sleep -Milliseconds 2500

Write-Output "=== Dismiss editor via Esc ==="
[U32]::PostMessage($hk, 0x0312, [IntPtr]114, [IntPtr]::Zero)
Start-Sleep -Milliseconds 2000

Write-Output "=== Ctrl+click thumbnail ==="
$thumb = [U32]::FindWindow($null, 'SnapDrop Capture')
if ($thumb -eq [IntPtr]::Zero) { Write-Output "  FAIL: thumbnail not found"; exit 1 }
$rect = New-Object U32+RECT
[U32]::GetWindowRect($thumb, [ref]$rect)
$cx = [int](($rect.L + $rect.R) / 2)
$cy = [int](($rect.T + $rect.B) / 2)
Write-Output "  thumbnail: ($($rect.L),$($rect.T))-($($rect.R),$($rect.B)) center=($cx,$cy)"

[U32]::keybd_event([U32]::VK_CONTROL, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 50
[U32]::SetCursorPos($cx, $cy)
Start-Sleep -Milliseconds 100
[U32]::mouse_event([U32]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 50
[U32]::mouse_event([U32]::MOUSEEVENTF_LEFTUP, 0, 0, 0, [IntPtr]::Zero)
Start-Sleep -Milliseconds 100
[U32]::keybd_event([U32]::VK_CONTROL, 0, [U32]::KEYEVENTF_KEYUP, [IntPtr]::Zero)
Start-Sleep -Milliseconds 3000

Write-Output "=== Verify ==="
$after = Count-WindowsOnFolder
Write-Output "  before=$before after=$after"
if ($after -ne $before) {
  Write-Output "  FAIL: duplicate window ($before -> $after)"
} else {
  Write-Output "  PASS: no duplicate - reuse working"
}
$fg = [U32]::GetForegroundWindow()
$sb = New-Object System.Text.StringBuilder 512
[U32]::GetWindowText($fg, $sb, 512)
Write-Output "  foreground: '$($sb.ToString())'"