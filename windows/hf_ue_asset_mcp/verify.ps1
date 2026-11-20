# Host-side checks for the HFUeAssetMcp service. Reads the token from the token file and never prints it.
# Restarts the service twice to time cold (cache deleted) and warm (cache present) start to /health 200.
param([switch]$NoRestart)
$ErrorActionPreference = 'Continue'
$dst = 'C:\HF\ue_asset_mcp'
$base = 'http://192.168.12.201:8433'
$tok = (Get-Content -LiteralPath "$dst\token.txt" -Raw).Trim()
function Wait-Health {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt 240) {
        try { $r = Invoke-WebRequest -UseBasicParsing -Uri "$base/health" -TimeoutSec 3; if ($r.StatusCode -eq 200) { return [math]::Round($sw.Elapsed.TotalSeconds, 1) } } catch {}
        Start-Sleep -Milliseconds 250
    }
    return -1
}
function Restart-Svc($dropCache) {
    Get-Process ue_asset_mcp -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep 1
    if ($dropCache) { Remove-Item -LiteralPath "$dst\catalog.json" -Force -ErrorAction SilentlyContinue }
    Start-ScheduledTask -TaskName 'HFUeAssetMcp'
    return Wait-Health
}
if (-not $NoRestart) {
    Write-Output ("cold start seconds (no cache): " + (Restart-Svc $true))
    Write-Output ("warm start seconds (cache): " + (Restart-Svc $false))
}
function Mcp($body, $auth) {
    $h = @{}
    if ($auth) { $h['Authorization'] = "Bearer $tok" }
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
Write-Output ("tools/list no token: " + (Mcp $list $false).code)
$r = Mcp $list $true
Write-Output ("tools/list with token: " + $r.code)
$names = ([regex]::Matches($r.body, '"name"\s*:\s*"([a-z_]+)"') | ForEach-Object { $_.Groups[1].Value }) -join ','
Write-Output "tools: $names"
Write-Output ("import_assets listed: " + ($names -match '(^|,)import_assets(,|$)'))
$s = Mcp '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_assets","arguments":{"query":"grass","limit":3}}}' $true
Write-Output ("search_assets grass: " + $s.code + " bytes=" + $s.body.Length)
