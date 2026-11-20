# One-time overlay backfill on scott-server (approved by Scott, 2026-11-20): writes derived
# <zip>.assets.json next to each library zip, then self-tests the catalog. The hosted service itself
# never runs with --write-overlays. Output goes to C:\HF\ue_asset_mcp\backfill.log.
$ErrorActionPreference = 'Continue'   # the exe logs to stderr; Stop would turn that into an error
$dst = 'C:\HF\ue_asset_mcp'
$log = Join-Path $dst 'backfill.log'
$sw = [System.Diagnostics.Stopwatch]::StartNew()
& "$dst\ue_asset_mcp.exe" --library F:/ue_assets_library/library --write-overlays --cache "$dst\catalog.json" --no-generated --no-install-log --selftest *> $log
"exit=$LASTEXITCODE seconds=$([int]$sw.Elapsed.TotalSeconds)" | Add-Content $log
