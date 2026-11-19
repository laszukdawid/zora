<#
.SYNOPSIS
  Installs Zora for the current Windows user.

.DESCRIPTION
  Builds Zora (or takes binaries from -From), copies them to %LOCALAPPDATA%\Zora\bin, writes a starting config to
  %APPDATA%\Zora\config.json when there is none (an existing one is never touched), downloads the speech model,
  and registers a scheduled task "Zora" that starts zorad at logon. The tray's "Restart Zora" runs that task.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File packaging\windows\install.ps1
  powershell -ExecutionPolicy Bypass -File packaging\windows\install.ps1 -From C:\Downloads\zora -NoModel
#>
param(
    [string]$From = "",   # a folder with built binaries; default: build them with cargo
    [switch]$NoModel,     # skip `zora-listen setup`
    [switch]$NoTask       # don't register the logon task
)
$ErrorActionPreference = "Stop"

$bins = "zorad.exe", "zora-listen.exe", "zora-win-dictation.exe", "zora-tray.exe"
$dest = Join-Path $env:LOCALAPPDATA "Zora\bin"

if (-not $From) {
    $repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
    Push-Location $repo
    try {
        cargo build --release -p zorad -p zora-listen -p zora-windows
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    } finally {
        Pop-Location
    }
    $From = Join-Path $repo "target\release"
}

New-Item -ItemType Directory -Force -Path $dest | Out-Null
foreach ($b in $bins) {
    Copy-Item -Force -Path (Join-Path $From $b) -Destination $dest
}
# Runtime libraries the speech plugin may have been linked against (onnxruntime, sherpa-onnx).
Get-ChildItem -Path $From -Filter *.dll -ErrorAction SilentlyContinue | Copy-Item -Force -Destination $dest
Write-Host "installed into $dest"

$config = Join-Path $env:APPDATA "Zora\config.json"
if (Test-Path $config) {
    Write-Host "kept the existing config $config"
} else {
    New-Item -ItemType Directory -Force -Path (Split-Path $config) | Out-Null
    $c = [ordered]@{
        wakeNames      = @("zora", "nova")
        dictation      = [ordered]@{ windowsApps = @(); monitor = "plugin" }
        defaultHarness = "claude"
        harnesses      = [ordered]@{
            claude = [ordered]@{ type = "claude"; workspace = ""; permissionMode = "acceptEdits" }
        }
        plugins        = [ordered]@{
            listen    = @{ command = @(Join-Path $dest "zora-listen.exe") }
            dictation = @{ command = @(Join-Path $dest "zora-win-dictation.exe") }
            desktop   = @{ command = @(Join-Path $dest "zora-tray.exe") }
        }
    }
    # UTF-8 without a BOM: zorad's JSON parser rejects one.
    [System.IO.File]::WriteAllText($config, ($c | ConvertTo-Json -Depth 6), (New-Object System.Text.UTF8Encoding $false))
    Write-Host "wrote $config"
}

if (-not $NoModel) {
    & (Join-Path $dest "zora-listen.exe") setup
    if ($LASTEXITCODE -ne 0) { throw "zora-listen setup failed" }
}

if (-not $NoTask) {
    $user = "$env:USERDOMAIN\$env:USERNAME"
    # conhost --headless runs the console program without a window.
    $action = New-ScheduledTaskAction -Execute "conhost.exe" -Argument "--headless `"$(Join-Path $dest 'zorad.exe')`" run"
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    # "Stop the existing instance" (3), so running the task again restarts Zora; the cmdlet has no switch for it.
    $settings.CimInstanceProperties.Item("MultipleInstances").Value = 3
    $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
    Register-ScheduledTask -TaskName "Zora" -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null
    Write-Host "registered the scheduled task Zora (starts zorad at logon)"
}

Write-Host ""
Write-Host "Next:"
Write-Host "  1. Set the project folder:  & `"$dest\zorad.exe`" settings set '{`"workspace`": `"C:\\path\\to\\repo`"}'"
Write-Host "  2. Name your dictation tool: run `"$dest\zora-win-dictation.exe`" watch, dictate, and add the program"
Write-Host "     name to dictation.windowsApps in $config (or set dictation.monitor to `"none`")."
Write-Host "  3. Start Zora now:           Start-ScheduledTask -TaskName Zora"
