[CmdletBinding()]
param(
    [switch]$Force
)

$ErrorActionPreference = "Stop"
$externalRoot = Join-Path $PSScriptRoot "external"
$manifestPath = Join-Path $PSScriptRoot "external-tools.json"
$manifest = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
$dotnetRoot = Join-Path $externalRoot "dotnet10"
$dotnetExe = Join-Path $dotnetRoot "dotnet.exe"
$cue4parseRoot = Join-Path $externalRoot "CUE4Parse"
$probeProject = Join-Path $PSScriptRoot "cue4parse_probe\Cue4ParseProbe.csproj"
$probeDll = Join-Path $PSScriptRoot "cue4parse_probe\bin\Release\net10.0\Cue4ParseProbe.dll"

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)]
        [scriptblock]$Command,
        [Parameter(Mandatory = $true)]
        [string]$Description
    )
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE"
    }
}

New-Item -ItemType Directory -Force -Path $externalRoot | Out-Null

if ($Force -or -not (Test-Path -LiteralPath $dotnetExe)) {
    $installer = Join-Path $externalRoot "dotnet-install.ps1"
    Invoke-WebRequest `
        -UseBasicParsing `
        -Uri "https://dot.net/v1/dotnet-install.ps1" `
        -OutFile $installer
    & $installer `
        -Version ([string]$manifest.dotnet.version) `
        -InstallDir $dotnetRoot `
        -NoPath
}

$installedVersion = (& $dotnetExe --version).Trim()
if ($installedVersion -ne [string]$manifest.dotnet.version) {
    throw "dotnet version mismatch: expected $($manifest.dotnet.version), got $installedVersion"
}

if ($Force -and (Test-Path -LiteralPath $cue4parseRoot)) {
    $resolvedExternal = (Resolve-Path -LiteralPath $externalRoot).Path
    $resolvedCue4Parse = (Resolve-Path -LiteralPath $cue4parseRoot).Path
    if (-not $resolvedCue4Parse.StartsWith($resolvedExternal, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to remove CUE4Parse outside tools/external"
    }
    Remove-Item -LiteralPath $resolvedCue4Parse -Recurse -Force
}

if (-not (Test-Path -LiteralPath (Join-Path $cue4parseRoot ".git"))) {
    Invoke-Checked `
        { git clone $manifest.cue4parse.repository $cue4parseRoot } `
        "CUE4Parse clone"
}

$safeDirectory = $cue4parseRoot.Replace("\", "/")
Invoke-Checked `
    { git -c "safe.directory=$safeDirectory" -C $cue4parseRoot fetch --depth 1 origin $manifest.cue4parse.commit } `
    "CUE4Parse fetch"
Invoke-Checked `
    { git -c "safe.directory=$safeDirectory" -C $cue4parseRoot checkout --detach $manifest.cue4parse.commit } `
    "CUE4Parse checkout"

$actualCommit = (
    git -c "safe.directory=$safeDirectory" -C $cue4parseRoot rev-parse HEAD
).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Unable to read CUE4Parse commit"
}
if ($actualCommit -ne [string]$manifest.cue4parse.commit) {
    throw "CUE4Parse commit mismatch: expected $($manifest.cue4parse.commit), got $actualCommit"
}

& $dotnetExe restore $probeProject
if ($LASTEXITCODE -ne 0) {
    Write-Warning "Online NuGet restore failed; retrying with local caches."
    Invoke-Checked {
        & $dotnetExe restore $probeProject --ignore-failed-sources
    } "Probe restore"
}
Invoke-Checked {
    & $dotnetExe build $probeProject -c Release --no-restore
} "Probe build"

if (-not (Test-Path -LiteralPath $probeDll)) {
    throw "Probe build completed without expected output: $probeDll"
}

Write-Host ""
Write-Host "Resource export tools are ready."
Write-Host "dotnet: $installedVersion"
Write-Host "CUE4Parse: $actualCommit"
Write-Host "probe: $probeDll"
