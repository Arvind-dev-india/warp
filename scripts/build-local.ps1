<#
.SYNOPSIS
    Build warp_local_proxy, warp-oss, and warp-tui-oss for Windows.
    Automatically installs required dependencies (Rust, protoc, Git LFS files).

.EXAMPLE
    # Build both (release)
    .\scripts\build-local.ps1

    # Build only the proxy (debug)
    .\scripts\build-local.ps1 -ProxyOnly -Profile debug

    # Build only warp-oss
    .\scripts\build-local.ps1 -WarpOnly

    # Build proxy, warp-oss, and warp-tui-oss
    .\scripts\build-local.ps1 -Tui

    # Build only the interactive Warp Agent CLI
    .\scripts\build-local.ps1 -TuiOnly -Profile debug

    # Skip dependency checks (faster rebuild)
    .\scripts\build-local.ps1 -SkipDeps
#>

[CmdletBinding()]
param(
    [ValidateSet("release","debug")]
    [string]$Profile = "release",
    [switch]$ProxyOnly,
    [switch]$WarpOnly,
    [switch]$Tui,
    [switch]$TuiOnly,
    [switch]$SkipDeps
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent (Split-Path -Parent $PSCommandPath)

if ($TuiOnly -and ($ProxyOnly -or $WarpOnly -or $Tui)) {
    throw "-TuiOnly cannot be combined with -ProxyOnly, -WarpOnly, or -Tui."
}

function Prepare-LocalTuiResources([string]$TuiBin) {
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

# --- Dependency checks ---
if (-not $SkipDeps) {
    Write-Host "=== Checking dependencies ===" -ForegroundColor Cyan

    # 1. Rust / Cargo
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        $cargobin = Join-Path $env:USERPROFILE ".cargo\bin"
        if (Test-Path (Join-Path $cargobin "cargo.exe")) {
            $env:PATH = "$cargobin;$env:PATH"
        } else {
            Write-Host "  Installing Rust via rustup..." -ForegroundColor Yellow
            $rustupInit = Join-Path $env:TEMP "rustup-init.exe"
            Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $rustupInit
            & $rustupInit -y --quiet
            Remove-Item $rustupInit -ErrorAction SilentlyContinue
            $env:PATH = "$cargobin;$env:PATH"
            if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
                throw "Failed to install Rust. Please install manually from https://rustup.rs"
            }
            Write-Host "  Rust $(cargo --version) installed." -ForegroundColor Green
        }
    }

    # 2. protoc (Protocol Buffers compiler)
    if (-not (Get-Command protoc -ErrorAction SilentlyContinue)) {
        $protocDir = Join-Path $env:USERPROFILE ".local\protoc"
        $protocBin = Join-Path $protocDir "bin\protoc.exe"
        if (Test-Path $protocBin) {
            $env:PATH = "$(Join-Path $protocDir 'bin');$env:PATH"
        } else {
            Write-Host "  Installing protoc..." -ForegroundColor Yellow
            $protocVer = "29.3"
            $protocUrl = "https://github.com/protocolbuffers/protobuf/releases/download/v$protocVer/protoc-$protocVer-win64.zip"
            $protocZip = Join-Path $env:TEMP "protoc.zip"
            New-Item -ItemType Directory -Force -Path $protocDir | Out-Null
            Invoke-WebRequest -Uri $protocUrl -OutFile $protocZip
            Expand-Archive -Path $protocZip -DestinationPath $protocDir -Force
            Remove-Item $protocZip -ErrorAction SilentlyContinue
            $env:PATH = "$(Join-Path $protocDir 'bin');$env:PATH"
            if (-not (Get-Command protoc -ErrorAction SilentlyContinue)) {
                throw "Failed to install protoc. Please install manually from https://github.com/protocolbuffers/protobuf/releases"
            }
            Write-Host "  protoc $(protoc --version) installed." -ForegroundColor Green
        }
    }

    # 3. Git LFS — ensure large binary assets are present
    if (Get-Command git -ErrorAction SilentlyContinue) {
        $lfsTestFile = Join-Path $RepoRoot "app\assets\windows\x64\conpty.dll"
        if ((Test-Path $lfsTestFile) -and (Get-Item $lfsTestFile).Length -lt 1KB) {
            # File exists but is an LFS pointer (tiny text file), not the real binary
            Write-Host "  Pulling Git LFS files..." -ForegroundColor Yellow
            git -C $RepoRoot lfs pull
            if ($LASTEXITCODE -ne 0) { throw "git lfs pull failed" }
            Write-Host "  Git LFS files pulled." -ForegroundColor Green
        } elseif (-not (Test-Path $lfsTestFile)) {
            Write-Host "  Pulling Git LFS files..." -ForegroundColor Yellow
            git -C $RepoRoot lfs pull
            if ($LASTEXITCODE -ne 0) { throw "git lfs pull failed" }
            Write-Host "  Git LFS files pulled." -ForegroundColor Green
        }
    }

    Write-Host "  All dependencies OK." -ForegroundColor Green
    Write-Host ""
}

