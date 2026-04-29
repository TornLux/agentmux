# Stop the agentmux broker. Prefers the broker.pid file for precision;
# falls back to name-based killing if the file is missing/stale.

$ErrorActionPreference = "Stop"

$pidFile = Join-Path $env:LOCALAPPDATA "agentmux\broker.pid"
$killed = $false

if (Test-Path -LiteralPath $pidFile) {
    $brokerPid = (Get-Content -LiteralPath $pidFile -Raw).Trim()
    if ($brokerPid) {
        $proc = Get-Process -Id $brokerPid -ErrorAction SilentlyContinue
        if ($proc -and $proc.ProcessName -eq 'broker') {
            Stop-Process -Id $brokerPid -Force
            Write-Host "broker (pid $brokerPid) stopped via pid file"
            $killed = $true
        }
    }
    Remove-Item -LiteralPath $pidFile -Force -ErrorAction SilentlyContinue
}

if (-not $killed) {
    $procs = @(Get-Process -Name broker -ErrorAction SilentlyContinue)
    if ($procs.Count -gt 0) {
        foreach ($p in $procs) {
            Stop-Process -InputObject $p -Force
            Write-Host "broker (pid $($p.Id)) stopped via name fallback"
        }
    } else {
        Write-Host "broker not running"
    }
}
