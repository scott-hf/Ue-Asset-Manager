# Throttled, resumable, serial vision-tag enrichment wrapper (runs on scott-server as task HFUeAssetEnrich).
# Deployed to C:\HF\ue_asset_mcp\run_enrich.ps1 by install_enrich.ps1. ASCII only (Windows PowerShell 5.1).
#
# Config (re-read every loop, edit while running): C:\HF\ue_asset_mcp\enrich.json
#   window_start "01:00", window_end "07:00" (local time; start > end spans midnight; equal = always on)
#   batch 20 (assets per ue_asset_enrich run), pause_s 60 (sleep between batches)
#   pack "" (optional pack filter), ollama_url (optional), idle_s 3600 (sleep when nothing is left)
# Stop: create C:\HF\ue_asset_mcp\enrich.stop (the running batch is ended within a couple of seconds,
#   at most one in-flight asset is lost; every finished asset is already saved atomically) and the loop exits.
# Log: C:\HF\ue_asset_mcp\logs\enrich.log (UTC timestamps, rotated at 5 MB to enrich.log.1).
# ue_asset_enrich is resumable (same model + prompt is skipped), so restarting never redoes work.
# The wrapper and the child run at BelowNormal priority. No secrets are involved.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$root = 'C:\HF\ue_asset_mcp'
$exe = Join-Path $root 'ue_asset_enrich.exe'
$cfgFile = Join-Path $root 'enrich.json'
$stopFile = Join-Path $root 'enrich.stop'
$logDir = Join-Path $root 'logs'
$logFile = Join-Path $logDir 'enrich.log'
$tmpOut = Join-Path $logDir 'enrich.batch.out'
$tmpErr = Join-Path $logDir 'enrich.batch.err'
$library = 'F:/ue_assets_library/library'
$defaultOllama = 'http://192.168.12.136:11434'
$maxLogBytes = 5MB
New-Item -ItemType Directory -Force $logDir | Out-Null

function Write-Log([string]$msg) {
    try {
        if ((Test-Path -LiteralPath $logFile) -and ((Get-Item -LiteralPath $logFile).Length -ge $maxLogBytes)) {
            Move-Item -LiteralPath $logFile -Destination "$logFile.1" -Force
        }
        $ts = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ')
        $clean = ($msg -replace '[^\x20-\x7E]', '?')
        Add-Content -LiteralPath $logFile -Value "$ts $clean" -Encoding ASCII
    } catch { }
}

function Get-Config {
    $c = @{ window_start = '01:00'; window_end = '07:00'; batch = 20; pause_s = 60; pack = ''; ollama_url = $defaultOllama; idle_s = 3600 }
    if (Test-Path -LiteralPath $cfgFile) {
        try {
            $j = (Get-Content -LiteralPath $cfgFile -Raw) | ConvertFrom-Json
            foreach ($k in @('window_start', 'window_end', 'pack', 'ollama_url')) {
                if ($null -ne $j.$k -and "$($j.$k)" -ne '') { $c[$k] = "$($j.$k)" }
            }
            foreach ($k in @('batch', 'pause_s', 'idle_s')) {
                if ($null -ne $j.$k -and [int]$j.$k -gt 0) { $c[$k] = [int]$j.$k }
            }
        } catch { Write-Log "config unreadable, using defaults: $($_.Exception.Message)" }
    }
    return $c
}

function Test-InWindow($c) {
    try {
        $s = [TimeSpan]::Parse($c.window_start); $e = [TimeSpan]::Parse($c.window_end)
    } catch { return $false }
    $t = (Get-Date).TimeOfDay
    if ($s -eq $e) { return $true }
    if ($s -lt $e) { return ($t -ge $s -and $t -lt $e) }
    return ($t -ge $s -or $t -lt $e)
}

function Test-Stop { return (Test-Path -LiteralPath $stopFile) }

# Sleep in 2 s slices so the stop file is noticed quickly. Returns $false when stop was requested.
function Sleep-Checked([int]$seconds) {
    $end = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $end) {
        if (Test-Stop) { return $false }
        Start-Sleep -Seconds 2
    }
    return -not (Test-Stop)
}

function Test-Ollama([string]$url) {
    try {
        $null = Invoke-WebRequest -UseBasicParsing -Uri ($url.TrimEnd('/') + '/api/tags') -TimeoutSec 5
        return $true
    } catch { return $false }
}

# Runs ue_asset_enrich with the given extra args; returns @{ exit; stopped; lines }.
function Invoke-Enrich([string[]]$extra, $c) {
    foreach ($f in @($tmpOut, $tmpErr)) { Remove-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue }
    $a = @('--library', $library, '--no-build', '--cache', (Join-Path $root 'enrich_catalog.json'), '--ollama-url', $c.ollama_url)
    if ($c.pack -ne '') { $a += @('--pack', $c.pack) }
    $a += $extra
    $argLine = ($a | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }) -join ' '
    $p = Start-Process -FilePath $exe -ArgumentList $argLine -WorkingDirectory $root -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $tmpOut -RedirectStandardError $tmpErr
    try { $p.PriorityClass = [System.Diagnostics.ProcessPriorityClass]::BelowNormal } catch { }
    $stopped = $false
    while (-not $p.WaitForExit(2000)) {
        if (Test-Stop) {
            $stopped = $true
            try { $p.Kill() } catch { }
            $null = $p.WaitForExit(10000)
            break
        }
    }
    $code = if ($stopped) { -1 } else { $p.ExitCode }
    $lines = @()
    foreach ($f in @($tmpOut, $tmpErr)) {
        if (Test-Path -LiteralPath $f) { $lines += @(Get-Content -LiteralPath $f -ErrorAction SilentlyContinue) }
    }
    return @{ exit = $code; stopped = $stopped; lines = $lines }
}