Push-Location $RepoRoot
try {
    $releaseFlag = if ($Profile -eq "release") { "--release" } else { $null }

    # Set CARGO_FULL_PROFILE so the warp build.rs copies DLLs (conpty, DXC)
    # to the correct target subdirectory (release vs debug).
    $env:CARGO_FULL_PROFILE = $Profile

    if (-not $TuiOnly -and -not $WarpOnly) {
        Write-Host "=== Building warp_local_proxy ($Profile) ===" -ForegroundColor Cyan
        $proxyArgs = @("build", "-p", "warp_local_proxy")
        if ($releaseFlag) { $proxyArgs += $releaseFlag }
        & cargo @proxyArgs
        if ($LASTEXITCODE -ne 0) { throw "warp_local_proxy build failed" }

        $ext = if ($env:OS -eq "Windows_NT") { ".exe" } else { "" }
        $bin = Join-Path (Join-Path (Join-Path $RepoRoot "target") $Profile) "warp-local-proxy$ext"
        Write-Host "  -> $bin" -ForegroundColor Green
    }

    if (-not $TuiOnly -and -not $ProxyOnly) {
        Write-Host ""
        Write-Host "=== Building warp-oss ($Profile) ===" -ForegroundColor Cyan
        $warpArgs = @("build", "--bin", "warp-oss")
        if ($releaseFlag) { $warpArgs += $releaseFlag }
        & cargo @warpArgs
        if ($LASTEXITCODE -ne 0) { throw "warp-oss build failed" }

        $ext = if ($env:OS -eq "Windows_NT") { ".exe" } else { "" }
        $bin = Join-Path (Join-Path (Join-Path $RepoRoot "target") $Profile) "warp-oss$ext"
        Write-Host "  -> $bin" -ForegroundColor Green

        # Ensure Windows runtime DLLs are in the target directory.
        # build.rs handles this via CARGO_FULL_PROFILE, but cargo may skip
        # re-running it on incremental builds. Copy them as a safety net.
        if ($env:OS -eq "Windows_NT") {
            $targetDir = Join-Path (Join-Path $RepoRoot "target") $Profile
            $arch = if ([Environment]::Is64BitOperatingSystem) { "x64" } else { "arm64" }
            $assetDir = Join-Path (Join-Path (Join-Path (Join-Path $RepoRoot "app") "assets\windows") $arch) ""
            $dllsToCopy = @("conpty.dll", "dxcompiler.dll", "dxil.dll")
            foreach ($dll in $dllsToCopy) {
                $src = Join-Path $assetDir $dll
                $dest = Join-Path $targetDir $dll
                if ((Test-Path $src) -and -not (Test-Path $dest)) {
                    Copy-Item $src $dest
                    Write-Host "  Copied $dll to $targetDir" -ForegroundColor Yellow
                }
            }
            $archDir = Join-Path $targetDir $arch
            $consoleSrc = Join-Path $assetDir "OpenConsole.exe"
            if ((Test-Path $consoleSrc) -and -not (Test-Path (Join-Path $archDir "OpenConsole.exe"))) {
                New-Item -ItemType Directory -Force -Path $archDir | Out-Null
                Copy-Item $consoleSrc $archDir
                Write-Host "  Copied OpenConsole.exe to $archDir" -ForegroundColor Yellow
            }
        }
    }

    if ($Tui -or $TuiOnly) {
        Write-Host ""
        Write-Host "=== Building warp-tui-oss ($Profile) ===" -ForegroundColor Cyan
        $tuiArgs = @(
            "build",
            "-p", "warp_tui",
            "--bin", "warp-tui-oss",
            "--features", "standalone"
        )
        if ($releaseFlag) { $tuiArgs += $releaseFlag }
        $previousBuildJobs = $env:CARGO_BUILD_JOBS
        if (-not $env:CARGO_BUILD_JOBS) {
            $env:CARGO_BUILD_JOBS = "2"
        }
        try {
            & cargo @tuiArgs
            if ($LASTEXITCODE -ne 0) { throw "warp-tui-oss build failed" }
        } finally {
            $env:CARGO_BUILD_JOBS = $previousBuildJobs
        }

        $ext = if ($env:OS -eq "Windows_NT") { ".exe" } else { "" }
        $tuiBin = Join-Path (Join-Path (Join-Path $RepoRoot "target") $Profile) "warp-tui-oss$ext"
        Write-Host "  -> $tuiBin" -ForegroundColor Green
        Write-Host "=== Preparing warp-tui-oss resources ===" -ForegroundColor Cyan
        Prepare-LocalTuiResources $tuiBin
    }

    Write-Host ""
    Write-Host "Build complete." -ForegroundColor Green
} finally { Pop-Location }
