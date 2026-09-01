<#
.SYNOPSIS
    warp-local.ps1 — Windows launcher for warp_local_proxy + Warp GUI or TUI.

.DESCRIPTION
    Single-command wrapper that runs warp_local_proxy with warp-oss or warp-tui-oss.
    Equivalent of scripts/warp-local (bash) for Windows / PowerShell.

.EXAMPLE
    # default — start proxy + warp-oss
    .\scripts\warp-local.ps1

    # reuse existing release binaries without invoking Cargo
    .\scripts\warp-local.ps1 -SkipBuild

    # use faster debug binaries when release behavior is not required
    .\scripts\warp-local.ps1 -Profile debug

    # build and run the interactive terminal UI
    .\scripts\warp-local.ps1 -Tui -Profile debug

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
    [switch]$Tui,
    [switch]$SkipBuild,
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
$ClientBinaryName = if ($Tui) { "warp-tui-oss" } else { "warp-oss" }
$ClientDescription = if ($Tui) { "interactive TUI" } else { "GUI" }
$ProxyStartedByLauncher = $false

# ---- config file ------------------------------------------------------------
# Reads KEY=VALUE lines from ~/.config/warp-local/config.env (same file as Linux).

$ConfigFile = if ($env:WARP_LOCAL_CONFIG) { $env:WARP_LOCAL_CONFIG }
              else { Join-Path (Join-Path (Join-Path $HOME ".config") "warp-local") "config.env" }

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
        $proxyPid = (Get-Content $ProxyPidFile -ErrorAction SilentlyContinue).Trim()
        if ($proxyPid) {
            $proc = Get-Process -Id $proxyPid -ErrorAction SilentlyContinue
            if ($proc -and -not $proc.HasExited) {
                $proc.Kill()
                $proc.WaitForExit(3000) | Out-Null
                Write-Host "warp-local: stopped proxy (pid $proxyPid)"
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

function Get-LocalBinaryPath([string]$Name) {
    $ext = if ($env:OS -eq "Windows_NT") { ".exe" } else { "" }
    $binDir = if ($Profile -eq "release") { "release" } else { "debug" }
return Join-Path (Join-Path (Join-Path $RepoRoot "target") $binDir) "$Name$ext"
}

function Build-LocalBinaries {
$buildProxy = -not (Test-ProxyAlive)
$targets = if ($Tui) {
    @(
        "-p", "warp_tui",
        "--bin", "warp-tui-oss",
        "--features", "warp_tui/standalone"
    )
} else {
    @(
        "-p", "warp",
        "--bin", "warp-oss"
    )
}
if ($buildProxy) {
    $targets = @(
        "-p", "warp_local_proxy",
        "--bin", "warp-local-proxy"
    ) + $targets
}

$cargoArgs = @("build")
if ($Profile -eq "release") {
    $cargoArgs += "--release"
}
$cargoArgs += $targets
$description = if ($buildProxy) {
    "proxy + $ClientBinaryName"
} else {
    $ClientBinaryName
}
$warpBin = Get-LocalBinaryPath $ClientBinaryName
if ($Profile -eq "release" -and -not (Test-Path $warpBin)) {
    Write-Host "warp-local: first release build; the optimized $ClientDescription build can take a long time." -ForegroundColor Yellow
    Write-Host "warp-local: compilation progress will be shown below. Later launches can use -SkipBuild." -ForegroundColor Yellow
}
Write-Host "warp-local: incremental $Profile build ($description)..."
$env:CARGO_FULL_PROFILE = $Profile
Push-Location $RepoRoot
try {
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo build failed with exit code $LASTEXITCODE."
    }
} finally { Pop-Location }
}

function Get-ClientBin {
$bin = Get-LocalBinaryPath $ClientBinaryName
if (-not (Test-Path $bin)) {
    $alternative = if ($Profile -eq "release") {
        "Run without -SkipBuild for the one-time release build, or use -Profile debug."
    } else {
        "Run without -SkipBuild once before using -SkipBuild."
    }
    throw "$ClientBinaryName binary not found at $bin. $alternative"
}
return $bin
}

function Prepare-TuiResources([string]$TuiBin) {
$resourcesDir = Join-Path (Split-Path -Parent $TuiBin) "resources"
$bundledSource = Join-Path (Join-Path $RepoRoot "resources") "bundled"
$bundledDestination = Join-Path $resourcesDir "bundled"
$settingsSchema = Join-Path $resourcesDir "settings_schema.json"

New-Item -ItemType Directory -Force -Path $resourcesDir | Out-Null
if (Test-Path $bundledDestination) {
    Remove-Item -LiteralPath $bundledDestination -Recurse -Force
}
Copy-Item -LiteralPath $bundledSource -Destination $bundledDestination -Recurse -Force

& $TuiBin dump-settings-schema $settingsSchema
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $settingsSchema)) {
    throw "Failed to generate TUI settings schema at $settingsSchema."
}
}

