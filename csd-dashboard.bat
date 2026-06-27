@echo off
REM csd-dashboard.bat - live terminal dashboard for the CSD pool miner (Windows).
REM Licensed under PolyForm Perimeter 1.0.0 (see LICENSE). Part of csd-pool-miner.
REM
REM READ-ONLY local viewer: GETs http://127.0.0.1:<port>/1/summary once per
REM refresh and draws it. Never writes config, never touches the miner binary
REM or the share/submit path, never opens a non-loopback socket. Worst case it
REM prints "endpoint unreachable". It cannot stop, slow, or corrupt mining.
REM
REM The endpoint exists only when the miner runs with --stats-port (3380).
REM
REM Usage: csd-dashboard.bat [--port N] [--refresh N] [--once] [--no-color] [--update] [-h]
setlocal EnableExtensions EnableDelayedExpansion

set "CSD_DASH_PORT="
set "CSD_DASH_REFRESH=%CSD_REFRESH%"
if "%CSD_DASH_REFRESH%"=="" set "CSD_DASH_REFRESH=2"
set "CSD_DASH_ONCE=0"
set "CSD_DASH_NOCOLOR=0"
set "CSD_DASH_UPDATE=0"
REM capture the script path NOW -- SHIFT in the parse loop also shifts %0
set "CSD_DASH_SELF=%~f0"

:parse
if "%~1"=="" goto parsed
if /i "%~1"=="--port" ( set "CSD_DASH_PORT=%~2" & shift & shift & goto parse )
if /i "%~1"=="--refresh" ( set "CSD_DASH_REFRESH=%~2" & shift & shift & goto parse )
if /i "%~1"=="--once" ( set "CSD_DASH_ONCE=1" & shift & goto parse )
if /i "%~1"=="--no-color" ( set "CSD_DASH_NOCOLOR=1" & shift & goto parse )
if /i "%~1"=="--update" ( set "CSD_DASH_UPDATE=1" & shift & goto parse )
if /i "%~1"=="-h" goto help
if /i "%~1"=="--help" goto help
echo unknown argument: %~1 1>&2
goto help

:help
echo csd-dashboard.bat - live CSD pool miner dashboard ^(read-only viewer^)
echo.
echo   --port N        stats port ^(default: %%CSD_STATS_PORT%% or 3380^)
echo   --refresh N     seconds between refreshes ^(default: 2^)
echo   --once          print one frame and exit
echo   --no-color      disable color
echo   --update        self-update this script from the latest release ^(fail-closed^)
echo   -h, --help      this help
echo.
echo The miner must run with --stats-port ^<port^> for the endpoint to exist.
endlocal & exit /b 0

:parsed
REM resolve endpoint URL: CSD_STATS_URL > --port > CSD_STATS_PORT > 3380
if defined CSD_STATS_URL (
  set "CSD_DASH_URL=%CSD_STATS_URL%"
) else (
  set "_p=%CSD_DASH_PORT%"
  if "!_p!"=="" set "_p=%CSD_STATS_PORT%"
  if "!_p!"=="" set "_p=3380"
  set "CSD_DASH_URL=http://127.0.0.1:!_p!/1/summary"
)

if "%CSD_DASH_UPDATE%"=="1" goto selfupdate

REM Hand off to the PowerShell render: extract the PS section (every line after
REM the marker) to a temp script and run it. A temp render script in %TEMP% is
REM harmless -- it never touches the miner, its config, or the share path.
set "PSL="
for /f "delims=:" %%a in ('findstr /n /b /c:"REM PS_SECTION_BELOW" "%CSD_DASH_SELF%"') do if not defined PSL set "PSL=%%a"
set "PSF=%TEMP%\csd-dash-%RANDOM%%RANDOM%.ps1"
more +%PSL% "%CSD_DASH_SELF%" > "%PSF%"
powershell -NoProfile -ExecutionPolicy Bypass -File "%PSF%"
set "RC=%ERRORLEVEL%"
del "%PSF%" >nul 2>&1
endlocal & exit /b %RC%

