<#
.SYNOPSIS
  Installs Zora for the current Windows user.

.DESCRIPTION
  Builds Zora (or takes binaries from -From), copies them to %LOCALAPPDATA%\Zora\bin, writes a starting config to
  %APPDATA%\Zora\config.json when there is none (an existing one is never touched), downloads the speech model,
  and registers a scheduled task \Zora\Zora that starts zorad at logon. The tray's "Restart Zora" runs that task.

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
    Copy-Item -Force -LiteralPath (Join-Path $From $b) -Destination $dest
}
# Runtime libraries the speech plugin may have been linked against (onnxruntime, sherpa-onnx).
Get-ChildItem -LiteralPath $From -Filter *.dll -ErrorAction SilentlyContinue | Copy-Item -Force -Destination $dest
Write-Host "installed into $dest"

$config = Join-Path $env:APPDATA "Zora\config.json"
if (Test-Path -LiteralPath $config) {
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
    # UTF-8 without a BOM (zorad's JSON parser rejects one), written beside the config and moved into place
    # without replacing anything: a config that appeared meanwhile is kept.
    $tmp = "$config.$PID.tmp"
    [System.IO.File]::WriteAllText($tmp, ($c | ConvertTo-Json -Depth 6), (New-Object System.Text.UTF8Encoding $false))
    try {
        [System.IO.File]::Move($tmp, $config)
        Write-Host "wrote $config"
    } catch [System.IO.IOException] {
        Remove-Item -LiteralPath $tmp -ErrorAction SilentlyContinue
        if (-not (Test-Path -LiteralPath $config)) { throw }
        Write-Host "kept the existing config $config"
    }
}

if (-not $NoModel) {
    & (Join-Path $dest "zora-listen.exe") setup
    if ($LASTEXITCODE -ne 0) { throw "zora-listen setup failed" }
}

if (-not $NoTask) {
    $user = "$env:USERDOMAIN\$env:USERNAME"
    # conhost --headless runs the console program without a window.
    $conhost = Join-Path $env:SystemRoot "System32\conhost.exe"
    $arguments = "--headless `"$(Join-Path $dest 'zorad.exe')`" run"
    # Only ever replace a task this script made for this install.
    $existing = Get-ScheduledTask -TaskPath "\Zora\" -TaskName "Zora" -ErrorAction SilentlyContinue
    if ($existing) {
        $a = @($existing.Actions)
        if ($a.Count -ne 1 -or $a[0].Execute -ne $conhost -or $a[0].Arguments -ne $arguments) {
            throw "a scheduled task \Zora\Zora exists that doesn't start this zorad; remove it (Unregister-ScheduledTask -TaskPath '\Zora\' -TaskName Zora) and run this again"
        }
    }
    $action = New-ScheduledTaskAction -Execute $conhost -Argument $arguments
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    # "Stop the existing instance" (3), so running the task again restarts Zora; the cmdlet has no switch for it.
    $settings.CimInstanceProperties.Item("MultipleInstances").Value = 3
    $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
    Register-ScheduledTask -TaskPath "\Zora\" -TaskName "Zora" -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null
    Write-Host "registered the scheduled task \Zora\Zora (starts zorad at logon)"
}

Write-Host ""
Write-Host "Next:"
Write-Host "  1. Set the project folder:  & `"$dest\zorad.exe`" settings set '{`"workspace`": `"C:\\path\\to\\repo`"}'"
Write-Host "  2. Name your dictation tool: run `"$dest\zora-win-dictation.exe`" watch, dictate, and add the program"
Write-Host "     name to dictation.windowsApps in $config (or set dictation.monitor to `"none`")."
Write-Host "  3. Start Zora now:           Start-ScheduledTask -TaskPath '\Zora\' -TaskName Zora"
