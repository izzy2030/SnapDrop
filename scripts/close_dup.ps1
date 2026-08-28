param([string]$Url = "file:///C:/Users/Izzy/Pictures/SnapDrop")
$sh = New-Object -ComObject Shell.Application
$kept = $false
$sh.Windows() | ForEach-Object {
  if ($_.LocationURL -eq $Url) {
    if ($kept) {
      Write-Host ("closing dup {0}" -f $_.HWND)
      $_.Quit()
    } else {
      $kept = $true
      Write-Host ("keeping {0}" -f $_.HWND)
    }
  }
}
Start-Sleep -Milliseconds 500