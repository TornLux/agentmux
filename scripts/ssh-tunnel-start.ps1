# Open an SSH tunnel between this host and a relay (C) so an agentmux
# broker on host A can be reached from host B via C, without exposing
# broker to the LAN. agentmux itself stays on its default 127.0.0.1
# bind — the tunnel makes B's loopback equal to A's loopback.
#
# Quick usage
# -----------
# On host A (broker host):
#     .\scripts\ssh-tunnel-start.ps1 -Side broker `
#         -RemoteHost relay.example.com -RemoteUser bob `
#         -Auth key -KeyFile $env:USERPROFILE\.ssh\id_ed25519
#
# On host B (viewer host):
#     .\scripts\ssh-tunnel-start.ps1 -Side viewer `
#         -RemoteHost relay.example.com -RemoteUser bob `
#         -Auth password
#     # then open http://127.0.0.1:8765/ in a browser, or:
#     #   .\claude-attach.exe --broker http://127.0.0.1:8765 --session default
#
# Stop:
#     .\scripts\ssh-tunnel-stop.ps1 -Side broker      # on A
#     .\scripts\ssh-tunnel-stop.ps1 -Side viewer      # on B
#
# Parameters
# ----------
#   -Side broker | viewer    pick the half (see Topology below)
#   -RemoteHost              relay host C (DNS or IP)
#   -RemoteUser              SSH username on C
#   -Auth key | password     auth method
#   -KeyFile <path>          private-key path (required when -Auth key)
#   -RemoteSshPort  22       SSH port on C
#   -BridgePort     18765    rendezvous port on C; A and B MUST agree
#   -BrokerPort     8765     A-side: agentmux broker's port (ignored on B)
#   -LocalPort      8765     B-side: local port that will reach broker (ignored on A)
#
# Topology
# --------
#   A (broker)  --ssh -R BridgePort:127.0.0.1:BrokerPort-->  C
#   B (viewer)  --ssh -L LocalPort:127.0.0.1:BridgePort-->   C
# A and B can use different SSH accounts on C as long as both can log in.
# C needs no special config (default GatewayPorts=no is what we want).
#
# Auth notes
# ----------
#   -Auth key       uses Windows 10+ built-in ssh.exe with
#                   -i <KeyFile> -o IdentitiesOnly=yes. Host key on first
#                   connect is auto-accepted (StrictHostKeyChecking=accept-new).
#   -Auth password  uses plink.exe (PuTTY) because Windows OpenSSH refuses
#                   to read a password from a non-tty. plink must be on PATH:
#                       winget install --id PuTTY.PuTTY
#                   First connect to a new host needs ONE interactive plink
#                   run so it can cache the host key in the registry; rerun
#                   this script after that. The script detects this case
#                   and tells you when it's the problem.
#
# Process tracking
# ----------------
# PID is recorded at %LOCALAPPDATA%\agentmux\ssh-tunnel-<side>.pid so
# ssh-tunnel-stop.ps1 can find it. Re-running the same -Side while a
# tunnel is already up is refused; stop it first.

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('broker','viewer')]
    [string]$Side,

    [Parameter(Mandatory)]
    [string]$RemoteHost,

    [Parameter(Mandatory)]
    [string]$RemoteUser,

    [Parameter(Mandatory)]
    [ValidateSet('key','password')]
    [string]$Auth,

    [string]$KeyFile,

    [int]$RemoteSshPort = 22,
    [int]$BridgePort    = 18765,
    [int]$BrokerPort    = 8765,
    [int]$LocalPort     = 8765
)

$ErrorActionPreference = 'Stop'

$pidDir = Join-Path $env:LOCALAPPDATA 'agentmux'
if (-not (Test-Path $pidDir)) {
    New-Item -ItemType Directory -Path $pidDir | Out-Null
}
$pidFile = Join-Path $pidDir "ssh-tunnel-$Side.pid"

if (Test-Path $pidFile) {
    $existing = Get-Content $pidFile -ErrorAction SilentlyContinue
    if ($existing -and (Get-Process -Id $existing -ErrorAction SilentlyContinue)) {
        Write-Error "An $Side tunnel is already running (PID $existing). Run ssh-tunnel-stop.ps1 -Side $Side first."
        exit 1
    }
    Remove-Item $pidFile -Force
}

if ($Side -eq 'broker') {
    $forwardFlag = '-R'
    $forwardSpec = "${BridgePort}:127.0.0.1:${BrokerPort}"
} else {
    $forwardFlag = '-L'
    $forwardSpec = "${LocalPort}:127.0.0.1:${BridgePort}"
}

