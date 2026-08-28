$ErrorActionPreference = "Continue"

# 1) Try FindWindowSW via COM with a URL string
$sw = $null
try {
  $sw = New-Object -ComObject ShellWindows
  Write-Host "ShellWindows created: $($sw | Get-Member -Name FindWindowSW -ErrorAction SilentlyContinue | Out-String)"
} catch {
  Write-Host "ShellWindows create failed: $_"
}

if ($sw) {
  $targets = @(
    "file:///C:/Users/Izzy/Pictures/SnapDrop",
    "file://C:/Users/Izzy/Pictures/SnapDrop",
    "C:/Users/Izzy/Pictures/SnapDrop"
  )
  foreach ($t in $targets) {
    $hwnd = 0
    try {
      $b = $sw.FindWindowSW($t, $null, 1, [ref]$hwnd, 1)
      Write-Host ("FindWindowSW '{0}' -> hwnd={1} obj={2}" -f $t, $hwnd, ($b -ne $null))
    } catch {
      Write-Host ("FindWindowSW '{0}' -> ERROR {1}" -f $t, $_.Exception.Message)
    }
  }
}

# 2) Enumerate approach: LocationURL compare
Write-Host "--- enumeration ---"
$sh = New-Object -ComObject Shell.Application
$target = "file:///C:/Users/Izzy/Pictures/SnapDrop"
$sh.Windows() | ForEach-Object {
  $url = $_.LocationURL
  $match = if ($url -eq $target) { "MATCH" } else { "" }
  Write-Host ("{0} hwnd={1} {2}" -f $url, $_.HWND, $match)
}

# 3) LocationURL casing variants
Write-Host "--- casing variants ---"
$sh.Windows() | Where-Object { $_.LocationURL -like "*snapdrop*" } | ForEach-Object {
  Write-Host ("EXACT: {0}" -f ($_.LocationURL -eq "file:///C:/Users/Izzy/Pictures/SnapDrop"))
  Write-Host ("INSENSITIVE: {0}" -f ($_.LocationURL.ToLower() -eq "file:///C:/users/izzy/pictures/snapdrop"))
}