#Requires -RunAsAdministrator
$ErrorActionPreference = 'Stop'

Write-Host ""
Write-Host "=== Disable older AMD OpenCL ICD (3652 / oem38) ===" -ForegroundColor Cyan
Write-Host ""

$ghostAmdOcl = 'SWD\DRIVERENUM\AMDOCL&5&22f726a&0'
$activeAmdOcl = 'SWD\DRIVERENUM\AMDOCL&7&200dbde&0'

Write-Host "--- Before ---"
pnputil /enum-devices /instanceid $ghostAmdOcl 2>&1
pnputil /enum-devices /instanceid $activeAmdOcl 2>&1

$bin = Join-Path $PSScriptRoot '..\..\target\release'
$diag = Join-Path $bin 'diagnose_opencl.exe'
if (Test-Path $diag) {
    & $diag --report (Join-Path $bin 'diagnose-opencl-before.json') 2>&1
}

Write-Host ""
Write-Host "--- Remove ghost AMD-OpenCL device (oem38 / AMDOCL-25.10) ---"
try {
    pnputil /remove-device $ghostAmdOcl 2>&1
} catch {
    Write-Warning "pnputil remove-device failed: $_"
}

try {
    Disable-PnpDevice -InstanceId $ghostAmdOcl -Confirm:$false -ErrorAction SilentlyContinue
} catch {}

Write-Host ""
Write-Host "--- Delete stale amdocl driver packages (keep oem72 = 3679) ---"
$deletePkgs = @('oem38.inf', 'oem4.inf', 'oem88.inf', 'oem101.inf', 'oem110.inf', 'oem113.inf')
foreach ($pkg in $deletePkgs) {
    Write-Host "Deleting $pkg ..."
    pnputil /delete-driver $pkg /uninstall /force 2>&1
}

Write-Host ""
Write-Host "--- After ---"
pnputil /enum-devices /class SoftwareComponent 2>&1 | Select-String -Pattern 'AMDOCL|Driver Name' -Context 0,1
if (Test-Path $diag) {
    & $diag --report (Join-Path $bin 'diagnose-opencl-after.json') 2>&1
}

Write-Host ""
Write-Host "Done. Reboot recommended, then run DIAGNOSE-AMD-GPU.bat" -ForegroundColor Green
Write-Host ""