if ($Auth -eq 'key') {
    if (-not $KeyFile) {
        Write-Error "-KeyFile is required when -Auth key."
        exit 1
    }
    if (-not (Test-Path $KeyFile)) {
        Write-Error "Key file not found: $KeyFile"
        exit 1
    }

    $sshExe = (Get-Command ssh.exe -ErrorAction SilentlyContinue).Source
    if (-not $sshExe) {
        Write-Error "ssh.exe not found. Install OpenSSH Client via Settings > Apps > Optional Features."
        exit 1
    }

    $sshArgs = @(
        '-i', $KeyFile,
        '-o', 'IdentitiesOnly=yes',
        '-N',
        $forwardFlag, $forwardSpec,
        '-p', $RemoteSshPort,
        '-o', 'ServerAliveInterval=30',
        '-o', 'ServerAliveCountMax=3',
        '-o', 'ExitOnForwardFailure=yes',
        '-o', 'StrictHostKeyChecking=accept-new',
        "${RemoteUser}@${RemoteHost}"
    )

    Write-Host "Starting $Side-side SSH tunnel via key auth..." -ForegroundColor Cyan
    $proc = Start-Process -FilePath $sshExe -ArgumentList $sshArgs -PassThru -WindowStyle Hidden
}
else {
    $plinkExe = (Get-Command plink.exe -ErrorAction SilentlyContinue).Source
    if (-not $plinkExe) {
        Write-Error @"
plink.exe not found on PATH. Password auth needs PuTTY's plink because
Windows OpenSSH won't read a password from a non-tty.

Install with one of:
  winget install --id PuTTY.PuTTY
  choco install putty

Then restart this shell so PATH picks up plink.exe. Or switch to -Auth key.
"@
        exit 1
    }

    $sec  = Read-Host -Prompt "SSH password for $RemoteUser@${RemoteHost}" -AsSecureString
    $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($sec)
    try   { $plain = [Runtime.InteropServices.Marshal]::PtrToStringAuto($bstr) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }

    # plink uses -P (capital) for SSH port; -batch disables prompts so a
    # missing host-key cache will fail fast instead of hanging.
    $plinkArgs = @(
        '-ssh',
        '-batch',
        '-N',
        $forwardFlag, $forwardSpec,
        '-P', $RemoteSshPort,
        '-pw', $plain,
        "${RemoteUser}@${RemoteHost}"
    )

    Write-Host "Starting $Side-side SSH tunnel via plink password auth..." -ForegroundColor Cyan
    $proc  = Start-Process -FilePath $plinkExe -ArgumentList $plinkArgs -PassThru -WindowStyle Hidden
    $plain = $null
}

Start-Sleep -Milliseconds 800
if ($proc.HasExited) {
    $code = $proc.ExitCode
    if ($Auth -eq 'password' -and $code -ne 0) {
        Write-Error @"
plink exited immediately (exit code $code). Common causes:
  - wrong password
  - host key not yet cached: run once interactively to accept it:
        plink -ssh ${RemoteUser}@${RemoteHost}
    answer 'y' at the host key prompt, Ctrl+C, then re-run this script.
  - sshd refused tunneling (AllowTcpForwarding no on relay)
"@
    } else {
        Write-Error "ssh process exited immediately (exit code $code). Check credentials, host reachability, and remote sshd config."
    }
    exit 1
}

$proc.Id | Set-Content -Path $pidFile -Encoding ASCII

Write-Host "Tunnel up. PID $($proc.Id) recorded in $pidFile" -ForegroundColor Green
if ($Side -eq 'broker') {
    Write-Host "Broker on this host (127.0.0.1:$BrokerPort) is now reachable as 127.0.0.1:$BridgePort on $RemoteHost." -ForegroundColor Green
    Write-Host "On the viewer host, run:" -ForegroundColor Yellow
    Write-Host "  .\scripts\ssh-tunnel-start.ps1 -Side viewer -RemoteHost $RemoteHost -RemoteUser <user> -Auth <key|password> [-BridgePort $BridgePort]" -ForegroundColor Yellow
} else {
    Write-Host "http://127.0.0.1:$LocalPort/ on this host now reaches the broker on the other side." -ForegroundColor Green
    Write-Host "  Browser: start http://127.0.0.1:$LocalPort/" -ForegroundColor Yellow
    Write-Host "  Attach:  .\claude-attach.exe --broker http://127.0.0.1:$LocalPort --session default" -ForegroundColor Yellow
}
