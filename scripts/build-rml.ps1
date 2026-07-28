[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $SourceDir,

    [Parameter(Mandatory = $true)]
    [string] $BuildDir,

    [Parameter(Mandatory = $true)]
    [string] $PackageDir,

    [Parameter(Mandatory = $true)]
    [string] $BuildVersion
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Program,

        [Parameter(ValueFromRemainingArguments = $true)]
        [string[]] $Arguments
    )

    $quotedArguments = foreach ($argument in $Arguments) {
        if ($argument.Length -gt 0 -and $argument -notmatch '[\s"]') {
            $argument
            continue
        }

        # Start-Process on Windows PowerShell 5.1 accepts one command-line
        # string. Apply the CommandLineToArgvW quoting rules so paths and
        # generator names containing spaces survive that round trip.
        $quoted = [regex]::Replace($argument, '(\\*)("|$)', {
            param($match)
            $slashes = $match.Groups[1].Value
            if ($match.Groups[2].Value -eq '"') {
                return ($slashes + $slashes + '\"')
            }
            return ($slashes + $slashes)
        })
        '"' + $quoted + '"'
    }

    $process = Start-Process `
        -FilePath $Program `
        -ArgumentList ($quotedArguments -join ' ') `
        -NoNewWindow `
        -PassThru
    # Start-Process -Wait can wait on unrelated descendants left behind by
    # Visual Studio's developer-environment scripts. Wait only for the command
    # process that this helper owns.
    # Accessing Handle first works around Windows PowerShell 5.1 returning a
    # blank ExitCode for a process that exits before WaitForExit observes it.
    $null = $process.Handle
    $process.WaitForExit()
    $process.Refresh()
    if ($process.ExitCode -ne 0) {
        throw "$Program exited with code $($process.ExitCode)"
    }
}

function Resolve-VSWhere {
    $programFilesX86 = [Environment]::GetFolderPath("ProgramFilesX86")
    $candidate = Join-Path $programFilesX86 "Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
        return $candidate
    }

    $command = Get-Command vswhere.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -ne $command) {
        return $command.Source
    }

    return $null
}

