# Copyright (c) KanariNetwork, Inc.
# SPDX-License-Identifier: Apache-2.0

<#
.SYNOPSIS
  Boot a local multi-node Kanari EVM network (kanari-sdk style).

.DESCRIPTION
  Generates a DAG committee (dag-committee.json + validator-{i}.key) once,
  then opens one terminal window per validator. Each validator runs a
  Mysticeti DAG core on a localhost TCP mesh and executes committed payloads
  into its own EVM state, so all validators converge to identical blocks.

  DAG ports start at BaseDagPort with a 10-port stride per validator and RPC
  is DAG port + 1 (e.g. validator 1: DAG 3500 / RPC 3501 with defaults).
  Keep (BaseDagPort + (NodeCount-1) * 10) * 10 <= 65535: the mesh dials out
  from source port listen * 10.

.EXAMPLE
  .\start-validators.ps1 -NodeCount 4
  .\start-validators.ps1 -NodeCount 4 -Reset -BaseDagPort 3500
  .\start-validators.ps1 -NodeCount 4 -Reset -ListenHost auto
#>

param(
    [int]$NodeCount = 4,
    [string]$KeysDir = "./dag-keys",
    [string]$DataRoot = "./.kanari-evm-validators",
    # DAG/RPC listen + advertised IP. "127.0.0.1" = this machine only;
    # "auto" = first LAN IPv4 (other machines can join via the committee
    # file); any explicit IP works the same way.
    [string]$ListenHost = "0.0.0.0",
    [int]$BaseDagPort = 3500,
    [string]$FaucetKey = "",
    [switch]$Reset
)

$ErrorActionPreference = "Stop"

function Get-LanIp {
    $ip = [System.Net.Dns]::GetHostAddresses([System.Net.Dns]::GetHostName()) |
        Where-Object {
            $_.AddressFamily -eq [System.Net.Sockets.AddressFamily]::InterNetwork -and
            $_.IPAddressToString -notlike "127.*"
        } |
        Select-Object -First 1 -ExpandProperty IPAddressToString
    if (-not $ip) {
        Write-Error "Could not detect a LAN IP; pass -ListenHost explicitly."
    }
    return $ip
}

# Leftover validators from previous runs hold the RocksDB lock — a new
# process on the same data dir then fails with "Failed to open RocksDB".
$leftovers = Get-Process kanari-evm-node -ErrorAction SilentlyContinue
if ($leftovers) {
    Write-Warning ("Found {0} running kanari-evm-node process(es). " -f $leftovers.Count +
        "Close old validator windows first, or use -Reset on a stopped network.")
}

if ($ListenHost -eq "auto") {
    $ListenHost = Get-LanIp
    Write-Host "Listening + advertising on LAN address: $ListenHost"
}
$advertiseHost = $ListenHost
if ($advertiseHost -eq "0.0.0.0") {
    # Bind-all is fine locally, but 0.0.0.0 in dag-committee.json is not
    # dialable from other machines — advertise the LAN IP instead.
    $advertiseHost = Get-LanIp
    Write-Host "Binding 0.0.0.0, advertising LAN address: $advertiseHost"
}
if ($advertiseHost -ne "127.0.0.1") {
    Write-Host "Allow inbound TCP on DAG ports $BaseDagPort..$($BaseDagPort + ($NodeCount - 1) * 10) in the firewall."
}

if ($NodeCount -lt 4) {
    Write-Error "Need at least 4 validators for quorum (got $NodeCount)."
}

if ($Reset -and (Test-Path $DataRoot)) {
    Write-Host "Removing previous validator state in $DataRoot ..."
    Remove-Item -Recurse -Force $DataRoot
}

Write-Host "Building kanari-evm-node ..."
cargo build --bin kanari-evm-node
$nodeExe = Join-Path (Get-Location) "target\debug\kanari-evm-node.exe"

if (-not (Test-Path (Join-Path $KeysDir "dag-committee.json"))) {
    Write-Host "Generating committee for $NodeCount validators in $KeysDir ..."
    & $nodeExe keygen --node-count $NodeCount --output-dir $KeysDir `
        --host $advertiseHost --base-dag-port $BaseDagPort
} else {
    Write-Host "Reusing existing committee in $KeysDir ."
    $existing = (Get-Content (Join-Path $KeysDir "dag-committee.json") -Raw | ConvertFrom-Json).authorities
    Write-Host ("  committee advertises: {0}" -f (($existing | ForEach-Object { $_.dag_address }) -join ", "))
    Write-Host "  (delete ./dag-keys to regenerate with different IPs)"
}

$committee = Join-Path $KeysDir "dag-committee.json"
# Prefer PowerShell 7 when installed, fall back to Windows PowerShell 5.1.
$shell = "powershell"
if (Get-Command pwsh -ErrorAction SilentlyContinue) {
    $shell = "pwsh"
}
for ($i = 1; $i -le $NodeCount; $i++) {
    $dagPort = $BaseDagPort + ($i - 1) * 10
    $rpcPort = $dagPort + 1
    $dataDir = Join-Path $DataRoot "node$i"
    $keyFile = Join-Path $KeysDir "validator-$i.key"
    # Per-validator TOML (the --config path); CLI flags still win if added.
    # NOTE: TOML basic strings treat `\` as escape, so paths use `/`.
    $committeeToml = $committee -replace '\\', '/'
    $keyToml = $keyFile -replace '\\', '/'
    $dataToml = $dataDir -replace '\\', '/'
    $toml = @"
committee = "$committeeToml"
key = "$keyToml"
data_dir = "$dataToml"
rpc_host = "$ListenHost"
rpc_port = $rpcPort
log_level = "info"
"@
    if ($FaucetKey -ne "") {
        $toml += "`nfaucet_key = `"$FaucetKey`""
    }
    $configFile = Join-Path $dataDir "validator.toml"
    New-Item -ItemType Directory -Path $dataDir -Force | Out-Null
    Set-Content -Path $configFile -Value $toml -Encoding utf8
    Write-Host "Starting validator $i (DAG $dagPort / RPC $rpcPort / $dataDir) ..."
    # NOTE: never name this $args — that is a PowerShell automatic variable
    # and the assignment would not reach Start-Process.
    $cmdLine = "& '$nodeExe' validator --config '$configFile'"
    Start-Process $shell -ArgumentList @("-NoExit", "-Command", $cmdLine)
}

Write-Host ""
Write-Host "Validators starting. RPC endpoints:"
for ($i = 1; $i -le $NodeCount; $i++) {
    $rpcPort = $BaseDagPort + ($i - 1) * 10 + 1
    Write-Host ("  validator {0}: http://{1}:{2}/ (chain id 19088)" -f $i, $ListenHost, $rpcPort)
}
Write-Host "Submit a raw transaction to any validator; all four seal it on commit."
