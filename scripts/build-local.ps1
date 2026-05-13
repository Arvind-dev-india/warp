<#
.SYNOPSIS
    Build warp_local_proxy and warp-oss for Windows.

.EXAMPLE
    # Build both (release)
    .\scripts\build-local.ps1

    # Build only the proxy (debug)
    .\scripts\build-local.ps1 -ProxyOnly -Profile debug

    # Build only warp-oss
    .\scripts\build-local.ps1 -WarpOnly
#>

[CmdletBinding()]
param(
    [ValidateSet("release","debug")]
    [string]$Profile = "release",
    [switch]$ProxyOnly,
    [switch]$WarpOnly
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent (Split-Path -Parent $PSCommandPath)

Push-Location $RepoRoot
try {
    $releaseFlag = if ($Profile -eq "release") { "--release" } else { $null }

    if (-not $WarpOnly) {
        Write-Host "=== Building warp_local_proxy ($Profile) ===" -ForegroundColor Cyan
        $proxyArgs = @("build", "-p", "warp_local_proxy")
        if ($releaseFlag) { $proxyArgs += $releaseFlag }
        & cargo @proxyArgs
        if ($LASTEXITCODE -ne 0) { throw "warp_local_proxy build failed" }

        $ext = if ($env:OS -eq "Windows_NT") { ".exe" } else { "" }
        $bin = Join-Path $RepoRoot "target" $Profile "warp-local-proxy$ext"
        Write-Host "  -> $bin" -ForegroundColor Green
    }

    if (-not $ProxyOnly) {
        Write-Host ""
        Write-Host "=== Building warp-oss ($Profile) ===" -ForegroundColor Cyan
        $warpArgs = @("build", "--bin", "warp-oss")
        if ($releaseFlag) { $warpArgs += $releaseFlag }
        & cargo @warpArgs
        if ($LASTEXITCODE -ne 0) { throw "warp-oss build failed" }

        $ext = if ($env:OS -eq "Windows_NT") { ".exe" } else { "" }
        $bin = Join-Path $RepoRoot "target" $Profile "warp-oss$ext"
        Write-Host "  -> $bin" -ForegroundColor Green
    }

    Write-Host ""
    Write-Host "Build complete." -ForegroundColor Green
} finally { Pop-Location }
