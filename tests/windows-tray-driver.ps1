param(
  [int]$MonitorPid,
  [ValidateSet('Inspect','Preview','Settings','Close','CloseThenSettings','Quit','Minimize','Suspend')][string]$Action='Inspect',
  [string]$Capture,
  [switch]$NoWait,
  [int]$TimeoutSeconds=15
)
$ErrorActionPreference='Stop'
# Native command IDs for the pinned muda 0.17 dependency: menu=1000,
# followed by Preview=1001, Settings=1002, Quit=1003.
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class MonitorWindows {
  [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct ProcessEntry {
    public uint Size,Usage,Pid; public UIntPtr Heap; public uint Module,Threads,Parent;
    public int Priority; public uint Flags;
    [MarshalAs(UnmanagedType.ByValTStr,SizeConst=260)] public string Exe;
  }
  [DllImport("kernel32.dll")] static extern IntPtr CreateToolhelp32Snapshot(uint flags,uint pid);
  [DllImport("kernel32.dll",CharSet=CharSet.Unicode)] static extern bool Process32FirstW(IntPtr snapshot,ref ProcessEntry entry);
  [DllImport("kernel32.dll",CharSet=CharSet.Unicode)] static extern bool Process32NextW(IntPtr snapshot,ref ProcessEntry entry);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
  [DllImport("kernel32.dll",SetLastError=true)] static extern IntPtr OpenProcess(uint access,bool inherit,uint pid);
  [DllImport("ntdll.dll")] static extern int NtSuspendProcess(IntPtr process);
  public static void Suspend(uint pid) {
    var process=OpenProcess(0x0800,false,pid);
    if(process==IntPtr.Zero) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
    try {
      int status=NtSuspendProcess(process);
      if(status!=0) throw new InvalidOperationException("NtSuspendProcess returned "+status);
    } finally {CloseHandle(process);}
  }
  static HashSet<uint> Family(int targetPid) {
    var family=new HashSet<uint> {(uint)targetPid};
    var snapshot=CreateToolhelp32Snapshot(2,0);
    try {
      var entry=new ProcessEntry {Size=(uint)Marshal.SizeOf(typeof(ProcessEntry))};
      if(Process32FirstW(snapshot,ref entry)) do {
        if(entry.Parent==(uint)targetPid) family.Add(entry.Pid);
      } while(Process32NextW(snapshot,ref entry));
    } finally {CloseHandle(snapshot);}
    return family;
  }
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
  public class Info { public IntPtr Handle; public uint Pid; public string Title; public string Class; public bool Visible; public bool Minimized; public bool Foreground; }
  public static Info[] List(int targetPid) {
    var result=new List<Info>();
    var family=Family(targetPid);
    EnumWindows((window,param)=> {
      uint owner; GetWindowThreadProcessId(window,out owner);
      if(family.Contains(owner)) {
        var title=new StringBuilder(256);var name=new StringBuilder(256);
        GetWindowText(window,title,256);GetClassName(window,name,256);
        result.Add(new Info {Handle=window,Pid=owner,Title=title.ToString(),Class=name.ToString(),Visible=IsWindowVisible(window),Minimized=IsIconic(window),Foreground=GetForegroundWindow()==window});
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
  function Send-WindowMessage($window, [uint32]$message, [uint64]$command) {
    if (-not $window) { throw "No target window for $Action (monitor PID $MonitorPid)." }
    [UIntPtr]$result=[UIntPtr]::Zero
    if ([MonitorWindows]::SendMessageTimeout($window.Handle,$message,[UIntPtr]::new($command),[IntPtr]::Zero,2,2000,[ref]$result) -eq [IntPtr]::Zero) {
      throw "Window did not accept $Action within the message timeout."
    }
  }
  switch ($Action) {
    'Preview' { Send-WindowMessage $trayWindow 0x111 1001 }
    'Settings' { Send-WindowMessage $trayWindow 0x111 1002 }
    'Quit' { Send-WindowMessage $trayWindow 0x111 1003 }
    'Close' { Send-WindowMessage $mainWindow 0x10 0 }
    'CloseThenSettings' {
      # Deliberately avoid waiting for the closing child before the tray request.
      Send-WindowMessage $mainWindow 0x10 0
      Send-WindowMessage $trayWindow 0x111 1002
    }
    'Minimize' { [void][MonitorWindows]::ShowWindow($mainWindow.Handle,6) }
    'Suspend' {
      if (-not $mainWindow -or $mainWindow.Pid -eq $MonitorPid) { throw 'Suspend requires a separate owned preview process.' }
      [MonitorWindows]::Suspend($mainWindow.Pid)
    }
  }
  if (-not $NoWait) {
    $deadline=[DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
      $current=[MonitorWindows]::List($MonitorPid)
      $preview=$current | Where-Object Title -eq 'win-thermalright-ai-monitor' | Select-Object -First 1
      $ready=switch ($Action) {
        'Close' { -not $preview }
        'Quit' { -not $current }
        'Minimize' { $preview -and $preview.Minimized }
        'Suspend' { $preview }
        'CloseThenSettings' { $preview -and $preview.Pid -ne $mainWindow.Pid -and $preview.Visible -and -not $preview.Minimized }
        default { $preview -and $preview.Visible -and -not $preview.Minimized }
      }
      if ($ready) { break }
      Start-Sleep -Milliseconds 50
    } while ([DateTime]::UtcNow -lt $deadline)
    if (-not $ready) { throw "Timed out waiting for $Action to complete (monitor PID $MonitorPid)." }
  }
}
[MonitorWindows]::List($MonitorPid) | Select-Object Handle,Pid,Title,Class,Visible,Minimized,Foreground | ConvertTo-Json
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
