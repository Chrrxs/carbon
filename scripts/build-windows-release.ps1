[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $Version,

	[string] $OutputDir = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Program,

        [Parameter(Mandatory = $true)]
        [string[]] $CommandArguments
    )

    & $Program @CommandArguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Program exited with code $LASTEXITCODE"
    }
}

function Resolve-GitBash {
    $git = Get-Command git.exe -CommandType Application -ErrorAction Stop |
        Select-Object -First 1
    $gitRoot = Split-Path -Parent (Split-Path -Parent $git.Source)
    $candidate = Join-Path $gitRoot "bin\bash.exe"
    if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
        throw "Git Bash was not found at $candidate"
    }
    return $candidate
}

if ($env:OS -ne "Windows_NT") {
    throw "native Windows releases must be built on Windows"
}
if (-not [Environment]::Is64BitOperatingSystem) {
    throw "native Windows releases require an x86_64 host"
}
if ($Version -notmatch '^(0|[1-9][0-9]?)\.(0|[1-9][0-9]?)\.([1-9][0-9]{4,5})$') {
    throw "invalid Carbon release version: $Version"
}

$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..")).ProviderPath
$temporaryRoot = if (-not [string]::IsNullOrWhiteSpace($env:RUNNER_TEMP)) {
    $env:RUNNER_TEMP
} else {
    [IO.Path]::GetTempPath()
}
if ([string]::IsNullOrWhiteSpace($OutputDir)) {
    $OutputDir = Join-Path $repo "target\release-assets"
}

$studioPlugin = Join-Path $repo "target\Carbon-windows.rbxm"
$bash = Resolve-GitBash
$environmentVariableNames = @(
    "CARBON_BUILD_IDENTITY",
    "CARBON_BUILD_VERSION",
	"CARBON_STUDIO_PLUGIN_BUNDLE",
    "Path",
    "INCLUDE",
    "LIB"
)
$previousEnvironment = @{}
foreach ($name in $environmentVariableNames) {
    if (Test-Path -LiteralPath "Env:$name") {
        $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
    }
}

Push-Location $repo
try {
    [Environment]::SetEnvironmentVariable("CARBON_BUILD_IDENTITY", $null, "Process")
    $identityOutput = & $bash "./scripts/build-identity"
    if ($LASTEXITCODE -ne 0) {
        throw "scripts/build-identity exited with code $LASTEXITCODE"
    }
    $buildIdentity = ($identityOutput | Out-String).Trim()
    if ($buildIdentity -notmatch '^0\.0\.0\+build\.[0-9a-f]{12}$') {
        throw "scripts/build-identity produced an invalid identity: $buildIdentity"
    }

	$env:CARBON_BUILD_IDENTITY = $buildIdentity
    $env:CARBON_BUILD_VERSION = $Version
    Invoke-Checked -Program $bash -CommandArguments @(
        "./scripts/build-studio-plugin",
        "target/Carbon-windows.rbxm"
    )
    $env:CARBON_STUDIO_PLUGIN_BUNDLE = $studioPlugin
    Invoke-Checked -Program "cargo.exe" -CommandArguments @(
        "build",
        "--locked",
        "--release",
        "--bin",
        "carbon"
    )
} finally {
    try {
        foreach ($name in $environmentVariableNames) {
            if ($previousEnvironment.ContainsKey($name)) {
                [Environment]::SetEnvironmentVariable($name, $previousEnvironment[$name], "Process")
            } else {
                [Environment]::SetEnvironmentVariable($name, $null, "Process")
            }
        }
    } finally {
        Pop-Location
    }
}

$executable = Join-Path $repo "target\release\carbon.exe"
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
    throw "Cargo did not produce $executable"
}
$versionOutput = & $executable "--color" "never" "--version"
if ($LASTEXITCODE -ne 0) {
    throw "the Windows executable could not report its version"
}
$actualVersion = (($versionOutput | Out-String).Trim() -split '\s+')[-1]
if ($actualVersion -ne $Version) {
    throw "the Windows executable reports $actualVersion, expected $Version"
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$asset = Join-Path $OutputDir "carbon-$Version-windows-x86_64.zip"
if (Test-Path -LiteralPath $asset) {
    Remove-Item -LiteralPath $asset -Force
}
Compress-Archive -LiteralPath $executable -DestinationPath $asset -CompressionLevel Optimal
if (-not (Test-Path -LiteralPath $asset -PathType Leaf)) {
    throw "Windows release archive was not created: $asset"
}

Write-Output $asset
