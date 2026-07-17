[CmdletBinding()]
param(
    [ValidateRange(1, [long]::MaxValue)]
    [long] $Rows = 100000,

    [ValidateRange(0, [long]::MaxValue)]
    [long] $ScanRows = 0,

    [string] $Output
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..\..')

if ($ScanRows -eq 0) {
    $ScanRows = [Math]::Max(1, [Math]::Floor($Rows / 6))
}
if ($ScanRows -gt $Rows) {
    throw "ScanRows ($ScanRows) cannot exceed Rows ($Rows)."
}
if (-not $Output) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $Output = "benchmark-results/windows-smoke-$stamp.json"
}

& (Join-Path $PSScriptRoot 'start-local-databases.ps1')

$binary = Join-Path (Get-Location) 'target\release\mysql-pg-range-bench.exe'
if (-not (Test-Path -LiteralPath $binary)) {
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) {
        throw 'Release build failed.'
    }
}

$oldMySqlUrl = $env:MYSQL_URL
$oldPostgresUrl = $env:POSTGRES_URL
$env:MYSQL_URL = 'mysql://benchmark:benchmark_password@127.0.0.1:3306/benchmark'
$env:POSTGRES_URL = 'postgres://benchmark:benchmark_password@127.0.0.1:55432/benchmark'
try {
    & $binary --database both --rows $Rows --scan-rows $ScanRows `
        --batch-size 1000 --transaction-rows 100000 `
        --warmups 2 --runs 5 --output $Output
    if ($LASTEXITCODE -ne 0) {
        throw "Benchmark exited with code $LASTEXITCODE."
    }
}
finally {
    if ($null -eq $oldMySqlUrl) {
        Remove-Item Env:MYSQL_URL -ErrorAction SilentlyContinue
    }
    else {
        $env:MYSQL_URL = $oldMySqlUrl
    }
    if ($null -eq $oldPostgresUrl) {
        Remove-Item Env:POSTGRES_URL -ErrorAction SilentlyContinue
    }
    else {
        $env:POSTGRES_URL = $oldPostgresUrl
    }
}

Write-Host "Benchmark result: $Output"
