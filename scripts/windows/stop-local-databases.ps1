[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..\..')

function Get-DotEnvValue {
    param(
        [Parameter(Mandatory)]
        [string] $Name,
        [string] $Default
    )

    if (Test-Path -LiteralPath '.env') {
        $line = Get-Content -LiteralPath '.env' |
            Where-Object { $_ -match "^\s*$([regex]::Escape($Name))=" } |
            Select-Object -First 1
        if ($line) {
            return ($line -replace '^[^=]*=', '').Trim()
        }
    }
    return $Default
}

$postgresData = Join-Path $env:LOCALAPPDATA 'postgres-benchmark\data'
$postgresRoots = Get-ChildItem -LiteralPath 'C:\Program Files\PostgreSQL' -Directory `
    -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -match '^\d+(\.\d+)*$' } |
    Sort-Object { [int]($_.Name -split '\.')[0] } -Descending
if ($postgresRoots -and (Test-Path -LiteralPath $postgresData)) {
    $pgCtl = Join-Path $postgresRoots[0].FullName 'bin\pg_ctl.exe'
    & $pgCtl -D $postgresData status *> $null
    if ($LASTEXITCODE -eq 0) {
        & $pgCtl -D $postgresData stop -m fast -w
        if ($LASTEXITCODE -ne 0) {
            throw 'Failed to stop the isolated PostgreSQL instance.'
        }
        Write-Host 'PostgreSQL benchmark instance stopped.'
    }
    else {
        Write-Host 'PostgreSQL benchmark instance is already stopped.'
    }
}

$mysqlHome = Join-Path $env:LOCALAPPDATA 'Programs\MySQL\mysql-8.4.9-winx64'
$mysqlAdmin = Join-Path $mysqlHome 'bin\mysqladmin.exe'
$mysqld = Join-Path $mysqlHome 'bin\mysqld.exe'
$listener = Get-NetTCPConnection -State Listen -LocalPort 3306 -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($listener) {
    $process = Get-Process -Id $listener.OwningProcess -ErrorAction Stop
    if ([IO.Path]::GetFullPath($process.Path) -ne [IO.Path]::GetFullPath($mysqld)) {
        throw "Port 3306 belongs to another process: $($process.Path)"
    }

    $env:MYSQL_PWD = Get-DotEnvValue -Name 'MYSQL_ROOT_PASSWORD' `
        -Default 'local_root_password'
    try {
        & $mysqlAdmin --protocol=tcp --host=127.0.0.1 --port=3306 `
            --user=root shutdown
        if ($LASTEXITCODE -ne 0) {
            throw 'Failed to stop the isolated MySQL instance.'
        }
    }
    finally {
        Remove-Item Env:MYSQL_PWD -ErrorAction SilentlyContinue
    }
    Write-Host 'MySQL benchmark instance stopped.'
}
else {
    Write-Host 'MySQL benchmark instance is already stopped.'
}
