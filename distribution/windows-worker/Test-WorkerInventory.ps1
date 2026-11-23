#requires -Version 5.1
[CmdletBinding()]
param(
    [ValidateSet('Consume', 'Contribute')]
    [string]$Mode = 'Contribute',
    [UInt64]$RequiredPackageVolumeBytes = 0
)

$ErrorActionPreference = 'Stop'
if ($Mode -eq 'Consume') {
    [ordered]@{
        evidence_version = 2
        mode = 'consume'
        observed_utc = [DateTime]::UtcNow.ToString('o')
        local_worker_required = $false
        wsl_required = $false
    } | ConvertTo-Json
    exit 0
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'Windows x64 is required for this target-machine check.'
}

$manifest = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'manifest.json') -Raw | ConvertFrom-Json
$exe = Join-Path $PSScriptRoot 'bin\orange-worker.exe'
$expected = $manifest.sha256.'bin/orange-worker.exe'
$actual = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
if ($manifest.target -ne 'x86_64-pc-windows-gnullvm' -or $actual -cne $expected) {
    throw 'The executable does not match the packaged target and SHA-256.'
}

# Verify the app-local loader dependencies before starting the executable.
$bin = Split-Path -LiteralPath $exe
$expectedDlls = @($manifest.sha256.PSObject.Properties | Where-Object { $_.Name -like 'bin/*.dll' })
foreach ($dll in $expectedDlls) {
    if ($dll.Name -cne 'bin/libunwind.dll') { throw 'Unknown packaged loader dependency.' }
    $path = Join-Path $bin 'libunwind.dll'
    if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $dll.Value) {
        throw 'Bundled runtime SHA-256 mismatch.'
    }
}
if (@(Get-ChildItem -LiteralPath $bin -Filter '*.dll' -File).Count -ne $expectedDlls.Count) {
    throw 'Unexpected DLL in the executable directory.'
}

function Get-OptionalFeatureObservation([string]$Name) {
    try {
        $feature = Get-CimInstance -ClassName Win32_OptionalFeature -Filter "Name = '$Name'" -OperationTimeoutSec 5
        if ($null -eq $feature) { return 'not_reported' }
        switch ($feature.InstallState) {
            1 { return 'enabled' }
            2 { return 'disabled' }
            3 { return 'absent' }
            default { return 'unknown' }
        }
    } catch {
        return 'query_unavailable'
    }
}

$observations = [ordered]@{}
try {
    $os = Get-CimInstance -ClassName Win32_OperatingSystem -Property Version, BuildNumber, OSArchitecture -OperationTimeoutSec 5
    $observations.os_version = $os.Version
    $observations.build_number = $os.BuildNumber
    $observations.os_architecture = $os.OSArchitecture
} catch {
    $observations.os_query = 'unavailable'
}
try {
    $machine = Get-CimInstance -ClassName Win32_ComputerSystem -Property HypervisorPresent -OperationTimeoutSec 5
    $observations.hypervisor_present = $machine.HypervisorPresent
} catch {
    $observations.hypervisor_present = $null
}
try {
    $volume = [IO.DriveInfo]::new([IO.Path]::GetPathRoot($PSScriptRoot))
    $observations.package_volume_free_bytes = $volume.AvailableFreeSpace.ToString([CultureInfo]::InvariantCulture)
} catch {
    $observations.package_volume_free_bytes = $null
}
$observations.required_package_volume_bytes = $RequiredPackageVolumeBytes.ToString([CultureInfo]::InvariantCulture)
$observations.package_volume_sufficient = if ($RequiredPackageVolumeBytes -eq 0 -or $null -eq $observations.package_volume_free_bytes) {
    $null
} else { [UInt64]$observations.package_volume_free_bytes -ge $RequiredPackageVolumeBytes }
$observations.wsl_optional_feature = Get-OptionalFeatureObservation 'Microsoft-Windows-Subsystem-Linux'
$observations.virtual_machine_platform_feature = Get-OptionalFeatureObservation 'VirtualMachinePlatform'

$wslLines = & $exe probe-wsl
$wslCode = $LASTEXITCODE
$wsl = ($wslLines -join [Environment]::NewLine) | ConvertFrom-Json
if ($wsl.version -ne 1 -or $wslCode -notin @(0, 2) -or
    ($wslCode -eq 0 -and ($wsl.status -ne 'observed' -or $null -eq $wsl.distributions)) -or
    ($wslCode -eq 2 -and ($wsl.status -eq 'observed' -or $null -ne $wsl.distributions))) {
    throw 'Unexpected WSL observation.'
}
$observations.wsl_registrations = $wsl
$outputLines = & $exe probe-gpu
$status = $LASTEXITCODE
$inventory = ($outputLines -join [Environment]::NewLine) | ConvertFrom-Json
if ($inventory.version -ne 1 -or $inventory.os -ne 'windows' -or $inventory.architecture -ne 'x86_64') {
    throw 'The executing Worker did not return the expected Windows inventory schema.'
}
if (($status -eq 0 -and ($inventory.status -ne 'observed' -or @($inventory.devices).Count -lt 1)) -or
    ($status -eq 2 -and ($inventory.status -eq 'observed' -or @($inventory.devices).Count -ne 0)) -or
    ($status -notin @(0, 2))) {
    throw 'Unexpected inventory status or device count.'
}

[ordered]@{
    evidence_version = 2
    mode = 'contribute'
    source = $manifest.source
    executable_sha256 = $actual
    observed_utc = [DateTime]::UtcNow.ToString('o')
    environment = $observations
    inventory = $inventory
    inference_verified = $false
    reclamation_verified = $false
} | ConvertTo-Json -Depth 7
exit $status