:selfupdate
set "CSD_DL=https://github.com/dangraagu/CSD-Mining-pool-public/releases/latest/download"
powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; $name='csd-dashboard.bat'; $dl=$env:CSD_DL; $self=$env:CSD_DASH_SELF; try { $sums=(Invoke-WebRequest -UseBasicParsing -Uri \"$dl/SHA256SUMS\" -TimeoutSec 15).Content; $want=($sums -split \"`n\" | Where-Object { $_ -match ('(?i)\s\*?'+[regex]::Escape($name)+'\s*$') } | ForEach-Object { ($_ -split '\s+')[0] } | Select-Object -First 1); if(-not $want){ Write-Error 'not in SHA256SUMS; refusing'; exit 1 }; $tmp=\"$self.new\"; Invoke-WebRequest -UseBasicParsing -Uri \"$dl/$name\" -OutFile $tmp -TimeoutSec 30; $got=(Get-FileHash -Algorithm SHA256 -LiteralPath $tmp).Hash; if($got -ne $want){ Remove-Item $tmp -Force; Write-Error 'checksum mismatch; kept current'; exit 1 }; Copy-Item -LiteralPath $self -Destination \"$self.bak\" -Force; Move-Item -LiteralPath $tmp -Destination $self -Force; Write-Host \"updated $name (prior at $name.bak)\" } catch { Write-Error $_; exit 1 }"
endlocal & exit /b %ERRORLEVEL%

REM PS_SECTION_BELOW
$url     = $env:CSD_DASH_URL
$refresh = [int]($env:CSD_DASH_REFRESH); if ($refresh -lt 1) { $refresh = 2 }
$once    = $env:CSD_DASH_ONCE -eq '1'
$nocolor = $env:CSD_DASH_NOCOLOR -eq '1'
# force '.' decimals + no locale group separators, identical on every machine
try { [System.Threading.Thread]::CurrentThread.CurrentCulture = [System.Globalization.CultureInfo]::InvariantCulture } catch {}

$script:prevGood = $null
$script:prevTs   = $null

function HR([double]$v) {
  $u='H/s'
  if     ($v -ge 1e12){ $v/=1e12; $u='TH/s' }
  elseif ($v -ge 1e9 ){ $v/=1e9 ; $u='GH/s' }
  elseif ($v -ge 1e6 ){ $v/=1e6 ; $u='MH/s' }
  elseif ($v -ge 1e3 ){ $v/=1e3 ; $u='kH/s' }
  '{0:F2} {1}' -f $v,$u
}
function UP([long]$s) {
  $h=[math]::Floor($s/3600); $m=[math]::Floor(($s%3600)/60); $x=$s%60
  if($h -gt 0){ "{0}h {1}m {2}s" -f $h,$m,$x } elseif($m -gt 0){ "{0}m {1}s" -f $m,$x } else { "{0}s" -f $x }
}
function ADDR([string]$a) { if($a -and $a.Length -gt 14){ $a.Substring(0,6)+'..'+$a.Substring($a.Length-4) } else { $a } }

function Col([string]$txt,[string]$c) {
  if($nocolor){ Write-Host -NoNewline $txt } else { Write-Host -NoNewline $txt -ForegroundColor $c }
}
function Line([string[]]$parts,[string[]]$cols) {
  for($i=0;$i -lt $parts.Count;$i++){ Col $parts[$i] $cols[$i] }
  $pad = 0
  try { $pad = [Console]::WindowWidth - 1 } catch { $pad = 60 }
  $len = ($parts -join '').Length
  if($len -lt $pad){ Write-Host (' ' * ($pad-$len)) } else { Write-Host '' }
}

function Draw {
  $r = $null
  try { $r = Invoke-RestMethod -Uri $url -TimeoutSec 4 } catch { $r = $null }

  if(-not $once){ try { [Console]::SetCursorPosition(0,0) } catch {} }

  if($null -eq $r){
    Line @('  CSD Pool Miner') @('Cyan')
    Line @('') @('Gray')
    Line @('  * stats endpoint unreachable') @('Red')
    Line @('  '+$url) @('DarkGray')
    Line @('') @('Gray')
    Line @('  Is the miner running with --stats-port ?') @('Gray')
    Line @('  HiveOS sets 3380. Standalone: add --stats-port 3380') @('Gray')
    Line @('  to your mine command, or pass --port N.') @('Gray')
    if(-not $once){ Line @('  retrying every '+$refresh+'s - Ctrl-C to quit') @('DarkGray') }
    return
  }

  $ver=$r.version; if(-not $ver){$ver='?'}
  $h = @(0,0,0); if($r.hashrate -and $r.hashrate.total){ $h=$r.hashrate.total }
  $good=[double]$r.results.shares_good; $total=[double]$r.results.shares_total
  $rej=[double]$r.results.shares_rejected; $stale=[double]$r.results.shares_stale
  $pool=$r.connection.pool; if(-not $pool){$pool='n/a'}
  $recon=$r.connection.reconnects; $fail=$r.connection.failovers
  $rejpct = if($total -gt 0){ ($rej+$stale)*100/$total } else { 0 }
  $rejcol = if($rejpct -gt 5){'Red'} elseif($rejpct -gt 1 -or $stale -gt 0){'Yellow'} else {'Green'}

  $now=[int][double]::Parse((Get-Date -UFormat %s))
  $rate='-'
  if($null -ne $script:prevGood -and $now -gt $script:prevTs){
    $d=$good-$script:prevGood; if($d -lt 0){$d=0}
    $rate=('{0:F1}/min' -f ($d*60/($now-$script:prevTs)))
  }
  $script:prevGood=$good; $script:prevTs=$now

  $temp='n/a'; $power='n/a'
  if($r.health){
    if($null -ne $r.health.gpu_temp_c){ $temp = ('{0:F0} C' -f [double]$r.health.gpu_temp_c) }
    if($null -ne $r.health.gpu_power_w){ $power = ('{0:F0} W' -f [double]$r.health.gpu_power_w) }
  }

  Line @('  CSD Pool Miner','   v'+$ver) @('Cyan','DarkGray')
  Line @('  Worker  ',(ADDR $r.worker_id),'    Uptime  ',(UP ([long]$r.uptime))) @('Gray','White','Gray','White')
  Line @('  Pool    ',$pool,'   ','* UP') @('Gray','White','Gray','Green')
  Line @('  ----------------------------------------------------') @('DarkGray')
  Line @('  HASHRATE   10s ',(HR ([double]$h[0])),'  1m ',(HR ([double]$h[1])),'  15m ',(HR ([double]$h[2]))) @('White','Cyan','Gray','Cyan','Gray','Cyan')
  Line @('  ----------------------------------------------------') @('DarkGray')
  Line @('  SHARES     acc ',("{0:F0}" -f $good),'  rej ',("{0:F0}" -f $rej),'  stale ',("{0:F0}" -f $stale)) @('White','Green','Gray',$rejcol,'Gray','Yellow')
  Line @('             total ',("{0:F0}" -f $total),'  reject% ',('{0:F2}%' -f $rejpct),'  rate ',$rate) @('Gray','White','Gray',$rejcol,'Gray','White')
  Line @('  ----------------------------------------------------') @('DarkGray')
  Line @('  GPU        temp ',$temp,'   power ',$power) @('White','Cyan','Gray','Cyan')
  Line @('  LINK       reconnects ',("{0}" -f $recon),'   failovers ',("{0}" -f $fail)) @('White','Gray','Gray','Gray')
  Line @('  ----------------------------------------------------') @('DarkGray')
  Line @('  EARNINGS   --  (set CSD_POOL_API to enable)') @('White')
  if(-not $once){ Line @('  refresh '+$refresh+'s - Ctrl-C to quit') @('DarkGray') }
}

if($once){ Draw; return }
try { [Console]::CursorVisible=$false } catch {}
try { Clear-Host } catch {}
try {
  while($true){ Draw; Start-Sleep -Seconds $refresh }
} finally {
  try { [Console]::CursorVisible=$true } catch {}
}
