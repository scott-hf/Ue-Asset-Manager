# Server-side install of the hosted read-only asset-library MCP service (scheduled task HFUeAssetMcp).
# Run on scott-server (through the WSL ssh route or locally); idempotent, safe to re-run.
# Cloned from the sanitizer install.ps1 template (F:\ue_assets_library\_deploy\install.ps1).
# Source files live in F:\ue_assets_library\_deploy\mcp\ (a SUBDIRECTORY: the sanitizer script
# copies every exe/dll of _deploy itself, so this service must not sit next to it).
# The service is read-only: it never gets --write-overlays and never writes into the library share.
# The bearer token is created once in token.txt (ACL: scott only); its value is never printed or logged.
param([switch]$NoTask)   # -NoTask: copy files and token only, do not register or start the task
$ErrorActionPreference = 'Stop'
$src = 'F:\ue_assets_library\_deploy\mcp'
$dst = 'C:\HF\ue_asset_mcp'
$bundles = Join-Path $dst 'bundles'
$tokenFile = Join-Path $dst 'token.txt'
$bind = '192.168.12.201:8433'
New-Item -ItemType Directory -Force $dst | Out-Null
New-Item -ItemType Directory -Force $bundles | Out-Null

Get-Process ue_asset_mcp -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep 1
Copy-Item "$src\ue_asset_mcp.exe", "$src\ue_asset_enrich.exe", "$src\libwinpthread-1.dll" $dst -Force

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
$a = New-ScheduledTaskAction -Execute "$dst\ue_asset_mcp.exe" -Argument $args2 -WorkingDirectory $dst
$t = New-ScheduledTaskTrigger -AtLogOn -User "$env:COMPUTERNAME\$env:USERNAME"
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -RestartCount 5 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName 'HFUeAssetMcp' -Action $a -Trigger $t -Settings $s -Force | Out-Null
Start-ScheduledTask -TaskName 'HFUeAssetMcp'
Start-Sleep 3
Get-ScheduledTask -TaskName 'HFUeAssetMcp' | Get-ScheduledTaskInfo | Select LastRunTime,LastTaskResult | Format-List
Get-Process ue_asset_mcp -ErrorAction SilentlyContinue | Select Id,Path | Format-List
