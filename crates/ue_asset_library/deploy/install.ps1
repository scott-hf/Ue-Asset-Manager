# Server-side install of the ue_asset_sanitizer service (scheduled task HFUeAssetSanitizer).
# Run on scott-server (through ssh or locally); idempotent, safe to re-run.
# Source of truth: this file; the live copy is F:\ue_assets_library\_deploy\install.ps1 next to
# ue_asset_sanitizer.exe, libwinpthread-1.dll and the first-time sanitizer.json.
$ErrorActionPreference = 'Stop'
$dst = 'C:\HF\ue_asset_sanitizer'
$logs = Join-Path $dst 'logs'
$logFile = 'C:/HF/ue_asset_sanitizer/logs/sanitizer.log'
New-Item -ItemType Directory -Force $dst | Out-Null
New-Item -ItemType Directory -Force $logs | Out-Null
Get-Process ue_asset_sanitizer -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep 1
Copy-Item F:/ue_assets_library/_deploy/*.exe,F:/ue_assets_library/_deploy/*.dll $dst -Force
if (-not (Test-Path "$dst\sanitizer.json")) { Copy-Item F:\ue_assets_library\_deploy\sanitizer.json $dst }

# The service rotates its own log (5 MiB x 6 files by default, rotated files older than 30 days are
# removed). Belt and braces: drop any stray log file in that folder not touched for 30 days.
Get-ChildItem -LiteralPath $logs -File -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -like '*.log' -or $_.Name -like '*.log.*' } |
    Where-Object { $_.LastWriteTime -lt (Get-Date).AddDays(-30) } |
    Remove-Item -Force -ErrorAction SilentlyContinue

$a = New-ScheduledTaskAction -Execute "$dst\ue_asset_sanitizer.exe" -Argument "--watch --config `"$dst\sanitizer.json`" --log-file `"$logFile`"" -WorkingDirectory $dst
$t = New-ScheduledTaskTrigger -AtLogOn -User "$env:COMPUTERNAME\$env:USERNAME"
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -RestartCount 5 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName 'HFUeAssetSanitizer' -Action $a -Trigger $t -Settings $s -Force | Out-Null
Start-ScheduledTask -TaskName 'HFUeAssetSanitizer'
Start-Sleep 3
Get-ScheduledTask -TaskName 'HFUeAssetSanitizer' | Get-ScheduledTaskInfo | Select LastRunTime,LastTaskResult | Format-List
Get-Process ue_asset_sanitizer | Select Id,Path | Format-List
if (Test-Path "$logs\sanitizer.log") { Get-Content "$logs\sanitizer.log" -Tail 8 }
