# Host-side checks for the HFUeAssetMcp service. No bearer token is used (the service runs with --no-auth).
# Restarts the service twice to time cold (cache deleted) and warm (cache present) start.
# Each start reports two times: listener (first /health 200, status may still be "loading") and
# ready (/health status "ok" = catalog loaded; tools/call answers BUSY until then).
param([switch]$NoRestart)
$ErrorActionPreference = 'Continue'
$dst = 'C:\HF\ue_asset_mcp'
$base = 'http://192.168.12.201:8433'
function Wait-Health {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $listener = -1
    while ($sw.Elapsed.TotalSeconds -lt 240) {
        try {
            $r = Invoke-WebRequest -UseBasicParsing -Uri "$base/health" -TimeoutSec 3
            if ($r.StatusCode -eq 200) {
                $t = [math]::Round($sw.Elapsed.TotalSeconds, 1)
                if ($listener -lt 0) { $listener = $t }
                if ((ConvertFrom-Json $r.Content).status -eq 'ok') { return "listener $listener s, ready $t s" }
            }
        } catch {}
        Start-Sleep -Milliseconds 250
    }
    return "listener $listener s, ready timeout (240 s)"
}
function Restart-Svc($dropCache) {
    Get-Process ue_asset_mcp -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep 1
    if ($dropCache) { Remove-Item -LiteralPath "$dst\catalog.json" -Force -ErrorAction SilentlyContinue }
    Start-ScheduledTask -TaskName 'HFUeAssetMcp'
    return Wait-Health
}
if (-not $NoRestart) {
    Write-Output ("cold start (no cache): " + (Restart-Svc $true))
    Write-Output ("warm start (cache): " + (Restart-Svc $false))
}
function Mcp($body) {
    $h = @{}
    try {
        $r = Invoke-WebRequest -UseBasicParsing -Uri "$base/mcp" -Method Post -ContentType 'application/json' -Headers $h -Body $body -TimeoutSec 60
        return @{ code = [int]$r.StatusCode; body = $r.Content }
    } catch {
        $c = 0; if ($_.Exception.Response) { $c = [int]$_.Exception.Response.StatusCode }
        return @{ code = $c; body = '' }
    }
}
$h = Invoke-WebRequest -UseBasicParsing -Uri "$base/health" -TimeoutSec 5
Write-Output "health: $($h.StatusCode)"
$list = '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
$r = Mcp $list
Write-Output ("tools/list no token: " + $r.code)
$names = ([regex]::Matches($r.body, '"name"\s*:\s*"([a-z_]+)"') | ForEach-Object { $_.Groups[1].Value }) -join ','
Write-Output "tools: $names"
Write-Output ("import_assets listed: " + ($names -match '(^|,)import_assets(,|$)'))
$s = Mcp '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_assets","arguments":{"query":"grass","limit":3}}}'
Write-Output ("search_assets grass (no token): " + $s.code + " bytes=" + $s.body.Length)
