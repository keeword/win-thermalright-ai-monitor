param([string]$Executable)
# Run on an interactive Windows desktop. Exercises actual WM_COMMAND tray selections.
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
if (-not $Executable) { $Executable = Join-Path $projectRoot 'target\release\win-thermalright-ai-monitor.exe' }
$Executable = (Resolve-Path -LiteralPath $Executable).Path
$driver = Join-Path $projectRoot 'tests\windows-tray-driver.ps1'
$testRoot = Join-Path $projectRoot 'target\tray-regression'
New-Item -ItemType Directory -Force -Path $testRoot | Out-Null
$configPath = Join-Path $testRoot 'settings.json'
Set-Content -LiteralPath $configPath -Value '{}' -Encoding utf8

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
    Write-Output "$action : visible=$visible, minimized=$minimized, preview destroyed when hidden"
}

$monitor = Start-Process -FilePath $Executable -ArgumentList @('--demo', '--preview', '--config', ('"{0}"' -f $configPath)) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $testRoot 'stdout.log') -RedirectStandardError (Join-Path $testRoot 'stderr.log')
try {
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    do {
        if ($monitor.HasExited) { throw "Monitor exited during startup: $(Get-Content -LiteralPath (Join-Path $testRoot 'stderr.log') -Raw)" }
        $window = Invoke-TrayAction 'Inspect'
        if ($window) { break }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $deadline)
    if (-not $window) { throw 'Preview window was not created.' }

    Assert-Window 'Close' $false
    Assert-Window 'Preview' $true
    Assert-Window 'Close' $false
    Assert-Window 'Settings' $true
    $null = & $driver -MonitorPid $monitor.Id -Action Inspect -Capture (Join-Path $testRoot 'settings.png')
    Assert-Window 'Minimize' $true $true
    Assert-Window 'Preview' $true
    $window = Invoke-TrayAction 'Inspect'
    $previewPid = $window.Pid
    if ($previewPid -eq $monitor.Id) { throw 'Preview did not run in a separate process.' }
    Stop-Process -Id $previewPid
    Start-Sleep -Milliseconds 500
    if ($monitor.HasExited) { throw 'Preview crash stopped the background runtime.' }
    Assert-Window 'Preview' $true
    Assert-Window 'Close' $false
    $null = Invoke-TrayAction 'Quit'
    if (-not $monitor.WaitForExit(5000)) { throw 'Quit from hidden tray did not exit.' }
    if ($monitor.ExitCode -ne 0) { throw "Monitor exited with code $($monitor.ExitCode)." }
    # Also exercise Quit while the preview is still open, including child cleanup.
    $monitor = Start-Process -FilePath $Executable -ArgumentList @('--demo', '--preview', '--config', ('"{0}"' -f $configPath)) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $testRoot 'visible-stdout.log') -RedirectStandardError (Join-Path $testRoot 'visible-stderr.log')
    Start-Sleep -Seconds 2
    $window = Invoke-TrayAction 'Inspect'
    if (-not $window) { throw 'Second preview was not created.' }
    $previewPid = $window.Pid
    $null = Invoke-TrayAction 'Quit'
    if (-not $monitor.WaitForExit(5000)) { throw 'Quit with a visible preview did not exit.' }
    if (Get-Process -Id $previewPid -ErrorAction SilentlyContinue) { throw 'Quit left an orphan preview process.' }
    Write-Output 'Windows tray regression passed; inspect target/tray-regression/settings.png for the settings panel.'
} finally {
    if (-not $monitor.HasExited) {
        $null = Invoke-TrayAction 'Quit'
        if (-not $monitor.WaitForExit(10000)) { Stop-Process -Id $monitor.Id }
    }
}
