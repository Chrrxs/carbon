[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$GuardScript
)

$ErrorActionPreference = 'Stop'

$source = @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public sealed class CarbonSuspendedProcessFixture : IDisposable
{
    private const uint CreateSuspended = 0x00000004;
    private const uint WaitObject0 = 0x00000000;

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct StartupInfo
    {
        public int Size;
        public string Reserved;
        public string Desktop;
        public string Title;
        public uint X;
        public uint Y;
        public uint XSize;
        public uint YSize;
        public uint XCountChars;
        public uint YCountChars;
        public uint FillAttribute;
        public uint Flags;
        public short ShowWindow;
        public short ReservedBytes;
        public IntPtr ReservedPointer;
        public IntPtr StandardInput;
        public IntPtr StandardOutput;
        public IntPtr StandardError;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct ProcessInformation
    {
        public IntPtr Process;
        public IntPtr Thread;
        public uint ProcessId;
        public uint ThreadId;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct FileTime
    {
        public uint Low;
        public uint High;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool CreateProcessW(
        string applicationName,
        StringBuilder commandLine,
        IntPtr processAttributes,
        IntPtr threadAttributes,
        bool inheritHandles,
        uint creationFlags,
        IntPtr environment,
        string currentDirectory,
        ref StartupInfo startupInfo,
        out ProcessInformation processInformation);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetProcessTimes(
        IntPtr process,
        out FileTime creation,
        out FileTime exit,
        out FileTime kernel,
        out FileTime user);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool TerminateProcess(IntPtr process, uint exitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);

    [DllImport("kernel32.dll")]
    private static extern bool CloseHandle(IntPtr handle);

    private IntPtr process;
    private IntPtr thread;

    public uint ProcessId { get; private set; }
    public long CreationFileTime { get; private set; }
    public string ExecutablePath { get; private set; }

    private CarbonSuspendedProcessFixture(
        ProcessInformation created,
        long creationFileTime,
        string executablePath)
    {
        process = created.Process;
        thread = created.Thread;
        ProcessId = created.ProcessId;
        CreationFileTime = creationFileTime;
        ExecutablePath = executablePath;
    }

    public static CarbonSuspendedProcessFixture Start()
    {
        string executable = Environment.ExpandEnvironmentVariables(@"%WINDIR%\System32\cmd.exe");
        var startup = new StartupInfo { Size = Marshal.SizeOf(typeof(StartupInfo)) };
        ProcessInformation created;
        bool started = CreateProcessW(
            executable,
            new StringBuilder("\"" + executable + "\" /c exit"),
            IntPtr.Zero,
            IntPtr.Zero,
            false,
            CreateSuspended,
            IntPtr.Zero,
            null,
            ref startup,
            out created);
        if (!started)
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateProcessW failed");
        }

        try
        {
            FileTime creation;
            FileTime exit;
            FileTime kernel;
            FileTime user;
            if (!GetProcessTimes(created.Process, out creation, out exit, out kernel, out user))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "GetProcessTimes failed");
            }
            long creationFileTime = unchecked((long)(((ulong)creation.High << 32) | creation.Low));
            return new CarbonSuspendedProcessFixture(created, creationFileTime, executable);
        }
        catch
        {
            TerminateProcess(created.Process, 1);
            WaitForSingleObject(created.Process, 5000);
            CloseHandle(created.Thread);
            CloseHandle(created.Process);
            throw;
        }
    }

    public static uint ReadExitCode(IntPtr process)
    {
        uint exitCode;
        if (!GetExitCodeProcess(process, out exitCode))
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "GetExitCodeProcess failed");
        }
        return exitCode;
    }

    public void Dispose()
    {
        if (process != IntPtr.Zero)
        {
            if (!TerminateProcess(process, 0))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "TerminateProcess failed");
            }
            uint wait = WaitForSingleObject(process, 5000);
            if (wait != WaitObject0)
            {
                throw new InvalidOperationException("Suspended process did not exit cleanly");
            }
            CloseHandle(process);
            process = IntPtr.Zero;
        }
        if (thread != IntPtr.Zero)
        {
            CloseHandle(thread);
            thread = IntPtr.Zero;
        }
    }
}
'@

Add-Type -TypeDefinition $source -Language CSharp
$target = [CarbonSuspendedProcessFixture]::Start()
try {
    $encodedExecutable = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($target.ExecutablePath))
    $commonArguments = @(
        '-Mta',
        '-NoProfile',
        '-NonInteractive',
        '-ExecutionPolicy',
        'Bypass',
        '-File',
        $GuardScript,
        '-TargetProcessId',
        $target.ProcessId.ToString(),
        '-ExecutableBase64',
        $encodedExecutable,
        '-CreationFileTime',
        $target.CreationFileTime.ToString(),
        '-Policy',
        'muted'
    )
    $powershell = Join-Path $PSHOME 'powershell.exe'
    function Invoke-Guard([string[]]$Arguments) {
        $stdoutPath = [IO.Path]::GetTempFileName()
        $stderrPath = [IO.Path]::GetTempFileName()
        try {
            $child = Start-Process `
                -FilePath $powershell `
                -ArgumentList $Arguments `
                -WindowStyle Hidden `
                -RedirectStandardOutput $stdoutPath `
                -RedirectStandardError $stderrPath `
                -PassThru
            $childHandle = $child.Handle
            $child.WaitForExit() | Out-Null
            $result = [pscustomobject]@{
                ExitCode = [CarbonSuspendedProcessFixture]::ReadExitCode($childHandle)
                Stdout = [IO.File]::ReadAllText($stdoutPath)
                Stderr = [IO.File]::ReadAllText($stderrPath)
            }
            return $result
        } finally {
            Remove-Item -LiteralPath $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
        }
    }

    $wrongExecutable = $target.ExecutablePath + '.unexpected'
    $wrongEncodedExecutable = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($wrongExecutable))
    $wrongArguments = @($commonArguments)
    $wrongArguments[[Array]::IndexOf($wrongArguments, $encodedExecutable)] = $wrongEncodedExecutable
    $rejected = Invoke-Guard ($wrongArguments + @('-Mode', 'spawn'))
    if ($rejected.ExitCode -eq 0 -or $rejected.Stderr -notlike '*path no longer matches*') {
        throw "Audio guard accepted a mismatched suspended-process path. stdout: $($rejected.Stdout) stderr: $($rejected.Stderr)"
    }

    $spawn = Invoke-Guard ($commonArguments + @('-Mode', 'spawn'))
    if ($spawn.ExitCode -ne 0) {
        throw "Audio guard spawn failed with exit code $($spawn.ExitCode) from $(@($spawn).Count) result(s). stdout: $($spawn.Stdout) stderr: $($spawn.Stderr)"
    }

    $command = Invoke-Guard ($commonArguments + @('-Mode', 'command', '-ConnectTimeoutMilliseconds', '10000'))
    if ($command.ExitCode -ne 0) {
        throw "Audio guard command failed. stdout: $($command.Stdout) stderr: $($command.Stderr)"
    }
    $response = $command.Stdout.Trim()
    $report = $response | ConvertFrom-Json
    if ($report.policy -ne 'muted' -or $report.matched_sessions -ne 0 -or $report.changed_sessions -ne 0) {
        throw "Audio guard returned an unexpected suspended-process acknowledgement: $response"
    }
    $response
} finally {
    $target.Dispose()
}
