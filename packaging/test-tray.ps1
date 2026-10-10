param([string]$Executable)
# Run on an interactive Windows desktop. Exercises actual WM_COMMAND tray selections.
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
if (-not $Executable) { $Executable = Join-Path $projectRoot 'target\release\win-thermalright-ai-monitor.exe' }
$Executable = (Resolve-Path -LiteralPath $Executable).Path
$driver = Join-Path $projectRoot 'tests\windows-tray-driver.ps1'
$testRoot = Join-Path $projectRoot ('target\tray-regression\{0}' -f [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-fff'))
New-Item -ItemType Directory -Force -Path $testRoot | Out-Null
$configPath = Join-Path $testRoot 'settings.json'
[IO.File]::WriteAllText($configPath, '{}', [Text.UTF8Encoding]::new($false))

function Invoke-TrayAction([string]$action) {
    $windows = & $driver -MonitorPid $monitor.Id -Action $action | ConvertFrom-Json
    return $windows | Where-Object Title -eq 'win-thermalright-ai-monitor' | Select-Object -First 1
}
function Assert-Window([string]$action, [bool]$visible, [bool]$minimized = $false) {
    $window = Invoke-TrayAction $action
    if (($visible -and (-not $window -or -not $window.Visible -or $window.Minimized -ne $minimized)) -or
        (-not $visible -and $window)) {
        throw "Tray regression failed after $action : expected visible=$visible, minimized=$minimized."
    }
    Write-Host "$action : visible=$visible, minimized=$minimized, preview destroyed when hidden"
    return $window
}
function Wait-InitialPreview {
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    do {
        $monitor.Refresh()
        if ($monitor.HasExited) { throw "Monitor exited during startup: $(Get-Content -LiteralPath $stderr -Raw)" }
        $window = Invoke-TrayAction 'Inspect'
        if ($window -and $window.Visible -and -not $window.Minimized) { return $window }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'Preview window was not created.'
}
function Start-Monitor([string]$prefix) {
    $script:stderr = Join-Path $testRoot "$prefix-stderr.log"
    Start-Process -FilePath $Executable -ArgumentList @('--demo', '--preview', '--config', ('"{0}"' -f $configPath)) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $testRoot "$prefix-stdout.log") -RedirectStandardError $stderr
}

$monitor = Start-Monitor 'hidden-quit'
try {
    $null = Wait-InitialPreview

    $null = Assert-Window 'Close' $false
    $null = Assert-Window 'Preview' $true
    $null = Assert-Window 'Close' $false
    $null = Assert-Window 'Settings' $true
    $null = & $driver -MonitorPid $monitor.Id -Action Inspect -Capture (Join-Path $testRoot 'settings.png')
    $null = Assert-Window 'Minimize' $true $true
    $null = Assert-Window 'Preview' $true
    for ($cycle = 1; $cycle -le 3; $cycle++) {
        $before = Invoke-TrayAction 'Inspect'
        $after = Assert-Window 'CloseThenSettings' $true
        if ($after.Pid -eq $before.Pid) { throw 'Rapid Settings request did not replace the closing preview.' }
        $null = & $driver -MonitorPid $monitor.Id -Action Inspect -Capture (Join-Path $testRoot "rapid-settings-$cycle.png")
    }
    $window = Invoke-TrayAction 'Inspect'
    $previewPid = $window.Pid
    if ($previewPid -eq $monitor.Id) { throw 'Preview did not run in a separate process.' }
    Stop-Process -Id $previewPid
    $crashed = Get-Process -Id $previewPid -ErrorAction SilentlyContinue
    if ($crashed -and -not $crashed.WaitForExit(5000)) { throw 'Preview termination timed out.' }
    $monitor.Refresh()
    if ($monitor.HasExited) { throw 'Preview crash stopped the background runtime.' }
    $null = Assert-Window 'Preview' $true
    $null = Assert-Window 'Close' $false
    $null = Invoke-TrayAction 'Quit'
    if (-not $monitor.WaitForExit(5000)) { throw 'Quit from hidden tray did not exit.' }
    if ($monitor.ExitCode -ne 0) { throw "Monitor exited with code $($monitor.ExitCode)." }
    # Also exercise Quit while the preview is still open, including child cleanup.
    $monitor = Start-Monitor 'visible-quit'
    $window = Wait-InitialPreview
    $previewPid = $window.Pid
    $null = Invoke-TrayAction 'Quit'
    if (-not $monitor.WaitForExit(5000)) { throw 'Quit with a visible preview did not exit.' }
    if ($monitor.ExitCode -ne 0) { throw "Monitor exited with code $($monitor.ExitCode)." }
    if (Get-Process -Id $previewPid -ErrorAction SilentlyContinue) { throw 'Quit left an orphan preview process.' }
    # A frozen client cannot read the graceful shutdown packet. The parent must
    # enforce its timeout, terminate it, and unblock both pipe worker threads.
    $monitor = Start-Monitor 'frozen-client'
    $window = Wait-InitialPreview
    $previewPid = $window.Pid
    $null = Invoke-TrayAction 'Suspend'
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $null = Invoke-TrayAction 'Quit'
    if (-not $monitor.WaitForExit(5000)) { throw 'Quit blocked on an unresponsive preview client.' }
    $timer.Stop()
    if ($timer.Elapsed.TotalSeconds -gt 5) { throw 'Unresponsive preview shutdown exceeded five seconds.' }
    if ($monitor.ExitCode -ne 0) { throw "Monitor exited with code $($monitor.ExitCode)." }
    if (Get-Process -Id $previewPid -ErrorAction SilentlyContinue) { throw 'Timeout fallback left an orphan preview process.' }
    Write-Host ('Unresponsive client: parent exited in {0:N2}s with no orphan.' -f $timer.Elapsed.TotalSeconds)
    $null = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
    if (@(Get-ChildItem -LiteralPath $testRoot -Filter 'settings.json.*.tmp').Count) { throw 'Configuration save left temporary files behind.' }
    Write-Output "Windows tray regression passed. Inspect Settings screenshots in $testRoot; GUI content is a visual check."
} finally {
    $monitor.Refresh()
    if (-not $monitor.HasExited) {
        $children = @([MonitorWindows]::List($monitor.Id) | Where-Object { $_.Pid -ne $monitor.Id } | Select-Object -ExpandProperty Pid -Unique)
        try { $null = Invoke-TrayAction 'Quit' } catch { Write-Warning "Tray cleanup failed: $_" }
        if (-not $monitor.WaitForExit(10000)) { Stop-Process -Id $monitor.Id }
        foreach ($childPid in $children) {
            if (Get-Process -Id $childPid -ErrorAction SilentlyContinue) { Stop-Process -Id $childPid -ErrorAction SilentlyContinue }
        }
    }
}
