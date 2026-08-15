[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('guard', 'spawn', 'command', 'compile')]
    [string]$Mode,

    [Parameter(Mandatory = $true)]
    [uint32]$TargetProcessId,

    [Parameter(Mandatory = $true)]
    [string]$ExecutableBase64,

    [Parameter(Mandatory = $true)]
    [int64]$CreationFileTime,

    [Parameter(Mandatory = $true)]
    [ValidateSet('parked', 'active')]
    [string]$Policy,

    [Parameter(Mandatory = $true)]
    [string]$HookLibrary,

    [int]$ConnectTimeoutMilliseconds = 10000
)

$ErrorActionPreference = 'Stop'

$processIdentitySource = @'
namespace CarbonStudioWindowGuard
{
    public static class ProcessIdentity
    {
        private const uint ProcessQueryLimitedInformation = 0x00001000;
        private const uint StillActive = 259;

        [System.Runtime.InteropServices.StructLayout(System.Runtime.InteropServices.LayoutKind.Sequential)]
        private struct FileTime
        {
            internal uint Low;
            internal uint High;
        }

        [System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
        private static extern System.IntPtr OpenProcess(
            uint desiredAccess,
            bool inheritHandle,
            uint processId);

        [System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetExitCodeProcess(
            System.IntPtr process,
            out uint exitCode);

        [System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool GetProcessTimes(
            System.IntPtr process,
            out FileTime creation,
            out FileTime exit,
            out FileTime kernel,
            out FileTime user);

        [System.Runtime.InteropServices.DllImport(
            "kernel32.dll",
            CharSet = System.Runtime.InteropServices.CharSet.Unicode,
            SetLastError = true)]
        private static extern bool QueryFullProcessImageNameW(
            System.IntPtr process,
            uint flags,
            System.Text.StringBuilder path,
            ref uint size);

        [System.Runtime.InteropServices.DllImport("kernel32.dll")]
        private static extern bool CloseHandle(System.IntPtr handle);

        private static string NativeError(string operation)
        {
            return operation + " failed with Windows error " +
                System.Runtime.InteropServices.Marshal.GetLastWin32Error().ToString();
        }

        private static string NormalizePath(string path)
        {
            try
            {
                return System.IO.Path.GetFullPath(path).TrimEnd('\\');
            }
            catch
            {
                return path.TrimEnd('\\');
            }
        }

        public static string Validate(uint processId, string expectedExecutable, long creationFileTime)
        {
            System.IntPtr process = OpenProcess(ProcessQueryLimitedInformation, false, processId);
            if (process == System.IntPtr.Zero)
            {
                return NativeError("OpenProcess");
            }
            try
            {
                uint exitCode;
                if (!GetExitCodeProcess(process, out exitCode))
                {
                    return NativeError("GetExitCodeProcess");
                }
                if (exitCode != StillActive)
                {
                    return "Roblox Studio process " + processId.ToString() + " is no longer running";
                }

                FileTime creation;
                FileTime exit;
                FileTime kernel;
                FileTime user;
                if (!GetProcessTimes(process, out creation, out exit, out kernel, out user))
                {
                    return NativeError("GetProcessTimes");
                }
                long actualCreation = unchecked((long)(((ulong)creation.High << 32) | creation.Low));
                if (actualCreation != creationFileTime)
                {
                    return "Roblox Studio PID " + processId.ToString() + " creation time no longer matches";
                }

                uint capacity = 32768;
                System.Text.StringBuilder path = new System.Text.StringBuilder((int)capacity);
                if (!QueryFullProcessImageNameW(process, 0, path, ref capacity))
                {
                    return NativeError("QueryFullProcessImageNameW");
                }
                if (!string.Equals(
                    NormalizePath(path.ToString()),
                    NormalizePath(expectedExecutable),
                    System.StringComparison.OrdinalIgnoreCase))
                {
                    return "Roblox Studio PID " + processId.ToString() + " executable path no longer matches";
                }
                return null;
            }
            finally
            {
                CloseHandle(process);
            }
        }

        public static bool IsExact(uint processId, string expectedExecutable, long creationFileTime)
        {
            return Validate(processId, expectedExecutable, creationFileTime) == null;
        }
    }
}
'@

function Add-ProcessIdentityInterop {
    if ($null -eq ('CarbonStudioWindowGuard.ProcessIdentity' -as [type])) {
        Add-Type -TypeDefinition $processIdentitySource -Language CSharp
    }
}

function Get-ExpectedExecutable {
    if ([string]::IsNullOrEmpty($ExecutableBase64)) {
        throw 'Carbon Studio window guard requires an executable identity'
    }
    [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($ExecutableBase64))
}

function Assert-ExactProcess([string]$ExpectedExecutable) {
    Add-ProcessIdentityInterop
    $validationError = [CarbonStudioWindowGuard.ProcessIdentity]::Validate(
        $TargetProcessId,
        $ExpectedExecutable,
        $CreationFileTime)
    if (-not [string]::IsNullOrEmpty($validationError)) {
        throw $validationError
    }
}

function Install-HookLibrary {
    $bytes = [IO.File]::ReadAllBytes($HookLibrary)
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $digest = ([BitConverter]::ToString($sha.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
    $directory = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Carbon\window-guards'
    [IO.Directory]::CreateDirectory($directory) | Out-Null
    $installed = Join-Path $directory ("carbon-studio-window-guard-$($digest.Substring(0, 16)).dll")
    $matches = $false
    if ([IO.File]::Exists($installed)) {
        $matches = [Linq.Enumerable]::SequenceEqual(
            [byte[]][IO.File]::ReadAllBytes($installed),
            [byte[]]$bytes)
    }
    if (-not $matches) {
        $temporary = "$installed.$([Guid]::NewGuid().ToString('N')).tmp"
        try {
            [IO.File]::WriteAllBytes($temporary, $bytes)
            Move-Item -LiteralPath $temporary -Destination $installed -Force
        } finally {
            Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
        }
    }
    return $installed
}

if ($Mode -eq 'command') {
    $expectedExecutable = Get-ExpectedExecutable
    Assert-ExactProcess $expectedExecutable
    $pipeName = "carbon-studio-window-v1-$TargetProcessId-$CreationFileTime"
    $pipe = [IO.Pipes.NamedPipeClientStream]::new(
        '.',
        $pipeName,
        [IO.Pipes.PipeDirection]::InOut,
        [IO.Pipes.PipeOptions]::Asynchronous
    )
    try {
        try {
            $pipe.Connect($ConnectTimeoutMilliseconds)
        } catch {
            throw "Carbon Studio window guard did not become ready for PID $TargetProcessId within $ConnectTimeoutMilliseconds ms"
        }
        $writer = [IO.StreamWriter]::new($pipe, [Text.UTF8Encoding]::new($false), 1024, $true)
        $reader = [IO.StreamReader]::new($pipe, [Text.UTF8Encoding]::new($false), $false, 1024, $true)
        try {
            $writer.AutoFlush = $true
            $writer.WriteLine($Policy)
            $response = $reader.ReadLine()
            if ([string]::IsNullOrEmpty($response)) {
                throw 'Carbon Studio window guard returned no policy acknowledgement'
            }
            $report = ConvertFrom-Json -InputObject $response
            if ($report.policy -ne $Policy) {
                throw "Carbon Studio window guard acknowledged unexpected policy '$($report.policy)'"
            }
            Write-Output $response
        } finally {
            $reader.Dispose()
            $writer.Dispose()
        }
    } finally {
        $pipe.Dispose()
    }
    exit 0
}

if ($Mode -eq 'spawn') {
    $expectedExecutable = Get-ExpectedExecutable
    Assert-ExactProcess $expectedExecutable
    $installedHookLibrary = Install-HookLibrary
    $arguments = @(
        '-Sta',
        '-NoProfile',
        '-NonInteractive',
        '-ExecutionPolicy',
        'Bypass',
        '-File',
        ('"' + $PSCommandPath + '"'),
        '-Mode',
        'guard',
        '-TargetProcessId',
        $TargetProcessId.ToString(),
        '-ExecutableBase64',
        $ExecutableBase64,
        '-CreationFileTime',
        $CreationFileTime.ToString(),
        '-Policy',
        $Policy,
        '-HookLibrary',
        ('"' + $installedHookLibrary + '"')
    )
    Start-Process `
        -FilePath (Join-Path $PSHOME 'powershell.exe') `
        -ArgumentList $arguments `
        -WindowStyle Hidden | Out-Null
    exit 0
}

$source = @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.IO;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

namespace CarbonStudioWindowGuard
{
    internal sealed class WindowGuard : IDisposable
    {
        private const int WhCbt = 5;

        private delegate bool EnumWindowsCallback(IntPtr window, IntPtr parameter);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr LoadLibraryW(string path);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool FreeLibrary(IntPtr module);

        [DllImport("kernel32.dll", CharSet = CharSet.Ansi, SetLastError = true)]
        private static extern IntPtr GetProcAddress(IntPtr module, string name);

        [DllImport("user32.dll")]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool EnumWindows(EnumWindowsCallback callback, IntPtr parameter);

        [DllImport("user32.dll")]
        private static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);

        [DllImport("user32.dll", SetLastError = true)]
        private static extern IntPtr SetWindowsHookExW(int hookType, IntPtr callback, IntPtr module, uint threadId);

        [DllImport("user32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool UnhookWindowsHookEx(IntPtr hook);

        private readonly uint targetProcessId;
        private readonly IntPtr module;
        private readonly IntPtr callback;
        private readonly Dictionary<uint, IntPtr> hooks = new Dictionary<uint, IntPtr>();
        private bool parked;
        private bool disposed;

        internal WindowGuard(uint targetProcessId, string hookLibrary, bool initiallyParked)
        {
            this.targetProcessId = targetProcessId;
            module = LoadLibraryW(hookLibrary);
            if (module == IntPtr.Zero)
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "could not load Carbon window hook library");
            }
            callback = GetProcAddress(module, "CarbonWindowGuardHook");
            if (callback == IntPtr.Zero)
            {
                FreeLibrary(module);
                throw new Win32Exception(Marshal.GetLastWin32Error(), "Carbon window hook export is missing");
            }
            parked = initiallyParked;
            RefreshHooks();
        }

        internal string SetPolicy(bool shouldPark)
        {
            parked = shouldPark;
            RefreshHooks();
            return "{\"policy\":\"" + (shouldPark ? "parked" : "active") +
                "\",\"guarded_threads\":" + hooks.Count.ToString() + "}";
        }

        internal void Pulse()
        {
            if (parked)
            {
                RefreshHooks();
            }
        }

        private void RefreshHooks()
        {
            if (!parked)
            {
                RemoveAllHooks();
                return;
            }

            HashSet<uint> observed = new HashSet<uint>();
            EnumWindowsCallback enumerator = delegate(IntPtr window, IntPtr parameter)
            {
                uint processId;
                uint threadId = GetWindowThreadProcessId(window, out processId);
                if (processId == targetProcessId && threadId != 0)
                {
                    observed.Add(threadId);
                }
                return true;
            };
            if (!EnumWindows(enumerator, IntPtr.Zero))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "could not enumerate Studio windows");
            }

            foreach (uint threadId in observed)
            {
                if (hooks.ContainsKey(threadId))
                {
                    continue;
                }
                IntPtr hook = SetWindowsHookExW(WhCbt, callback, module, threadId);
                if (hook == IntPtr.Zero)
                {
                    throw new Win32Exception(
                        Marshal.GetLastWin32Error(),
                        "could not install Carbon activation veto for Studio thread " + threadId.ToString());
                }
                hooks.Add(threadId, hook);
            }

            List<uint> removed = new List<uint>();
            foreach (KeyValuePair<uint, IntPtr> pair in hooks)
            {
                if (!observed.Contains(pair.Key))
                {
                    UnhookWindowsHookEx(pair.Value);
                    removed.Add(pair.Key);
                }
            }
            foreach (uint threadId in removed)
            {
                hooks.Remove(threadId);
            }
        }

        private void RemoveAllHooks()
        {
            foreach (IntPtr hook in hooks.Values)
            {
                if (!UnhookWindowsHookEx(hook))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "could not remove Carbon activation veto");
                }
            }
            hooks.Clear();
        }

        public void Dispose()
        {
            if (disposed)
            {
                return;
            }
            disposed = true;
            try
            {
                RemoveAllHooks();
            }
            finally
            {
                FreeLibrary(module);
            }
        }
    }

    public static class Program
    {
        public static void Run(
            uint processId,
            string expectedExecutable,
            long creationFileTime,
            string hookLibrary,
            string initialPolicy)
        {
            string identity = processId.ToString() + "-" + creationFileTime.ToString();
            string pipeName = "carbon-studio-window-v1-" + identity;
            bool ownsMutex;
            using (Mutex singleton = new Mutex(true, "Local\\CarbonStudioWindow-v1-" + identity, out ownsMutex))
            {
                if (!ownsMutex)
                {
                    return;
                }
                try
                {
                    if (!ProcessIdentity.IsExact(processId, expectedExecutable, creationFileTime))
                    {
                        return;
                    }
                    using (WindowGuard guard = new WindowGuard(
                        processId,
                        hookLibrary,
                        string.Equals(initialPolicy, "parked", StringComparison.Ordinal)))
                    {
                        while (ProcessIdentity.IsExact(processId, expectedExecutable, creationFileTime))
                        {
                            using (NamedPipeServerStream pipe = new NamedPipeServerStream(
                                pipeName,
                                PipeDirection.InOut,
                                1,
                                PipeTransmissionMode.Byte,
                                PipeOptions.Asynchronous))
                            {
                                IAsyncResult connection = pipe.BeginWaitForConnection(null, null);
                                while (!connection.AsyncWaitHandle.WaitOne(50))
                                {
                                    try
                                    {
                                        guard.Pulse();
                                    }
                                    catch
                                    {
                                        // Keep the verified guardian and its existing hooks alive.
                                        // A disappearing UI thread can race enumeration; the next
                                        // pulse retries any new or replacement thread.
                                    }
                                    if (!ProcessIdentity.IsExact(processId, expectedExecutable, creationFileTime))
                                    {
                                        return;
                                    }
                                }
                                pipe.EndWaitForConnection(connection);
                                using (StreamReader reader = new StreamReader(
                                    pipe, new UTF8Encoding(false), false, 1024, true))
                                using (StreamWriter writer = new StreamWriter(
                                    pipe, new UTF8Encoding(false), 1024, true))
                                {
                                    writer.AutoFlush = true;
                                    string command = reader.ReadLine();
                                    if (string.Equals(command, "parked", StringComparison.Ordinal))
                                    {
                                        writer.WriteLine(guard.SetPolicy(true));
                                    }
                                    else if (string.Equals(command, "active", StringComparison.Ordinal))
                                    {
                                        writer.WriteLine(guard.SetPolicy(false));
                                    }
                                    else
                                    {
                                        writer.WriteLine("{\"error\":\"invalid policy\"}");
                                    }
                                }
                            }
                        }
                    }
                }
                finally
                {
                    if (ownsMutex)
                    {
                        singleton.ReleaseMutex();
                    }
                }
            }
        }
    }
}
'@

$installedHookLibrary = Install-HookLibrary
Add-Type -TypeDefinition ($source + [Environment]::NewLine + $processIdentitySource) -Language CSharp
if ($Mode -eq 'compile') {
    Write-Output 'ok'
    exit 0
}

$expectedExecutable = Get-ExpectedExecutable
Assert-ExactProcess $expectedExecutable
[CarbonStudioWindowGuard.Program]::Run(
    $TargetProcessId,
    $expectedExecutable,
    $CreationFileTime,
    $installedHookLibrary,
    $Policy
)
