$launcher = Join-Path $PSScriptRoot "warp-local.ps1"
if ($args -contains "-Profile") {
    & $launcher -Tui @args
} else {
    & $launcher -Tui -Profile debug @args
}
exit $LASTEXITCODE