function Start-Proxy {
    if (Test-ProxyAlive) {
        Write-Host "warp-local: proxy already running on $Bind"
        return
    }

    $proxyBin = Get-LocalBinaryPath "warp-local-proxy"
    if (-not (Test-Path $proxyBin)) {
        throw "Proxy binary not found at $proxyBin. Run without -SkipBuild first."
    }

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
    $stderrLog = $ProxyLog -replace '\.log$', '-stderr.log'
    $proc = Start-Process -FilePath $proxyBin `
                          -ArgumentList $proxyArgs `
                          -RedirectStandardOutput $ProxyLog `
                          -RedirectStandardError $stderrLog `
                          -WindowStyle Hidden `
                          -PassThru

    $proc.Id | Set-Content $ProxyPidFile
    $script:ProxyStartedByLauncher = $true

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

# ---- user pre-seeding -------------------------------------------------------
# On Windows, if no persisted user exists in secure storage, seed a local-mode
# user so warp-oss skips the onboarding/login flow (matching Linux behavior
# where the user was previously signed in).

function Ensure-LocalUser {
    $applicationName = if ($Tui) { "WarpTui" } else { "WarpOss" }
    $stateDir = Join-Path (Join-Path (Join-Path (Join-Path $env:LOCALAPPDATA "warp") $applicationName) "data") ""
    $dataDomain = if ($Tui) { "dev.warp.WarpTui.tui" } else { "dev.warp.WarpOss" }
    $userFile = Join-Path $stateDir "$dataDomain-User"

    if (Test-Path $userFile) { return }

    Write-Host "warp-local: seeding local user (first run)..." -ForegroundColor Yellow

    if (-not (Test-Path $stateDir)) {
        New-Item -ItemType Directory -Force -Path $stateDir | Out-Null
    }

    # Build the PersistedUser JSON matching the exact serde format.
    # anonymous_user_type is null so is_user_anonymous() returns false,
    # which enables all AI features (same as the proxy's get_user response).
    $userJson = @{
        id_token = @{
            id_token        = "local-mode-token"
            refresh_token   = "local-mode-refresh"
            expiration_time = "2099-12-31T23:59:59Z"
        }
        refresh_token          = ""
        local_id               = "local-user-uid"
        email                  = "local@local"
        display_name           = "Local User"
        photo_url              = $null
        is_onboarded           = $true
        needs_sso_link         = $false
        anonymous_user_type    = $null
        linked_at              = $null
        personal_object_limits = $null
        is_on_work_domain      = $false
    } | ConvertTo-Json -Compress

    # Encrypt with DPAPI (same as Rust's CryptProtectData, CurrentUser scope)
    Add-Type -AssemblyName System.Security
    $plainBytes = [System.Text.Encoding]::UTF8.GetBytes($userJson)
    $encBytes = [System.Security.Cryptography.ProtectedData]::Protect(
        $plainBytes, $null, [System.Security.Cryptography.DataProtectionScope]::CurrentUser)
    [System.IO.File]::WriteAllBytes($userFile, $encBytes)

    Write-Host "warp-local: local user seeded at $userFile" -ForegroundColor Green
}

# ---- main flow --------------------------------------------------------------

if ($StopProxy) {
    Stop-ProxyProcess
    exit 0
}

Ensure-LocalUser

if (-not $SkipBuild) {
    Build-LocalBinaries
}

$warpBin = Get-ClientBin

if ($Tui -and (-not $SkipBuild -or -not (Test-Path (Join-Path (Split-Path -Parent $warpBin) "resources\settings_schema.json")))) {
    Write-Host "warp-local: preparing TUI resources..."
    Prepare-TuiResources $warpBin
}

Start-Proxy

try {
    Write-Host "warp-local: launching $warpBin $($WarpArgs -join ' ')"

    if ($Tui -or $Bind -ne "127.0.0.1:8765") {
        $env:WARP_SERVER_ROOT_URL = "http://$Bind"
    }
    if ($Tui) {
        $env:WARP_WS_SERVER_URL = "ws://$Bind/graphql/v2"
        if (-not $env:WARP_API_KEY) {
            $env:WARP_API_KEY = "local-mode-token"
        }
    }

    # Suppress noisy INFO logs from warp-oss — only show warnings and errors.
    # Save and restore so the proxy (already running) is unaffected.
    $prevRustLog = $env:RUST_LOG
    if (-not $env:RUST_LOG) {
        $env:RUST_LOG = "warn"
    }

    & $warpBin @WarpArgs

    $env:RUST_LOG = $prevRustLog
} finally {
    if (-not $KeepProxy -and $ProxyStartedByLauncher) {
        Stop-ProxyProcess
    } elseif ($KeepProxy) {
        Write-Host "warp-local: leaving proxy running (use -StopProxy to stop it later)"
    }
}
