param(
    [Parameter(Mandatory = $true)]
    [string] $BuildDir,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[A-Za-z0-9_.+-]+$')]
    [string] $Target,

    [string] $SourceDir = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Normalize-WindowsPathSpelling {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $normalized = [IO.Path]::GetFullPath($Path.Replace('/', '\')).TrimEnd('\')
    if ($normalized -match '^\\\\wsl(?:\.localhost|\$)\\([^\\]+)(.*)$') {
        return "\\wsl.localhost\$($Matches[1].ToLowerInvariant())$($Matches[2])"
    }
    return $normalized
}

function Resolve-VisualStudioRoot {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        throw "Visual Studio locator was not found: $vswhere"
    }
    $installationPath = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath |
        Select-Object -First 1
    if ([string]::IsNullOrWhiteSpace($installationPath)) {
        throw "Visual Studio with the x64 C++ toolchain was not found"
    }
    return $installationPath
}

function Initialize-MsvcEnvironment {
    param(
        [Parameter(Mandatory = $true)]
        [string] $VisualStudioRoot
    )

    if ($null -ne (Get-Command cl.exe -CommandType Application -ErrorAction SilentlyContinue) -and
        -not [string]::IsNullOrWhiteSpace($env:INCLUDE) -and
        -not [string]::IsNullOrWhiteSpace($env:LIB)) {
        return
    }

    $devCmd = Join-Path $VisualStudioRoot "Common7\Tools\VsDevCmd.bat"
    if (-not (Test-Path -LiteralPath $devCmd -PathType Leaf)) {
        throw "Visual Studio developer environment script was not found: $devCmd"
    }
    $environmentLines = & $env:ComSpec /d /s /c `
        "call `"$devCmd`" -no_logo -arch=x64 >nul && set"
    if ($LASTEXITCODE -ne 0) {
        throw "Visual Studio developer environment initialization failed"
    }
    foreach ($line in $environmentLines) {
        $separator = $line.IndexOf('=')
        if ($separator -le 0) {
            continue
        }
        [Environment]::SetEnvironmentVariable(
            $line.Substring(0, $separator),
            $line.Substring($separator + 1),
            [EnvironmentVariableTarget]::Process)
    }
    if ($null -eq (Get-Command cl.exe -CommandType Application -ErrorAction SilentlyContinue) -or
        [string]::IsNullOrWhiteSpace($env:INCLUDE) -or
        [string]::IsNullOrWhiteSpace($env:LIB)) {
        throw "Visual Studio developer environment is incomplete"
    }
}

function Resolve-CMake {
    param(
        [Parameter(Mandatory = $true)]
        [string] $VisualStudioRoot
    )

    if (-not [string]::IsNullOrWhiteSpace($env:CARBON_QUALIFY_CMAKE)) {
        if (-not (Test-Path -LiteralPath $env:CARBON_QUALIFY_CMAKE -PathType Leaf)) {
            throw "CARBON_QUALIFY_CMAKE does not exist: $env:CARBON_QUALIFY_CMAKE"
        }
        return (Resolve-Path -LiteralPath $env:CARBON_QUALIFY_CMAKE).Path
    }
    $candidate = Join-Path $VisualStudioRoot `
        "Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe"
    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
        return $candidate
    }
    $command = Get-Command cmake.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -ne $command) {
        return $command.Source
    }
    throw "CMake was not found in Visual Studio or on PATH; set CARBON_QUALIFY_CMAKE to cmake.exe"
}

function Get-CMakeCacheValue {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Cache,

        [Parameter(Mandatory = $true)]
        [string] $Key
    )

    $prefix = "${Key}:"
    $line = [IO.File]::ReadLines($Cache) |
        Where-Object { $_.StartsWith($prefix, [StringComparison]::Ordinal) } |
        Select-Object -First 1
    if ($null -eq $line) {
        throw "CMake cache is missing $Key`: $Cache"
    }
    return $line.Substring($line.IndexOf('=') + 1)
}

$resolvedBuildDir = if (Test-Path -LiteralPath $BuildDir) {
    (Resolve-Path -LiteralPath $BuildDir).ProviderPath
} else {
    [IO.Path]::GetFullPath($BuildDir)
}
$BuildDir = Normalize-WindowsPathSpelling $resolvedBuildDir
$cache = Join-Path $BuildDir "CMakeCache.txt"
if (-not (Test-Path -LiteralPath $cache -PathType Leaf)) {
    throw "persistent RML CMake build directory is not configured: $BuildDir"
}
$generator = Get-CMakeCacheValue -Cache $cache -Key "CMAKE_GENERATOR"
if ($generator -ne "Ninja") {
    throw "persistent RML CMake cache uses '$generator', expected Ninja: $cache"
}
if ([string]::IsNullOrWhiteSpace($SourceDir)) {
    $SourceDir = Join-Path $PSScriptRoot "..\rml"
}
$expectedSource = Normalize-WindowsPathSpelling (
    (Resolve-Path -LiteralPath $SourceDir).ProviderPath)
$cachedSource = Normalize-WindowsPathSpelling (
    Get-CMakeCacheValue -Cache $cache -Key "CMAKE_HOME_DIRECTORY")
if (-not $cachedSource.Equals($expectedSource, [StringComparison]::Ordinal)) {
    throw "persistent RML CMake cache source mismatch: cached '$cachedSource', expected '$expectedSource'. Configure a fresh build directory."
}
if (-not (Test-Path -LiteralPath (Join-Path $cachedSource "CMakeLists.txt") -PathType Leaf)) {
    throw "persistent RML CMake cache source is unavailable: $cachedSource"
}

$visualStudioRoot = Resolve-VisualStudioRoot
Initialize-MsvcEnvironment -VisualStudioRoot $visualStudioRoot
$cmake = Resolve-CMake -VisualStudioRoot $visualStudioRoot

& $cmake --build $BuildDir --config Release --target $Target
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
