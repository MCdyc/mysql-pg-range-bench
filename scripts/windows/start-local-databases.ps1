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

function Get-ListeningProcess {
    param([Parameter(Mandatory)][int] $Port)

    $listener = Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $listener) {
        return $null
    }
    return Get-Process -Id $listener.OwningProcess -ErrorAction Stop
}

$mysqlHome = Join-Path $env:LOCALAPPDATA 'Programs\MySQL\mysql-8.4.9-winx64'
$mysqlInstance = Join-Path $env:LOCALAPPDATA 'mysql-benchmark'
$mysqlData = Join-Path $mysqlInstance 'data'
$mysqld = Join-Path $mysqlHome 'bin\mysqld.exe'
$mysqlAdmin = Join-Path $mysqlHome 'bin\mysqladmin.exe'
$mysqlPort = 3306

if (-not (Test-Path -LiteralPath $mysqld) -or -not (Test-Path -LiteralPath $mysqlData)) {
    throw "The local MySQL benchmark instance is not installed at $mysqlHome"
}

$mysqlProcess = Get-ListeningProcess -Port $mysqlPort
if ($mysqlProcess) {
    if ([IO.Path]::GetFullPath($mysqlProcess.Path) -ne [IO.Path]::GetFullPath($mysqld)) {
        throw "Port $mysqlPort belongs to another process: $($mysqlProcess.Path)"
    }
    Write-Host "MySQL is already running on 127.0.0.1:$mysqlPort."
}
else {
    $arguments = @(
        "--basedir=$mysqlHome",
        "--datadir=$mysqlData",
        "--port=$mysqlPort",
        '--bind-address=127.0.0.1',
        '--mysqlx=0',
        "--log-error=$mysqlInstance\mysql-error.log",
        '--pid-file=mysqld.pid',
        '--skip-log-bin',
        '--innodb-flush-log-at-trx-commit=1',
        '--max-allowed-packet=64M',
        '--character-set-server=utf8mb4',
        '--collation-server=utf8mb4_0900_ai_ci',
        '--default-time-zone=+00:00'
    )
    $mysqlProcess = Start-Process -FilePath $mysqld -ArgumentList $arguments `
        -WorkingDirectory $mysqlInstance -WindowStyle Hidden -PassThru

    $env:MYSQL_PWD = Get-DotEnvValue -Name 'DB_PASSWORD' -Default 'benchmark_password'
    try {
        $ready = $false
        for ($attempt = 0; $attempt -lt 60; $attempt++) {
            Start-Sleep -Milliseconds 500
            & $mysqlAdmin --protocol=tcp --host=127.0.0.1 --port=$mysqlPort `
                --user=benchmark ping --silent 2>$null
            if ($LASTEXITCODE -eq 0) {
                $ready = $true
                break
            }
            if ($mysqlProcess.HasExited) {
                break
            }
        }
    }
    finally {
        Remove-Item Env:MYSQL_PWD -ErrorAction SilentlyContinue
    }
    if (-not $ready) {
        Get-Content -LiteralPath (Join-Path $mysqlInstance 'mysql-error.log') `
            -Tail 40 -ErrorAction SilentlyContinue
        throw 'MySQL did not become ready.'
    }
    Write-Host "MySQL started on 127.0.0.1:$mysqlPort."
}

$postgresRoot = 'C:\Program Files\PostgreSQL'
$postgresVersion = Get-ChildItem -LiteralPath $postgresRoot -Directory |
    Where-Object { $_.Name -match '^\d+(\.\d+)*$' } |
    Sort-Object { [int]($_.Name -split '\.')[0] } -Descending |
    Select-Object -First 1
if (-not $postgresVersion) {
    throw "No PostgreSQL installation was found under $postgresRoot"
}

$postgresBin = Join-Path $postgresVersion.FullName 'bin'
$postgresExe = Join-Path $postgresBin 'postgres.exe'
$pgIsReady = Join-Path $postgresBin 'pg_isready.exe'
$postgresInstance = Join-Path $env:LOCALAPPDATA 'postgres-benchmark'
$postgresData = Join-Path $postgresInstance 'data'
$postgresPort = 55432

if (-not (Test-Path -LiteralPath $postgresData)) {
    throw "The isolated PostgreSQL benchmark cluster is missing at $postgresData"
}

$postgresProcess = Get-ListeningProcess -Port $postgresPort
if ($postgresProcess) {
    if ([IO.Path]::GetFullPath($postgresProcess.Path) -ne [IO.Path]::GetFullPath($postgresExe)) {
        throw "Port $postgresPort belongs to another process: $($postgresProcess.Path)"
    }
    Write-Host "PostgreSQL is already running on 127.0.0.1:$postgresPort."
}
else {
    $arguments = @(
        '-D', $postgresData,
        '-p', "$postgresPort",
        '-h', '127.0.0.1',
        '-c', 'timezone=UTC',
        '-c', 'max_parallel_workers_per_gather=0'
    )
    $postgresProcess = Start-Process -FilePath $postgresExe -ArgumentList $arguments `
        -WorkingDirectory $postgresInstance -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $postgresInstance 'postgres-stdout.log') `
        -RedirectStandardError (Join-Path $postgresInstance 'postgres-stderr.log') `
        -PassThru

    $ready = $false
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        Start-Sleep -Milliseconds 500
        & $pgIsReady -h 127.0.0.1 -p $postgresPort -d benchmark *> $null
        if ($LASTEXITCODE -eq 0) {
            $ready = $true
            break
        }
        if ($postgresProcess.HasExited) {
            break
        }
    }
    if (-not $ready) {
        Get-Content -LiteralPath (Join-Path $postgresInstance 'postgres-stderr.log') `
            -Tail 40 -ErrorAction SilentlyContinue
        throw 'PostgreSQL did not become ready.'
    }
    Write-Host "PostgreSQL started on 127.0.0.1:$postgresPort."
}

Write-Host 'Local benchmark databases are ready.'
Write-Host '  MySQL:     127.0.0.1:3306 / benchmark'
Write-Host '  PostgreSQL: 127.0.0.1:55432 / benchmark'
