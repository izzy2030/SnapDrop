$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class ShellInvoke {
  [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
  public static extern int SHParseDisplayName(string pszName, IntPtr pbc, out IntPtr ppidl, uint sfgaoIn, out uint psfgaoOut);
  [DllImport("shell32.dll")]
  public static extern int SHOpenFolderAndSelectItems(IntPtr pidlFolder, uint cidl, IntPtr[] apidl, uint dwFlags);
  [DllImport("ole32.dll")]
  public static extern void CoTaskMemFree(IntPtr pv);
}
"@

function Dump-Windows {
  $sw = New-Object -ComObject Shell.Application
  $i = 0
  foreach ($w in $sw.Windows()) {
    try {
      Write-Output ("  [{0}] Name='{1}' URL='{2}'" -f $i, $w.LocationName, $w.LocationURL)
    } catch {
      Write-Output "  [$i] <error>"
    }
    $i++
  }
  Write-Output "  total: $i"
}

$folder = 'C:\Users\Izzy\Pictures\SnapDrop'
$file = Join-Path $folder (Get-ChildItem $folder -File | Select-Object -First 1).Name
Write-Output "target file: $file"

Write-Output '== windows before =='
Dump-Windows

$pidlFolder = [IntPtr]::Zero
$sfgao = [uint32]0
[ShellInvoke]::SHParseDisplayName($folder, [IntPtr]::Zero, [ref]$pidlFolder, [uint32]0, [ref]$sfgao) | Out-Null

$pidlFile = [IntPtr]::Zero
[ShellInvoke]::SHParseDisplayName($file, [IntPtr]::Zero, [ref]$pidlFile, [uint32]0, [ref]$sfgao) | Out-Null
Write-Output "folder pidl=$pidlFolder file pidl=$pidlFile"

# Variant A: folder only
[ShellInvoke]::SHOpenFolderAndSelectItems($pidlFolder, 0, $null, 0) | Out-Null
Start-Sleep -Milliseconds 1200
Write-Output '== after folder-only open =='
Dump-Windows

# Variant B: select the file
[ShellInvoke]::SHOpenFolderAndSelectItems($pidlFolder, 1, @($pidlFile), 0) | Out-Null
Start-Sleep -Milliseconds 1200
Write-Output '== after select-file open =='
Dump-Windows

# Variant C: select the same file AGAIN — reuse test
[ShellInvoke]::SHOpenFolderAndSelectItems($pidlFolder, 1, @($pidlFile), 0) | Out-Null
Start-Sleep -Milliseconds 1200
Write-Output '== after 2nd select-file open (reuse?) =='
Dump-Windows

[ShellInvoke]::CoTaskMemFree($pidlFolder)
[ShellInvoke]::CoTaskMemFree($pidlFile)