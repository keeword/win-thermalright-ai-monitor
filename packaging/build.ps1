param([switch]$SkipBuild, [string]$ExecutablePath)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
Push-Location -LiteralPath $projectRoot
try {
    if (-not $SkipBuild) {
        cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw 'Rust release build failed.' }
    }
    $packagePath = Join-Path $projectRoot 'dist\win-thermalright-ai-monitor'
    New-Item -ItemType Directory -Force -Path $packagePath | Out-Null
    $expectedPackagePath = [IO.Path]::GetFullPath($packagePath)
    $packagePath = (Resolve-Path -LiteralPath $packagePath).Path
    if ($packagePath -ne $expectedPackagePath -or
        ((Get-Item -LiteralPath $packagePath).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Unexpected package path; packaging aborted.'
    }
    $releaseFiles = @('win-thermalright-ai-monitor.exe', 'README.md', 'LICENSE', 'THIRD_PARTY.md')
    $releaseExecutable = if ($ExecutablePath) {
        [IO.Path]::GetFullPath($ExecutablePath)
    } else {
        Join-Path $projectRoot 'target\release\win-thermalright-ai-monitor.exe'
    }
    Copy-Item -LiteralPath $releaseExecutable -Destination (Join-Path $packagePath 'win-thermalright-ai-monitor.exe')
    foreach ($fileName in @('README.md', 'LICENSE', 'THIRD_PARTY.md')) {
        Copy-Item -LiteralPath (Join-Path $projectRoot $fileName) -Destination $packagePath
    }
    foreach ($item in Get-ChildItem -LiteralPath $packagePath -Force) {
        if ($item.Name -notin $releaseFiles) {
            $cleanupPath = [IO.Path]::GetFullPath($item.FullName)
            if (-not $cleanupPath.StartsWith($packagePath + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
                throw 'Cleanup target is outside the package; packaging aborted.'
            }
            Remove-Item -LiteralPath $cleanupPath -Recurse -Force
        }
    }
    Write-Output "Package ready: $packagePath"
} finally {
    Pop-Location
}
