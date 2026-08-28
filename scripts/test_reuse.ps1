param([string]$Action = "snap")
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class T3 {
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool EnumWindows(T3.E cb, IntPtr lp);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern int GetClassNameW(IntPtr h, [Out] char[] b, int n);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  public delegate bool E(IntPtr h, IntPtr lp);
}
"@
function Get-SnapWins {
  $list = New-Object System.Collections.ArrayList
  $cb = [T3+E]{
    param($h, $lp)
    $pid2 = 0
    [T3]::GetWindowThreadProcessId($h, [ref]$pid2) | Out-Null
    $p = Get-Process -Id $pid2 -ErrorAction SilentlyContinue
    if ($p -and $p.ProcessName -like "*snapdrop*") {
      $c = New-Object char[] 100
      [T3]::GetClassNameW($h, $c, 100) | Out-Null
      $null = $list.Add([pscustomobject]@{ H = $h; Class = ((-join $c) -replace "`0", ""); Visible = [T3]::IsWindowVisible($h) })
    }
    return $true
  }
  [T3]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
  return $list
}
switch ($Action) {
  "hotkey" {
    foreach ($w in (Get-SnapWins)) { if ($w.Class -like "*global_hotkey*") { [T3]::PostMessage($w.H, 0x0312, [IntPtr]0x02080009, [IntPtr]::Zero) | Out-Null; Write-Host "hotkey posted" } }
  }
  "select" {
    [T3]::SetCursorPos(220, 220) | Out-Null
    Start-Sleep -Milliseconds 250
    [T3]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 250
    [T3]::SetCursorPos(480, 380) | Out-Null
    Start-Sleep -Milliseconds 250
    [T3]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
    Write-Host "selection sent"
  }
  "esc" {
    foreach ($w in (Get-SnapWins)) { if ($w.Class -like "*global_hotkey*") { [T3]::PostMessage($w.H, 0x0312, [IntPtr]114, [IntPtr]::Zero) | Out-Null; Write-Host "esc posted" } }
  }
  "explorers" {
    $sh = New-Object -ComObject Shell.Application
    $sh.Windows() | ForEach-Object { Write-Host ("EXPL h={0} {1}" -f $_.HWND, $_.LocationURL) }
  }
  "snap" {
    foreach ($w in (Get-SnapWins)) { Write-Host ("{0} vis={1}" -f $w.Class, $w.Visible) }
  }
}