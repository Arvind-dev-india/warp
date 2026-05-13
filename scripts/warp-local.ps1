<#
.SYNOPSIS
    warp-local.ps1 — Windows launcher for warp_local_proxy + warp-oss.

.DESCRIPTION
    Single-command wrapper that runs warp_local_proxy and warp-oss together.
    Equivalent of scripts/warp-local (bash) for Windows / PowerShell.

.EXAMPLE
    # default — start proxy + warp-oss
    .\scripts\warp-local.ps1

    # point at Azure OpenAI
    .\scripts\warp-local.ps1 -Backend "https://myres.openai.azure.com/openai/deployments/gpt-4o" `
        -AuthStyle azure-api-key -ApiKey "abc..." -Model gpt-4o

    # point at Ollama
    .\scripts\warp-local.ps1 -Backend http://localhost:11434/v1 -AuthStyle none -Model llama3.1

    # pass args to warp-oss
    .\scripts\warp-local.ps1 -WarpArgs "whoami"

    # leave the proxy running after warp-oss exits
    .\scripts\warp-local.ps1 -KeepProxy

    # stop a previously-launched proxy
    .\scripts\warp-local.ps1 -StopProxy
#>

[CmdletBinding()]
param(
    [string]$Bind          = "127.0.0.1:8765",
    [string]$Backend       = "",
    [string]$AuthStyle     = "",
    [string]$ApiKey        = "",
    [string]$AzureApiVersion = "",
    [string]$Model         = "",
    [ValidateSet("release","debug")]
    [string]$Profile       = "release",
    [switch]$KeepProxy,
    [switch]$StopProxy,
    [Parameter(ValueFromRemainingArguments)]
    [string[]]$WarpArgs
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# ---- paths ------------------------------------------------------------------

$RepoRoot  = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
$ProxyLog  = if ($env:WARP_LOCAL_PROXY_LOG)     { $env:WARP_LOCAL_PROXY_LOG }
             else { Join-Path $env:TEMP "warp-local-proxy.log" }
$ProxyPidFile = if ($env:WARP_LOCAL_PROXY_PIDFILE) { $env:WARP_LOCAL_PROXY_PIDFILE }
                else { Join-Path $env:TEMP "warp-local-proxy.pid" }

# ---- config file ------------------------------------------------------------
# Reads KEY=VALUE lines from ~/.config/warp-local/config.env (same file as Linux).

$ConfigFile = if ($env:WARP_LOCAL_CONFIG) { $env:WARP_LOCAL_CONFIG }
              else { Join-Path $HOME ".config" "warp-local" "config.env" }

$ConfigVars = @{}
if (Test-Path $ConfigFile) {
    Get-Content $ConfigFile | ForEach-Object {
        $line = $_.Trim()
        if ($line -and -not $line.StartsWith("#")) {
            $eqIdx = $line.IndexOf("=")
            if ($eqIdx -gt 0) {
                $key = $line.Substring(0, $eqIdx).Trim()
                $val = $line.Substring($eqIdx + 1).Trim()
                $ConfigVars[$key] = $val
            }
        }
    }
}

# Resolve: CLI param → config file → env var → default
function Resolve-Setting([string]$CliValue, [string]$ConfigKey, [string]$EnvKey, [string]$Default) {
    if ($CliValue)                              { return $CliValue }
    if ($ConfigVars.ContainsKey($ConfigKey))     { return $ConfigVars[$ConfigKey] }
    $envVal = [System.Environment]::GetEnvironmentVariable($EnvKey)
    if ($envVal)                                { return $envVal }
    return $Default
}

$Backend       = Resolve-Setting $Backend       "WARP_LOCAL_PROXY_BACKEND"          "WARP_LOCAL_PROXY_BACKEND"          "http://localhost:3113/v1"
$AuthStyle     = Resolve-Setting $AuthStyle     "WARP_LOCAL_PROXY_AUTH_STYLE"       "WARP_LOCAL_PROXY_AUTH_STYLE"       "bearer"
$ApiKey        = Resolve-Setting $ApiKey        "WARP_LOCAL_PROXY_BACKEND_API_KEY"  "WARP_LOCAL_PROXY_BACKEND_API_KEY"  ""
$AzureApiVersion = Resolve-Setting $AzureApiVersion "WARP_LOCAL_PROXY_AZURE_API_VERSION" "WARP_LOCAL_PROXY_AZURE_API_VERSION" ""
$Model         = Resolve-Setting $Model         "WARP_LOCAL_PROXY_DEFAULT_MODEL"    "WARP_LOCAL_PROXY_DEFAULT_MODEL"    "gpt-5-mini"

# ---- helpers ----------------------------------------------------------------

$ProxyHost = $Bind.Split(":")[0]
$ProxyPort = $Bind.Split(":")[1]
$HealthUrl = "http://${ProxyHost}:${ProxyPort}/healthz"

function Test-ProxyAlive {
    try {
        $null = Invoke-RestMethod -Uri $HealthUrl -TimeoutSec 2 -ErrorAction Stop
        return $true
    } catch {
        return $false
    }
}

function Stop-ProxyProcess {
    $stopped = $false

    # Try pidfile first
    if (Test-Path $ProxyPidFile) {
        $pid = (Get-Content $ProxyPidFile -ErrorAction SilentlyContinue).Trim()
        if ($pid) {
            $proc = Get-Process -Id $pid -ErrorAction SilentlyContinue
            if ($proc -and -not $proc.HasExited) {
                $proc.Kill()
                $proc.WaitForExit(3000) | Out-Null
                Write-Host "warp-local: stopped proxy (pid $pid)"
                $stopped = $true
            }
        }
        Remove-Item $ProxyPidFile -Force -ErrorAction SilentlyContinue
    }

    # Orphan detection — find any warp-local-proxy.exe process
    if (-not $stopped) {
        $orphans = Get-Process -Name "warp-local-proxy" -ErrorAction SilentlyContinue
        foreach ($orphan in $orphans) {
            if (-not $orphan.HasExited) {
                $orphan.Kill()
                $orphan.WaitForExit(3000) | Out-Null
                Write-Host "warp-local: stopped orphan proxy (pid $($orphan.Id))"
            }
        }
    }
}

function Build-Proxy {
    $ext = if ($IsWindows -or $env:OS -eq "Windows_NT") { ".exe" } else { "" }
    $binDir = if ($Profile -eq "release") { "release" } else { "debug" }
    $proxyBin = Join-Path $RepoRoot "target" $binDir "warp-local-proxy$ext"

    if (Test-Path $proxyBin) { return }

    Write-Host "warp-local: building warp_local_proxy ($Profile)..."
    Push-Location $RepoRoot
    try {
        if ($Profile -eq "release") {
            cargo build --quiet --release -p warp_local_proxy
        } else {
            cargo build --quiet -p warp_local_proxy
        }
    } finally { Pop-Location }
}

function Get-WarpBin {
    $ext = if ($IsWindows -or $env:OS -eq "Windows_NT") { ".exe" } else { "" }
    $bin = Join-Path $RepoRoot "target" $Profile "warp-oss$ext"
    if (Test-Path $bin) { return $bin }

    Write-Host "warp-local: warp-oss not found at $bin, building (profile=$Profile)..."
    Push-Location $RepoRoot
    try {
        if ($Profile -eq "release") {
            cargo build --release --bin warp-oss
        } else {
            cargo build --bin warp-oss
        }
    } finally { Pop-Location }
    return $bin
}

function Start-Proxy {
    if (Test-ProxyAlive) {
        Write-Host "warp-local: proxy already running on $Bind"
        return
    }

    Build-Proxy

    $ext = if ($IsWindows -or $env:OS -eq "Windows_NT") { ".exe" } else { "" }
    $binDir = if ($Profile -eq "release") { "release" } else { "debug" }
    $proxyBin = Join-Path $RepoRoot "target" $binDir "warp-local-proxy$ext"

    $proxyArgs = @(
        "--bind", $Bind,
        "--backend-base-url", $Backend,
        "--backend-auth-style", $AuthStyle,
        "--default-model", $Model
    )
    if ($ApiKey)        { $proxyArgs += "--backend-api-key", $ApiKey }
    if ($AzureApiVersion) { $proxyArgs += "--azure-api-version", $AzureApiVersion }

    Write-Host "warp-local: starting proxy -> $Bind (backend $Backend, model $Model)"
    Write-Host "warp-local: log -> $ProxyLog"

    # Clear log
    "" | Set-Content $ProxyLog

    # Start as background process
    $proc = Start-Process -FilePath $proxyBin `
                          -ArgumentList $proxyArgs `
                          -RedirectStandardOutput $ProxyLog `
                          -RedirectStandardError $ProxyLog `
                          -WindowStyle Hidden `
                          -PassThru

    $proc.Id | Set-Content $ProxyPidFile

    # Wait for healthz
    $healthy = $false
    for ($i = 0; $i -lt 50; $i++) {
        if (Test-ProxyAlive) {
            Write-Host "warp-local: proxy healthy"
            $healthy = $true
            break
        }
        Start-Sleep -Milliseconds 200
    }

    if (-not $healthy) {
        Write-Host "warp-local: proxy failed to start; tail of log:" -ForegroundColor Red
        if (Test-Path $ProxyLog) { Get-Content $ProxyLog -Tail 20 }
        exit 1
    }
}

# ---- main flow --------------------------------------------------------------

if ($StopProxy) {
    Stop-ProxyProcess
    exit 0
}

Start-Proxy

$warpBin = Get-WarpBin

try {
    Write-Host "warp-local: launching $warpBin $($WarpArgs -join ' ')"

    # If the user picked a non-default bind address, propagate it.
    if ($Bind -ne "127.0.0.1:8765") {
        $env:WARP_SERVER_ROOT_URL = "http://$Bind"
    }

    & $warpBin @WarpArgs
} finally {
    if (-not $KeepProxy) {
        Stop-ProxyProcess
    } else {
        Write-Host "warp-local: leaving proxy running (use -StopProxy to stop it later)"
    }
}
