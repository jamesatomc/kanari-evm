# Start a Kanari EVM node (mirrors kanari-node/start-node.ps1)
param(
    [ValidateSet("start", "local")]
    [string]$Command = "start",

    [ValidateSet("devnet", "testnet", "mainnet")]
    [string]$Network = "devnet",
    [string]$DataDir = "",
    [string]$BaseDataDir = "$env:USERPROFILE\.kanari\evm-devnet",
    [int]$RpcPort = 8545,
    [string]$RpcHost = "127.0.0.1",
    [string]$FaucetKey = "",
    [string]$StateFile = ""
)

$exeCandidates = @(
    (Join-Path $PSScriptRoot "target\debug\kanari-evm-node.exe"),
    (Join-Path $PSScriptRoot "target\release\kanari-evm-node.exe")
)
$exePath = $exeCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $exePath) {
    $onPath = Get-Command kanari-evm-node -ErrorAction SilentlyContinue
    if ($onPath) { $exePath = $onPath.Source }
}
if (-not $exePath) {
    Write-Host 'Error: kanari-evm-node executable not found! Run: cargo build -p kanari-evm --bin kanari-evm-node' -ForegroundColor Red
    exit 1
}
Write-Host "Using: $exePath" -ForegroundColor DarkGray

if ($Command -eq "local") {
    Write-Host '========================================' -ForegroundColor Cyan
    Write-Host 'Starting Kanari EVM Local Node' -ForegroundColor Green
    Write-Host '========================================' -ForegroundColor Cyan
    Write-Host 'RPC Port:' $RpcPort -ForegroundColor Yellow
    Write-Host 'RPC URL:  http://127.0.0.1:'$RpcPort -ForegroundColor Yellow
    Write-Host '========================================' -ForegroundColor Cyan
    Write-Host ''
    & $exePath local --rpc-port $RpcPort
    exit $LASTEXITCODE
}

if ([string]::IsNullOrWhiteSpace($DataDir)) {
    $DataDir = $BaseDataDir
}
if (-not (Test-Path $DataDir)) {
    New-Item -ItemType Directory -Path $DataDir -Force | Out-Null
}

$rpcUrl = "http://${RpcHost}:${RpcPort}"

Write-Host '========================================' -ForegroundColor Cyan
Write-Host 'Starting Kanari EVM Node' -ForegroundColor Green
Write-Host '========================================' -ForegroundColor Cyan
Write-Host 'Network:' $Network -ForegroundColor Yellow
Write-Host 'RPC Port:' $RpcPort -ForegroundColor Yellow
Write-Host 'RPC Bind Host:' $RpcHost -ForegroundColor Yellow
Write-Host 'Data Dir:' $DataDir -ForegroundColor Yellow
Write-Host "RPC URL:  $rpcUrl" -ForegroundColor Yellow
Write-Host '========================================' -ForegroundColor Cyan
Write-Host ''

$nodeArgs = @(
    "start",
    "--network", $Network,
    "--rpc-port", $RpcPort,
    "--rpc-host", $RpcHost,
    "--data-dir", $DataDir
)
if ($FaucetKey -ne "") {
    $nodeArgs += @("--faucet-key", $FaucetKey)
}
if ($StateFile -ne "") {
    $nodeArgs += @("--state-file", $StateFile)
}

& $exePath @nodeArgs
