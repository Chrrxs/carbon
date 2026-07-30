param(
    [Parameter(Mandatory = $true)]
    [string] $BuildDir,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[A-Za-z0-9_.+-]+$')]
    [string] $Target
)

$ErrorActionPreference = "Stop"
$cache = Join-Path $BuildDir "CMakeCache.txt"
if (-not (Test-Path -LiteralPath $cache -PathType Leaf)) {
    throw "persistent RML CMake build directory is not configured: $BuildDir"
}

& cmake --build $BuildDir --config Release --target $Target
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
