param([int]$MonitorPid, [ValidateSet('Inspect','Preview','Settings','Close','Quit','Minimize')][string]$Action='Inspect', [string]$Capture)
$ErrorActionPreference='Stop'
# Native command IDs for the pinned muda 0.17 dependency: menu=1000,
# followed by Preview=1001, Settings=1002, Quit=1003.
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class MonitorWindows {
  public delegate bool EnumCallback(IntPtr window, IntPtr param);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumCallback callback, IntPtr param);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetWindowText(IntPtr window, StringBuilder text, int size);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetClassName(IntPtr window, StringBuilder text, int size);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr window);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr window);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr window, int action);
  [StructLayout(LayoutKind.Sequential)] public struct Bounds {public int Left,Top,Right,Bottom;}
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr window,out Bounds bounds);
  [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr window, IntPtr dc, uint flags);
  [DllImport("user32.dll", SetLastError=true)] public static extern IntPtr SendMessageTimeout(IntPtr window, uint message, UIntPtr wparam, IntPtr lparam, uint flags, uint timeout, out UIntPtr result);
  public class Info { public IntPtr Handle; public string Title; public string Class; public bool Visible; public bool Minimized; public bool Foreground; }
  public static Info[] List(int targetPid) {
    var result=new List<Info>();
    EnumWindows((window,param)=> {
      uint owner; GetWindowThreadProcessId(window,out owner);
      if(owner==(uint)targetPid) {
        var title=new StringBuilder(256);var name=new StringBuilder(256);
        GetWindowText(window,title,256);GetClassName(window,name,256);
        result.Add(new Info {Handle=window,Title=title.ToString(),Class=name.ToString(),Visible=IsWindowVisible(window),Minimized=IsIconic(window),Foreground=GetForegroundWindow()==window});
      }
      return true;
    },IntPtr.Zero);
    return result.ToArray();
  }
}
'@
$windows=[MonitorWindows]::List($MonitorPid)
if ($Action -ne 'Inspect') {
  $trayWindow=$windows | Where-Object Class -eq 'tray_icon_app' | Select-Object -First 1
  $mainWindow=$windows | Where-Object Title -eq 'win-thermalright-ai-monitor' | Select-Object -First 1
  [UIntPtr]$messageResult=[UIntPtr]::Zero
  switch ($Action) {
    'Preview' { [void][MonitorWindows]::SendMessageTimeout($trayWindow.Handle,0x111,[UIntPtr]::new([uint64]1001),[IntPtr]::Zero,2,2000,[ref]$messageResult) }
    'Settings' { [void][MonitorWindows]::SendMessageTimeout($trayWindow.Handle,0x111,[UIntPtr]::new([uint64]1002),[IntPtr]::Zero,2,2000,[ref]$messageResult) }
    'Quit' { [void][MonitorWindows]::SendMessageTimeout($trayWindow.Handle,0x111,[UIntPtr]::new([uint64]1003),[IntPtr]::Zero,2,2000,[ref]$messageResult) }
    'Close' { [void][MonitorWindows]::SendMessageTimeout($mainWindow.Handle,0x10,[UIntPtr]::Zero,[IntPtr]::Zero,2,2000,[ref]$messageResult) }
    'Minimize' { [void][MonitorWindows]::ShowWindow($mainWindow.Handle,6) }
  }
  Start-Sleep -Milliseconds 1500
}
[MonitorWindows]::List($MonitorPid) | Select-Object Handle,Title,Class,Visible,Minimized,Foreground | ConvertTo-Json
if ($Capture) {
  Add-Type -AssemblyName System.Drawing
  $mainWindow=[MonitorWindows]::List($MonitorPid) | Where-Object Title -eq 'win-thermalright-ai-monitor' | Select-Object -First 1
  $oldDpi=[MonitorWindows]::SetThreadDpiAwarenessContext([IntPtr](-4))
  $bounds=New-Object MonitorWindows+Bounds
  [void][MonitorWindows]::GetWindowRect($mainWindow.Handle,[ref]$bounds)
  $bitmap=New-Object Drawing.Bitmap ($bounds.Right-$bounds.Left),($bounds.Bottom-$bounds.Top)
  $graphics=[Drawing.Graphics]::FromImage($bitmap)
  try {
    $dc=$graphics.GetHdc()
    try { if (-not [MonitorWindows]::PrintWindow($mainWindow.Handle,$dc,2)) {throw 'PrintWindow failed.'} }
    finally {$graphics.ReleaseHdc($dc)}
    $bitmap.Save([IO.Path]::GetFullPath($Capture, (Get-Location).Path),[Drawing.Imaging.ImageFormat]::Png)
  } finally {$graphics.Dispose();$bitmap.Dispose();[void][MonitorWindows]::SetThreadDpiAwarenessContext($oldDpi)}
}