function Resolve-VisualStudioRoot {
    $vswhere = Resolve-VSWhere
    if ($null -ne $vswhere) {
        $installations = @(& $vswhere `
            -latest `
            -products "*" `
            -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            -property installationPath)
        foreach ($candidate in $installations) {
            if (-not [string]::IsNullOrWhiteSpace($candidate) -and
                (Test-Path -LiteralPath (Join-Path $candidate "VC\Tools\MSVC") -PathType Container)) {
                return $candidate
            }
        }
    }

    $programFilesRoots = @(
        [Environment]::GetFolderPath("ProgramFiles"),
        [Environment]::GetFolderPath("ProgramFilesX86")
    ) | Select-Object -Unique
    foreach ($programFilesRoot in $programFilesRoots) {
        foreach ($version in @("18", "17")) {
            foreach ($edition in @("BuildTools", "Community", "Professional", "Enterprise")) {
                $candidate = Join-Path $programFilesRoot "Microsoft Visual Studio\$version\$edition"
                if (Test-Path -LiteralPath (Join-Path $candidate "VC\Tools\MSVC") -PathType Container) {
                    return $candidate
                }
            }
        }
    }
    throw "Visual Studio C++ build tools were not found"
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

    $candidate = Join-Path $VisualStudioRoot "Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe"
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

function Resolve-Ninja {
    param(
        [Parameter(Mandatory = $true)]
        [string] $VisualStudioRoot
    )

    $candidate = Join-Path $VisualStudioRoot "Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja\ninja.exe"
    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
        return $candidate
    }

    $command = Get-Command ninja.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -ne $command) {
        return $command.Source
    }

    throw "Ninja was not found in Visual Studio or on PATH"
}

function ConvertTo-LocalDrivePath {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    $resolved = if (Test-Path -LiteralPath $Path) {
        (Resolve-Path -LiteralPath $Path).ProviderPath
    } else {
        $Path
    }
    foreach ($drive in Get-PSDrive -PSProvider FileSystem) {
        if ([string]::IsNullOrWhiteSpace($drive.DisplayRoot)) {
            continue
        }
        $root = $drive.DisplayRoot.TrimEnd('\')
        if ($resolved.StartsWith($root, [StringComparison]::OrdinalIgnoreCase)) {
            return (Join-Path $drive.Root $resolved.Substring($root.Length).TrimStart('\'))
        }
    }
    return $resolved
}

function Find-OneBuildFile {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Root,

        [Parameter(Mandatory = $true)]
        [string] $Name,

        [string] $RequiredPathFragment = ""
    )

    $files = Get-ChildItem -LiteralPath $Root -Recurse -File -Filter $Name |
        Where-Object {
            $_.FullName -notmatch "[\\/]_deps[\\/]" -and
            $_.FullName -notmatch "[\\/]qualification-stage[\\/]" -and
            ($RequiredPathFragment -eq "" -or $_.FullName -like "*$RequiredPathFragment*")
        } |
        Sort-Object LastWriteTimeUtc -Descending
    if (-not $files) {
        throw "$Name was not produced beneath $Root"
    }
    return $files[0].FullName
}

$SourceDir = ConvertTo-LocalDrivePath $SourceDir
$BuildDir = ConvertTo-LocalDrivePath $BuildDir
$PackageDir = ConvertTo-LocalDrivePath $PackageDir
if ($BuildVersion -notmatch '^[0-9]+\.[0-9]+\.[0-9]+([+-][0-9A-Za-z.-]+)?$') {
    throw "invalid automatic build version: $BuildVersion"
}
$env:CARBON_BUILD_VERSION = $BuildVersion
if ((Split-Path -Leaf $BuildDir) -ne "carbon-rml-build" -and (Split-Path -Leaf $BuildDir) -ne "qualification-fresh-rml") {
    throw "refusing to clean unexpected RML build directory: $BuildDir"
}
New-Item -ItemType Directory -Force -Path $BuildDir | Out-Null

$visualStudioRoot = Resolve-VisualStudioRoot
$cmake = Resolve-CMake -VisualStudioRoot $visualStudioRoot
$ninja = Resolve-Ninja -VisualStudioRoot $visualStudioRoot
$windowsSdkBin = Get-ChildItem -LiteralPath (Join-Path ([Environment]::GetFolderPath("ProgramFilesX86")) "Windows Kits\10\bin") -Directory |
    Where-Object {
        Test-Path -LiteralPath (Join-Path $_.FullName "x64\rc.exe") -PathType Leaf
    } |
    Sort-Object Name -Descending |
    Select-Object -First 1
if (-not $windowsSdkBin) {
    throw "a Windows 10/11 x64 SDK was not found"
}
$resourceCompiler = (Join-Path $windowsSdkBin.FullName "x64\rc.exe").Replace('\', '/')
$manifestTool = (Join-Path $windowsSdkBin.FullName "x64\mt.exe").Replace('\', '/')
$windowsSdkRoot = Split-Path -Parent (Split-Path -Parent $windowsSdkBin.FullName)
$windowsSdkVersion = $windowsSdkBin.Name
$vcTools = Get-ChildItem -LiteralPath (Join-Path $visualStudioRoot "VC\Tools\MSVC") -Directory |
    Where-Object {
        Test-Path -LiteralPath (Join-Path $_.FullName "bin\Hostx64\x64\cl.exe") -PathType Leaf
    } |
    Sort-Object Name -Descending |
    Select-Object -First 1
if (-not $vcTools) {
    throw "the x64 MSVC compiler was not found beneath $visualStudioRoot"
}

# A partially registered Build Tools installation can make VsDevCmd fail even
# when the complete compiler and SDK are present. Construct the documented
# MSVC/Windows SDK environment directly from their versioned install roots.
$windowsSystem = Join-Path ([Environment]::GetFolderPath("Windows")) "System32"
$env:Path = @(
    (Join-Path $vcTools.FullName "bin\Hostx64\x64"),
    (Join-Path $windowsSdkBin.FullName "x64"),
    $windowsSystem,
    $env:Path
) -join ';'
$env:INCLUDE = @(
    (Join-Path $vcTools.FullName "include"),
    (Join-Path $windowsSdkRoot "Include\$windowsSdkVersion\ucrt"),
    (Join-Path $windowsSdkRoot "Include\$windowsSdkVersion\shared"),
    (Join-Path $windowsSdkRoot "Include\$windowsSdkVersion\um"),
    (Join-Path $windowsSdkRoot "Include\$windowsSdkVersion\winrt"),
    (Join-Path $windowsSdkRoot "Include\$windowsSdkVersion\cppwinrt")
) -join ';'
$env:LIB = @(
    (Join-Path $vcTools.FullName "lib\x64"),
    (Join-Path $windowsSdkRoot "Lib\$windowsSdkVersion\ucrt\x64"),
    (Join-Path $windowsSdkRoot "Lib\$windowsSdkVersion\um\x64")
) -join ';'
Write-Host "[fresh-rml] Ensuring an isolated .NET 10 SDK"
$dotnetSdkVersion = "10.0.302"
$dotnetSdkArchiveName = "dotnet-sdk-10.0.302-win-x64.zip"
$dotnetSdkArchive = Join-Path $BuildDir $dotnetSdkArchiveName
$dotnetSdkUrl = "https://builds.dotnet.microsoft.com/dotnet/Sdk/$dotnetSdkVersion/$dotnetSdkArchiveName"
$dotnetSdkSha512 = "7d170ed75fa9af34c00646621d92011dbd71943952e2787cd15df9be78e6452b55dadef34d7eff77b802e6af4959e071a55855ac649afeac70901c3a2a258716"
$dotnetRoot = Join-Path $BuildDir "dotnet-sdk"
$dotnet = Join-Path $dotnetRoot "dotnet.exe"
if (-not (Test-Path -LiteralPath $dotnet -PathType Leaf)) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    Write-Host "[fresh-rml] Downloading pinned .NET SDK $dotnetSdkVersion"
    Invoke-WebRequest -UseBasicParsing -Uri $dotnetSdkUrl -OutFile $dotnetSdkArchive
    $sha512 = [Security.Cryptography.SHA512]::Create()
    $archiveStream = [IO.File]::OpenRead($dotnetSdkArchive)
    try {
        $actualDotnetSdkSha512 = ([BitConverter]::ToString($sha512.ComputeHash($archiveStream))).Replace("-", "").ToLowerInvariant()
    }
    finally {
        $archiveStream.Dispose()
        $sha512.Dispose()
    }
    if ($actualDotnetSdkSha512 -ne $dotnetSdkSha512) {
        throw "pinned .NET SDK archive hash mismatch: expected $dotnetSdkSha512, got $actualDotnetSdkSha512"
    }
    New-Item -ItemType Directory -Force -Path $dotnetRoot | Out-Null
    Expand-Archive -LiteralPath $dotnetSdkArchive -DestinationPath $dotnetRoot
    Remove-Item -LiteralPath $dotnetSdkArchive -Force
}
if (-not (Test-Path -LiteralPath $dotnet -PathType Leaf)) {
    throw "the isolated .NET 10 SDK installation did not produce dotnet.exe"
}
$installedDotnetSdk = Join-Path $dotnetRoot "sdk\$dotnetSdkVersion"
if (-not (Test-Path -LiteralPath $installedDotnetSdk -PathType Container)) {
    throw "isolated .NET SDK version directory is missing: $installedDotnetSdk"
}
$env:Path = "$dotnetRoot;$env:Path"

Write-Host "[fresh-rml] Configuring native loader with MSVC and Ninja"
Invoke-Checked -Program $cmake -Arguments @(
    "-S", $SourceDir,
    "-B", $BuildDir,
    "-G", "Ninja",
    "-DCMAKE_BUILD_TYPE=Release",
    "-DCMAKE_MAKE_PROGRAM=$ninja",
    "-DCMAKE_RC_COMPILER=$resourceCompiler",
    "-DCMAKE_MT=$manifestTool",
    "-DROBLOX_MODLOADER_BUILD_DUMPER=OFF",
    "-DROBLOX_MODLOADER_BUILD_EXAMPLES=OFF",
    "-DROBLOX_MODLOADER_BUILD_MANAGED_PROJECTS=OFF",
    "-DROBLOX_MODLOADER_USE_CMAKE_CSHARP=OFF",
    "-DRML_FORCE_INCLUDE_COMMON_WITHOUT_PCH=OFF",
    "-DCARBON_BUILD_VERSION=$BuildVersion",
    "-DBUILD_TESTING=OFF"
)
Invoke-Checked -Program $cmake -Arguments @("--build", $BuildDir, "--parallel", "--target", "roblox_modloader")

Write-Host "[fresh-rml] Compiling Carbon Studio helper"
$helperSource = Join-Path $PSScriptRoot "carbon-studio-helper.cpp"
if (-not (Test-Path -LiteralPath $helperSource -PathType Leaf)) {
    throw "carbon-studio-helper.cpp was not found: $helperSource"
}
$clCompiler = Join-Path $vcTools.FullName "bin\Hostx64\x64\cl.exe"
$helperBuildDir = Join-Path $BuildDir "helper"
New-Item -ItemType Directory -Force -Path $helperBuildDir | Out-Null
$helperObj = Join-Path $helperBuildDir "carbon-studio-helper.obj"
$helperOut = Join-Path $helperBuildDir "carbon-studio-helper.exe"
Invoke-Checked -Program $clCompiler -Arguments @(
    "/nologo", "/O2", "/MT", "/std:c++17", "/EHsc", "/W4", "/WX",
    "/Fo:$helperObj", "/Fe:$helperOut", $helperSource,
    "/link", "/Brepro", "/INCREMENTAL:NO", "kernel32.lib", "user32.lib", "advapi32.lib"
)

Write-Host "[fresh-rml] Testing the complete managed runtime"
$bridgeTests = Join-Path $SourceDir "code\dotnet\CarbonBridge.Tests\CarbonBridge.Tests.csproj"
$runtimeTests = Join-Path $SourceDir "code\dotnet\Runtime\Runtime.slnx"
$robloxTests = Join-Path $SourceDir "code\dotnet\Roblox\Roblox.slnx"
$managedArtifacts = Join-Path $BuildDir "managed-artifacts"
Invoke-Checked -Program $dotnet -Arguments @(
    "test", $runtimeTests, "-c", "Release", "--nologo",
    "--artifacts-path", $managedArtifacts
)
Invoke-Checked -Program $dotnet -Arguments @(
    "test", $robloxTests, "-c", "Release", "--nologo",
    "--artifacts-path", $managedArtifacts
)
Invoke-Checked -Program $dotnet -Arguments @(
    "test", $bridgeTests, "-c", "Release", "--nologo",
    "--artifacts-path", $managedArtifacts
)

$stage = Join-Path $BuildDir "qualification-stage"
$stageRml = Join-Path $stage "RobloxModLoader"
$stageRuntime = Join-Path $stageRml "runtime"
$stageBridge = Join-Path $stageRml "mods\carbon\dotnet"
$stageLibraries = Join-Path $stageRml "libraries"
$thirdPartyNotices = Join-Path $SourceDir "THIRD_PARTY_NOTICES.md"
if (Test-Path -LiteralPath $stage) {
    Remove-Item -LiteralPath $stage -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $stageRuntime, $stageBridge, $stageLibraries | Out-Null
if (-not (Test-Path -LiteralPath $thirdPartyNotices -PathType Leaf)) {
    throw "third-party notices are missing: $thirdPartyNotices"
}
Copy-Item -LiteralPath $thirdPartyNotices -Destination (Join-Path $stageRml "THIRD_PARTY_NOTICES.md") -Force

Write-Host "[fresh-rml] Publishing managed runtime and Carbon bridge"
Invoke-Checked -Program $dotnet -Arguments @(
    "publish",
    (Join-Path $SourceDir "code\dotnet\Runtime\src\RML.Core\RML.Core.csproj"),
    "-c", "Release", "-o", $stageRuntime, "--nologo",
    "--artifacts-path", $managedArtifacts
)
Invoke-Checked -Program $dotnet -Arguments @(
    "publish",
    (Join-Path $SourceDir "code\dotnet\Runtime\src\RML.NativeHost\RML.NativeHost.csproj"),
    "-c", "Release", "-o", $stageRuntime, "--nologo",
    "--artifacts-path", $managedArtifacts
)
$bridgePublish = Join-Path $BuildDir "carbon-bridge"
Invoke-Checked -Program $dotnet -Arguments @(
    "publish",
    (Join-Path $SourceDir "code\dotnet\CarbonBridge\CarbonBridge.csproj"),
    "-c", "Release", "-o", $bridgePublish, "--nologo",
    "--artifacts-path", $managedArtifacts
)

$loader = Find-OneBuildFile -Root $BuildDir -Name "roblox_modloader.dll" -RequiredPathFragment "code\roblox_modloader"
$proxy = Find-OneBuildFile -Root $BuildDir -Name "dwmapi.dll"
Copy-Item -LiteralPath $loader -Destination (Join-Path $stageRml "roblox_modloader.dll") -Force
Copy-Item -LiteralPath $proxy -Destination (Join-Path $stage "dwmapi.dll") -Force
Copy-Item -LiteralPath $helperOut -Destination (Join-Path $stage "carbon-studio-helper.exe") -Force

foreach ($optional in @("roblox_modloader.pdb", "dwmapi.pdb", "carbon-studio-helper.pdb")) {
    $built = Get-ChildItem -LiteralPath $BuildDir -Recurse -File -Filter $optional -ErrorAction SilentlyContinue |
        Where-Object {
            $_.FullName -notmatch "[\\/]_deps[\\/]" -and
            $_.FullName -notmatch "[\\/]qualification-stage[\\/]"
        } |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    if ($built) {
        Copy-Item -LiteralPath $built.FullName -Destination $stageRml -Force
    }
}

Copy-Item -Path (Join-Path $bridgePublish "Carbon.RmlBridge.*") -Destination $stageBridge -Force
foreach ($debugFile in @("RML.Core.pdb", "RML.Interop.pdb", "RML.Logging.pdb", "Roblox.pdb")) {
    $candidate = Join-Path $bridgePublish $debugFile
    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
        Copy-Item -LiteralPath $candidate -Destination $stageBridge -Force
    }
}

$runtimeConfig = Get-ChildItem -LiteralPath $BuildDir -Recurse -File -Filter "RobloxModLoader.runtimeconfig.json" |
    Sort-Object LastWriteTimeUtc -Descending |
    Select-Object -First 1
if (-not $runtimeConfig) {
    throw "RobloxModLoader.runtimeconfig.json was not generated"
}
Copy-Item -LiteralPath $runtimeConfig.FullName -Destination (Join-Path $stageRuntime "RobloxModLoader.runtimeconfig.json") -Force
Copy-Item -LiteralPath $runtimeConfig.FullName -Destination (Join-Path $stageRuntime "RML.runtimeconfig.json") -Force

$nethost = Get-ChildItem -LiteralPath $BuildDir -Recurse -File -Filter "nethost.dll" -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match "win-x64" } |
    Select-Object -First 1
if (-not $nethost) {
    $dotnetRoot = Split-Path -Parent $dotnet
    $nethost = Get-ChildItem -LiteralPath $dotnetRoot -Recurse -File -Filter "nethost.dll" -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match "win-x64" } |
        Sort-Object FullName -Descending |
        Select-Object -First 1
}
if (-not $nethost) {
    throw "nethost.dll was not found in the fresh build or installed .NET host packs"
}
Copy-Item -LiteralPath $nethost.FullName -Destination (Join-Path $stageRuntime "nethost.dll") -Force

$luauLibraries = Join-Path $SourceDir "code\roblox_modloader\resources\luau"
if (Test-Path -LiteralPath $luauLibraries -PathType Container) {
    Copy-Item -Path (Join-Path $luauLibraries "*") -Destination $stageLibraries -Recurse -Force
}
$defaultConfig = Join-Path $SourceDir "tools\packaging\config.default.toml"
Copy-Item -LiteralPath $defaultConfig -Destination (Join-Path $stageRml "config.toml") -Force

$required = @(
    (Join-Path $stage "dwmapi.dll"),
    (Join-Path $stage "carbon-studio-helper.exe"),
    (Join-Path $stageRml "roblox_modloader.dll"),
    (Join-Path $stageRuntime "RML.Core.dll"),
    (Join-Path $stageRuntime "RML.NativeHost.dll"),
    (Join-Path $stageRuntime "Roblox.dll"),
    (Join-Path $stageRuntime "nethost.dll"),
    (Join-Path $stageRuntime "RML.runtimeconfig.json"),
    (Join-Path $stageBridge "Carbon.RmlBridge.dll")
)
$missing = $required | Where-Object { -not (Test-Path -LiteralPath $_ -PathType Leaf) }
if ($missing) {
    throw "fresh RML stage is incomplete:`n$($missing -join "`n")"
}

$marker = @{
    schemaVersion = 1
    buildVersion = $BuildVersion
} | ConvertTo-Json
$utf8 = New-Object System.Text.UTF8Encoding($false)
[IO.File]::WriteAllText((Join-Path $stageRml "carbon-rml.json"), $marker, $utf8)

Write-Host "[fresh-rml] Staging Carbon RML bundle $BuildVersion in $PackageDir"
if (Test-Path -LiteralPath $PackageDir) {
    Remove-Item -LiteralPath $PackageDir -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $PackageDir | Out-Null
Copy-Item -Path (Join-Path $stage "*") -Destination $PackageDir -Recurse -Force

Invoke-Checked -Program $dotnet -Arguments @("build-server", "shutdown")
Write-Host "[fresh-rml] Carbon RML bundle is ready"
