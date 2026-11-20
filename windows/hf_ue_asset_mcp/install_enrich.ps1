# Server-side install of the throttled vision-tag enrichment task HFUeAssetEnrich (per-user logon task).
# Run on scott-server (WSL ssh route or locally); idempotent, safe to re-run. ASCII only (PowerShell 5.1).
# Source files: F:\ue_assets_library\_deploy\mcp\ (run_enrich.ps1). Needs install.ps1 to have staged
# ue_asset_enrich.exe into C:\HF\ue_asset_mcp\ first. An existing enrich.json is kept.
# -NoStart registers the task without starting it.
param([switch]$NoStart)
$ErrorActionPreference = 'Stop'
$src = 'F:\ue_assets_library\_deploy\mcp'
$dst = 'C:\HF\ue_asset_mcp'
$cfg = Join-Path $dst 'enrich.json'
if (-not (Test-Path -LiteralPath "$dst\ue_asset_enrich.exe")) { throw "missing $dst\ue_asset_enrich.exe: run install.ps1 first" }
New-Item -ItemType Directory -Force (Join-Path $dst 'logs') | Out-Null

# Stop a running wrapper through its stop file before replacing it.
$stop = Join-Path $dst 'enrich.stop'
$running = Get-Process -Name powershell -ErrorAction SilentlyContinue | Where-Object { $_.Path -and (Get-CimInstance Win32_Process -Filter "ProcessId=$($_.Id)").CommandLine -like '*run_enrich.ps1*' }
if ($running) {
    New-Item -ItemType File -Force $stop | Out-Null
    Start-Sleep 8
    Get-Process -Name powershell -ErrorAction SilentlyContinue | Where-Object { (Get-CimInstance Win32_Process -Filter "ProcessId=$($_.Id)").CommandLine -like '*run_enrich.ps1*' } | Stop-Process -Force
}
Copy-Item "$src\run_enrich.ps1" $dst -Force

if (-not (Test-Path -LiteralPath $cfg)) {
    $json = '{' + "`r`n" + '  "window_start": "01:00",' + "`r`n" + '  "window_end": "07:00",' + "`r`n" + '  "batch": 20,' + "`r`n" + '  "pause_s": 60,' + "`r`n" + '  "pack": ""' + "`r`n" + '}'
    [System.IO.File]::WriteAllText($cfg, $json, (New-Object System.Text.ASCIIEncoding))
    Write-Output 'enrich.json created (window 01:00-07:00, batch 20, pause 60 s)'
} else {
    Write-Output 'enrich.json kept'
}
if (Test-Path -LiteralPath $stop) { Remove-Item -LiteralPath $stop -Force }

$args2 = "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$dst\run_enrich.ps1`""
$a = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument $args2 -WorkingDirectory $dst
$t = New-ScheduledTaskTrigger -AtLogOn -User "$env:COMPUTERNAME\$env:USERNAME"
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -RestartCount 5 -RestartInterval (New-TimeSpan -Minutes 5) -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -Priority 7
Register-ScheduledTask -TaskName 'HFUeAssetEnrich' -Action $a -Trigger $t -Settings $s -Force | Out-Null
if (-not $NoStart) { Start-ScheduledTask -TaskName 'HFUeAssetEnrich'; Start-Sleep 3 }
Get-ScheduledTask -TaskName 'HFUeAssetEnrich' | Get-ScheduledTaskInfo | Select LastRunTime,LastTaskResult | Format-List
