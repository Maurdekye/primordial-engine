# Starts Primordial Engine for the friend test on Windows (the user's PC), and records this machine's VR setup beside
# its logs: the Windows side of scripts/friend-kit (friend-test S7; S5's firewall check, @coordinator-opus).
#
#   .\run-windows.ps1                    VR, a world of your own on the flat plane (with the spawn panel)
#   .\run-windows.ps1 -Join '<invite>'   VR, joining an invite (it prints a reply code to send back)
#   .\run-windows.ps1 -Headless ...      no window and no VR (a check without the headset)
#   .\run-windows.ps1 -Window            the desktop window, no VR
#
# Any other pe-app option passes through. pe-app.exe is the one beside this script, else -Exe <path>. Each run's logs
# go to %LOCALAPPDATA%\Primordial Engine\friend-test\logs\<time>\ (or $env:PE_KIT_LOGS\<time>).
param(
    [string]$Join,
    [switch]$Headless,
    [switch]$Window,
    [string]$Exe,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Rest
)
$ErrorActionPreference = 'Continue'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $Exe) { $Exe = Join-Path $here 'pe-app.exe' }
if (-not (Test-Path $Exe)) { Write-Host "pe-app.exe not found at $Exe (give -Exe <path>)"; exit 2 }
$Exe = (Resolve-Path $Exe).Path
$root = $env:PE_KIT_LOGS
if (-not $root) { $root = Join-Path $env:LOCALAPPDATA 'Primordial Engine\friend-test\logs' }
$ts = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
$logs = Join-Path $root $ts
New-Item -ItemType Directory -Force -Path $logs | Out-Null

# The firewall (S5): pe-app binds UDP 4433 before VR starts, so Windows' allow prompt shows on the desktop. This PC
# opens the session, so its inbound rules matter. Read only, every run: the enabled inbound rules for this exe, and
# the active networks' categories (@design-reviewer's M1 on S7: a refused prompt leaves BLOCK rules, and "private"
# doesn't cover a network Windows calls Public).
$fw = @()
$rules = @()
try {
    # No filter names an exe Windows has never seen: that's "no rule", not a failure to read.
    $filters = Get-NetFirewallApplicationFilter -Program $Exe -ErrorAction SilentlyContinue
    $rules = @($filters | Get-NetFirewallRule -ErrorAction Stop |
        Where-Object { $_.Direction -eq 'Inbound' -and $_.Enabled -eq 'True' })
    if ($rules.Count -eq 0) { $fw += '  no enabled inbound rule names this pe-app.exe' }
    foreach ($r in $rules) { $fw += "  rule '$($r.DisplayName)': $($r.Action), profiles $($r.Profile)" }
} catch {
    $fw += "  the rules couldn't be read: $($_.Exception.Message)"
}
# Each active network on its own (@design-reviewer's L2 on S7): blocked if an inbound Block rule covers its profile
# (Block overrides Allow), allowed if an Allow rule does, else no rule. Ticking only Private in Windows' prompt makes
# a Public Block rule, which is fine as long as the session goes over a Private network.
function Test-Covers($rule, $category) {
    $p = "$($rule.Profile)"
    $want = if ("$category" -eq 'DomainAuthenticated') { 'Domain' } else { "$category" }
    return ($p -eq 'Any') -or ($p -split ',\s*' -contains $want)
}
$allowedNets = @()
$blockedNets = @()
foreach ($n in Get-NetConnectionProfile -ErrorAction SilentlyContinue) {
    $c = $n.NetworkCategory
    $verdict = 'no rule'
    if ($rules | Where-Object { "$($_.Action)" -eq 'Allow' -and (Test-Covers $_ $c) }) { $verdict = 'allowed' }
    if ($rules | Where-Object { "$($_.Action)" -eq 'Block' -and (Test-Covers $_ $c) }) { $verdict = 'blocked' }
    $line = "$($n.InterfaceAlias) ($c, '$($n.Name)'): $verdict"
    $fw += "  network $line"
    if ($verdict -eq 'allowed') { $allowedNets += $line } elseif ($verdict -eq 'blocked') { $blockedNets += $line }
}
$fwOk = $allowedNets.Count -gt 0
if (-not $fwOk) {
    Write-Host ''
    Write-Host 'FIREWALL: no network this PC is on lets the friend in to this pe-app.exe yet.'
    if ($blockedNets.Count -gt 0) {
        Write-Host 'Blocked on:'
        $blockedNets | ForEach-Object { Write-Host "  $_" }
        Write-Host 'Allow it: Windows Security > Firewall & network protection > Allow an app through firewall.'
    } else {
        Write-Host 'When Windows asks whether Primordial Engine may use the network, allow it, BEFORE putting the'
        Write-Host 'headset on, for the type of the network the session goes over (listed below).'
    }
    $fw | ForEach-Object { Write-Host $_ }
    Write-Host ''
}

# What this machine has for VR, before anything starts: Windows, the GPU and its driver, the OpenXR runtime.
$report = @()
$report += "== run $ts, exe $Exe"
$report += "commit: " + ((Get-Content (Join-Path $here 'COMMIT') -ErrorAction SilentlyContinue) -join ' ')
$os = Get-CimInstance Win32_OperatingSystem
$report += "== system"
$report += "$($os.Caption) $($os.Version)"
$report += "== GPU"
foreach ($g in Get-CimInstance Win32_VideoController) { $report += "$($g.Name), driver $($g.DriverVersion)" }
$report += "== OpenXR runtime"
$active = (Get-ItemProperty -Path 'HKLM:\SOFTWARE\Khronos\OpenXR\1' -Name ActiveRuntime -ErrorAction SilentlyContinue).ActiveRuntime
$report += "ActiveRuntime: $active"
if ($active -and (Test-Path $active)) {
    $json = Get-Content $active -Raw | ConvertFrom-Json
    $report += "  name: $($json.runtime.name), library: $($json.runtime.library_path)"
}
$report += "XR_RUNTIME_JSON=$env:XR_RUNTIME_JSON"
$report += "== firewall: a network lets the friend in: $fwOk"
$report += $fw
$report | Set-Content -Path (Join-Path $logs 'system.txt') -Encoding utf8
$report | ForEach-Object { Write-Host $_ }

$env:PE_XR_LOG = '1'
$env:PE_CUT_LOG = Join-Path $logs 'cuts.txt'
$env:PE_TICK_TRACE = Join-Path $logs 'trace.txt'
# No script toolchain: both machines' build ids must say the same (@engine-architect).
Remove-Item Env:CARGO -ErrorAction SilentlyContinue

$argv = @()
if (-not $Headless -and -not $Window) { $argv += '--vr' }
if ($Headless) { $argv += '--headless' }
# The friend test's plane, also when joining (pe-app ignores it there), so the menu's relaunches keep it.
if (-not ($Rest -contains '--preset')) { $argv += @('--preset', 'plane') }
if ($Join) { $argv += @('--join', $Join) }
$argv += @('--log', (Join-Path $logs 'pe-app.log'))
if ($Rest) { $argv += $Rest }
Write-Host "== pe-app $($argv -join ' ') (logs: $logs)"
# Start-Process joins its arguments with spaces and quotes nothing, so quote each one that needs it.
$quoted = ($argv | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }) -join ' '
$p = Start-Process -FilePath $Exe -ArgumentList $quoted -NoNewWindow -Wait -PassThru `
    -RedirectStandardError (Join-Path $logs 'stderr.txt')
Write-Host "pe-app ended ($($p.ExitCode)). Logs: $logs"
Write-Host '(If it restarted itself from the menu, the new one is still running and writes to the same folder.)'
exit $p.ExitCode