function Parse-Done($lines) {
    $r = @{ described = 0; failed = 0; no_thumbnail = 0; already_done = 0; planned = 0; found = $false }
    foreach ($l in $lines) {
        if ($l -match '^done: (\d+) pack\(s\), (\d+) described, (\d+) failed, (\d+) without thumbnail, (\d+) already done(, (\d+) planned)?') {
            $r.found = $true
            $r.described = [int]$Matches[2]; $r.failed = [int]$Matches[3]
            $r.no_thumbnail = [int]$Matches[4]; $r.already_done = [int]$Matches[5]
            if ($Matches[7]) { $r.planned = [int]$Matches[7] }
        }
    }
    return $r
}

# Cheap estimate (a dry run reads every thumbnail and takes many minutes): thumbnails listed in the
# <zip>.assets.json overlays minus items already in the <zip>.vision.json overlays. Only an estimate
# (ignores model/prompt changes and the pack filter match rules); batch results keep it current.
function Get-RemainingEstimate($c) {
    $total = 0; $done = 0
    try {
        $lib = $library.Replace('/', '\')
        $filter = if ($c.pack -ne '') { "*$($c.pack)*" } else { '*' }
        foreach ($f in @(Get-ChildItem -LiteralPath $lib -Filter '*.assets.json' | Where-Object { $_.Name -like $filter })) {
            $total += [regex]::Matches([IO.File]::ReadAllText($f.FullName), '"has_thumbnail":true').Count
        }
        foreach ($f in @(Get-ChildItem -LiteralPath $lib -Filter '*.vision.json' | Where-Object { $_.Name -like $filter })) {
            $done += [regex]::Matches([IO.File]::ReadAllText($f.FullName), '"category":').Count
        }
    } catch { Write-Log "remaining estimate failed: $($_.Exception.Message)"; return 0 }
    return [Math]::Max(0, $total - $done)
}

# Single instance guard.
$created = $false
$mutex = New-Object System.Threading.Mutex($true, 'Global\HFUeAssetEnrichWrapper', [ref]$created)
if (-not $created) { Write-Log 'another wrapper instance is running, exiting'; exit 0 }

try {
    [System.Diagnostics.Process]::GetCurrentProcess().PriorityClass = [System.Diagnostics.ProcessPriorityClass]::BelowNormal
} catch { }
if (-not (Test-Path -LiteralPath $exe)) { Write-Log "missing $exe, exiting"; exit 2 }
Write-Log "wrapper start pid=$PID"

$inWindowPrev = $null
$remaining = -1
$noProgress = 0
$exitReason = 'stop file'
try {
    while ($true) {
        if (Test-Stop) { break }
        $c = Get-Config
        $inWin = Test-InWindow $c
        if ($inWin -ne $inWindowPrev) {
            Write-Log ("window {0}-{1}: now {2}" -f $c.window_start, $c.window_end, $(if ($inWin) { 'inside, working' } else { 'outside, waiting' }))
            $inWindowPrev = $inWin
            if ($inWin) { $remaining = -1 }
        }
        if (-not $inWin) {
            if (-not (Sleep-Checked 30)) { break }
            continue
        }
        if (-not (Test-Ollama $c.ollama_url)) {
            Write-Log "ollama unreachable at $($c.ollama_url), sleeping 300 s"
            if (-not (Sleep-Checked 300)) { break }
            continue
        }
        if ($remaining -lt 0) {
            $remaining = Get-RemainingEstimate $c
            Write-Log "remaining estimate at window start: $remaining (thumbnails in overlays minus described)"
        }
        $eff = [Math]::Min($c.batch * [Math]::Pow(2, [Math]::Min($noProgress, 2)), 400)
        $eff = [int]$eff
        $t0 = Get-Date
        $res = Invoke-Enrich @('--limit', "$eff") $c
        $secs = [int]((Get-Date) - $t0).TotalSeconds
        $r = Parse-Done $res.lines
        if ($res.stopped) { Write-Log "batch ended by stop file after $secs s (in-flight asset discarded)"; break }
        foreach ($l in $res.lines) {
            if ($l -match 'skipped:|thumbnail not readable|^warning:|ue_asset_enrich:') {
                Write-Log ('  ' + $l.Substring(0, [Math]::Min($l.Length, 200)))
            }
        }
        if (-not $r.found) {
            Write-Log "batch exit $($res.exit) without result line, secs=$secs"
            $noProgress++
        } else {
            if ($remaining -ge 0) { $remaining = [Math]::Max(0, $remaining - $r.described) }
            Write-Log ("batch limit={0} described={1} failed={2} no_thumbnail={3} already_done={4} secs={5} remaining~{6}" -f $eff, $r.described, $r.failed, $r.no_thumbnail, $r.already_done, $secs, $remaining)
            if ($r.described -eq 0 -and $r.failed -eq 0) {
                Write-Log "nothing left to describe, idle $($c.idle_s) s"
                $remaining = 0
                if (-not (Sleep-Checked $c.idle_s)) { break }
                $remaining = -1
                continue
            }
            if ($r.described -eq 0) {
                $noProgress++
                if ($noProgress -ge 5) { Write-Log "WARNING: $noProgress batches without progress (failing assets or model); check Ollama" }
            } else { $noProgress = 0 }
        }
        $wait = if ($noProgress -gt 0) { [Math]::Min(600, $c.pause_s * [int][Math]::Pow(2, [Math]::Min($noProgress, 4))) } else { $c.pause_s }
        if (-not (Sleep-Checked $wait)) { break }
    }
} catch {
    $exitReason = "error: $($_.Exception.Message)"
    Write-Log $exitReason
} finally {
    Write-Log "wrapper exit ($exitReason)"
    try { $mutex.ReleaseMutex() } catch { }
}
