# Server-side install of the hosted read-only asset-library MCP service (scheduled task HFUeAssetMcp).
# Run on scott-server (through the WSL ssh route or locally); idempotent, safe to re-run.
# Cloned from the sanitizer install.ps1 template (F:\ue_assets_library\_deploy\install.ps1).
# Source files live in F:\ue_assets_library\_deploy\mcp\ (a SUBDIRECTORY: the sanitizer script
# copies every exe/dll of _deploy itself, so this service must not sit next to it).
# The service is read-only: it never gets --write-overlays and never writes into the library share.
# The bearer token is created once in token.txt (ACL: scott only); its value is never printed or logged.
param([switch]$NoTask, [switch]$NoCopy)   # -NoTask: copy files and token only, do not register or start the task
# -NoCopy: re-register the task only (leave the installed exes alone, e.g. while the enrich task runs)
$ErrorActionPreference = 'Stop'
$src = 'F:\ue_assets_library\_deploy\mcp'
$dst = 'C:\HF\ue_asset_mcp'
$bundles = Join-Path $dst 'bundles'
$tokenFile = Join-Path $dst 'token.txt'
$bind = '192.168.12.201:8433'
New-Item -ItemType Directory -Force $dst | Out-Null
New-Item -ItemType Directory -Force $bundles | Out-Null

if (-not $NoCopy) {
    Get-Process ue_asset_mcp -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep 1
    Copy-Item "$src\ue_asset_mcp.exe", "$src\ue_asset_enrich.exe", "$src\libwinpthread-1.dll" $dst -Force
}

if (-not (Test-Path -LiteralPath $tokenFile)) {
    $bytes = New-Object byte[] 32
    $rng = New-Object System.Security.Cryptography.RNGCryptoServiceProvider
    $rng.GetBytes($bytes)
    $rng.Dispose()
    $hex = -join ($bytes | ForEach-Object { $_.ToString('x2') })
    [System.IO.File]::WriteAllText($tokenFile, $hex, (New-Object System.Text.ASCIIEncoding))
    $me = "$env:COMPUTERNAME\$env:USERNAME"
    & icacls.exe $tokenFile /inheritance:r /grant:r "${me}:(F)" | Out-Null
    Write-Output 'token file created (value not shown)'
} else {
    Write-Output 'token file kept'
}

if ($NoTask) { Write-Output 'task registration skipped'; return }

$args2 = "--http $bind --token-file `"$tokenFile`" --read-only --library F:/ue_assets_library/library --cache `"$dst\catalog.json`" --bundle-dir `"$bundles`" --no-generated --no-install-log"
# The service logs to stderr only; cmd appends it to logs\service.log (rotated here when over 5 MB).
# One line per reload at most every 5 minutes, so the file grows slowly.
$logDir = Join-Path $dst 'logs'
$logFile = Join-Path $logDir 'service.log'
New-Item -ItemType Directory -Force $logDir | Out-Null
if ((Test-Path -LiteralPath $logFile) -and ((Get-Item -LiteralPath $logFile).Length -ge 5MB)) {
    Move-Item -LiteralPath $logFile -Destination "$logFile.1" -Force
}
$cmdLine = "/c `"`"$dst\ue_asset_mcp.exe`" $args2 >> `"$logFile`" 2>&1`""
$a = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\cmd.exe" -Argument $cmdLine -WorkingDirectory $dst
$t = New-ScheduledTaskTrigger -AtLogOn -User "$env:COMPUTERNAME\$env:USERNAME"
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -RestartCount 5 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName 'HFUeAssetMcp' -Action $a -Trigger $t -Settings $s -Force | Out-Null
Start-ScheduledTask -TaskName 'HFUeAssetMcp'
Start-Sleep 3
Get-ScheduledTask -TaskName 'HFUeAssetMcp' | Get-ScheduledTaskInfo | Select LastRunTime,LastTaskResult | Format-List
Get-Process ue_asset_mcp -ErrorAction SilentlyContinue | Select Id,Path | Format-List
