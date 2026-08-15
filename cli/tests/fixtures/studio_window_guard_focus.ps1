[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$GuardScript,

    [Parameter(Mandatory = $true)]
    [string]$HookLibrary
)

$ErrorActionPreference = 'Stop'

$fixtureSource = @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading;
using System.Windows.Forms;

internal static class CarbonWindowGuardFixture
{
    [DllImport("kernel32.dll")]
    private static extern uint GetCurrentThreadId();

    [DllImport("user32.dll")]
    private static extern IntPtr GetForegroundWindow();

    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool AttachThreadInput(uint attachThreadId, uint attachToThreadId, bool attach);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool ShowWindowAsync(IntPtr window, int command);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool BringWindowToTop(IntPtr window);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetForegroundWindow(IntPtr window);

    [DllImport("user32.dll")]
    private static extern IntPtr SetFocus(IntPtr window);

    private static void Activate(IntPtr window)
    {
        IntPtr foreground = GetForegroundWindow();
        uint ignored;
        uint foregroundThread = foreground == IntPtr.Zero
            ? 0
            : GetWindowThreadProcessId(foreground, out ignored);
        uint currentThread = GetCurrentThreadId();
        bool attached = false;
        try
        {
            if (foregroundThread != 0 && foregroundThread != currentThread)
            {
                attached = AttachThreadInput(currentThread, foregroundThread, true);
                if (!attached)
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(), "AttachThreadInput failed");
                }
            }
            ShowWindowAsync(window, 9);
            BringWindowToTop(window);
            SetForegroundWindow(window);
            SetFocus(window);
        }
        finally
        {
            if (attached)
            {
                AttachThreadInput(currentThread, foregroundThread, false);
            }
        }
    }

    [STAThread]
    public static void Main()
    {
        Application.EnableVisualStyles();
        Application.SetCompatibleTextRenderingDefault(false);
        using (Form form = new Form())
        {
            form.Text = "Carbon window guard fixture";
            form.Width = 180;
            form.Height = 100;
            form.ShowInTaskbar = false;
            form.StartPosition = FormStartPosition.Manual;
            form.Left = 40;
            form.Top = 40;

            Thread commands = null;
            form.Shown += delegate
            {
                Console.WriteLine("ready");
                Console.Out.Flush();
                commands = new Thread(delegate()
                {
                    string command;
                    while ((command = Console.ReadLine()) != null)
                    {
                        if (string.Equals(command, "activate", StringComparison.Ordinal))
                        {
                            form.BeginInvoke((Action)delegate
                            {
                                Activate(form.Handle);
                                Console.WriteLine("activated");
                                Console.Out.Flush();
                            });
                        }
                        else if (string.Equals(command, "exit", StringComparison.Ordinal))
                        {
                            form.BeginInvoke((Action)delegate { form.Close(); });
                            return;
                        }
                    }
                });
                commands.IsBackground = true;
                commands.Start();
            };
            Application.Run(form);
        }
    }
}
'@

$probeSource = @'
using System;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading;

public static class CarbonWindowGuardProbe
{
    private const uint EventSystemForeground = 3;
    private const uint WineventOutOfContext = 0;
    private const uint PmRemove = 1;

    private delegate void WinEventDelegate(
        IntPtr hook,
        uint eventType,
        IntPtr window,
        int objectId,
        int childId,
        uint eventThread,
        uint eventTime);

    [StructLayout(LayoutKind.Sequential)]
    private struct Point
    {
        public int X;
        public int Y;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct Message
    {
        public IntPtr Window;
        public uint Id;
        public UIntPtr WParam;
        public IntPtr LParam;
        public uint Time;
        public Point Cursor;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct FileTime
    {
        public uint Low;
        public uint High;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetProcessTimes(
        IntPtr process,
        out FileTime creation,
        out FileTime exit,
        out FileTime kernel,
        out FileTime user);

    [DllImport("user32.dll")]
    private static extern IntPtr GetForegroundWindow();

    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);

    [DllImport("user32.dll")]
    private static extern IntPtr SetWinEventHook(
        uint eventMin,
        uint eventMax,
        IntPtr eventHookModule,
        WinEventDelegate callback,
        uint processId,
        uint threadId,
        uint flags);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool UnhookWinEvent(IntPtr hook);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool PeekMessage(
        out Message message,
        IntPtr window,
        uint messageMin,
        uint messageMax,
        uint removeMessage);

    [DllImport("user32.dll")]
    private static extern IntPtr DispatchMessage(ref Message message);

    public static long CreationFileTime(Process process)
    {
        FileTime creation;
        FileTime exit;
        FileTime kernel;
        FileTime user;
        if (!GetProcessTimes(process.Handle, out creation, out exit, out kernel, out user))
        {
            throw new InvalidOperationException("GetProcessTimes failed");
        }
        return unchecked((long)(((ulong)creation.High << 32) | creation.Low));
    }

    public static uint ForegroundProcessId()
    {
        uint processId;
        GetWindowThreadProcessId(GetForegroundWindow(), out processId);
        return processId;
    }

    public static bool WaitForForeground(uint processId, int timeoutMilliseconds)
    {
        DateTime deadline = DateTime.UtcNow.AddMilliseconds(timeoutMilliseconds);
        do
        {
            if (ForegroundProcessId() == processId)
            {
                return true;
            }
            Thread.Sleep(10);
        }
        while (DateTime.UtcNow < deadline);
        return false;
    }

    public static bool WatchForbiddenForeground(
        uint expectedProcessId,
        uint forbiddenProcessId,
        StreamWriter trigger,
        int observationMilliseconds)
    {
        bool sawForbiddenProcess = false;
        WinEventDelegate callback = delegate(
            IntPtr hook,
            uint eventType,
            IntPtr window,
            int objectId,
            int childId,
            uint eventThread,
            uint eventTime)
        {
            uint processId;
            GetWindowThreadProcessId(window, out processId);
            if (processId == forbiddenProcessId)
            {
                sawForbiddenProcess = true;
            }
        };

        IntPtr eventHook = SetWinEventHook(
            EventSystemForeground,
            EventSystemForeground,
            IntPtr.Zero,
            callback,
            0,
            0,
            WineventOutOfContext);
        if (eventHook == IntPtr.Zero)
        {
            throw new InvalidOperationException("SetWinEventHook failed");
        }

        try
        {
            if (ForegroundProcessId() != expectedProcessId)
            {
                throw new InvalidOperationException("foreground precondition was lost");
            }
            trigger.WriteLine("activate");
            trigger.Flush();

            DateTime deadline = DateTime.UtcNow.AddMilliseconds(observationMilliseconds);
            while (DateTime.UtcNow < deadline)
            {
                Message message;
                while (PeekMessage(out message, IntPtr.Zero, 0, 0, PmRemove))
                {
                    DispatchMessage(ref message);
                }
                if (sawForbiddenProcess)
                {
                    return false;
                }
                Thread.Sleep(1);
            }
            return ForegroundProcessId() == expectedProcessId;
        }
        finally
        {
            UnhookWinEvent(eventHook);
            GC.KeepAlive(callback);
        }
    }
}
'@

function Start-WindowFixture([string]$Executable) {
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $Executable
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    if (-not $process.Start()) {
        throw 'Could not start the window guard fixture'
    }
    if ($process.StandardOutput.ReadLine() -ne 'ready') {
        throw "Window guard fixture did not become ready: $($process.StandardError.ReadToEnd())"
    }
    return $process
}

function Invoke-Guard([Diagnostics.Process]$Target, [string]$Mode, [string]$Policy) {
    $creationFileTime = [CarbonWindowGuardProbe]::CreationFileTime($Target)
    $encodedExecutable = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($Target.MainModule.FileName))
    $arguments = @(
        '-Sta',
        '-NoProfile',
        '-NonInteractive',
        '-ExecutionPolicy',
        'Bypass',
        '-File',
        $GuardScript,
        '-Mode',
        $Mode,
        '-TargetProcessId',
        $Target.Id.ToString(),
        '-ExecutableBase64',
        $encodedExecutable,
        '-CreationFileTime',
        $creationFileTime.ToString(),
        '-Policy',
        $Policy,
        '-HookLibrary',
        $HookLibrary,
        '-ConnectTimeoutMilliseconds',
        '10000'
    )
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = Join-Path $PSHOME 'powershell.exe'
    $start.Arguments = (($arguments | ForEach-Object { '"' + $_.Replace('"', '\"') + '"' }) -join ' ')
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $child = [Diagnostics.Process]::new()
    $child.StartInfo = $start
    if (-not $child.Start()) {
        throw "Could not start window guard $Mode/$Policy"
    }
    $stdout = $child.StandardOutput.ReadToEnd()
    $stderr = $child.StandardError.ReadToEnd()
    $child.WaitForExit()
    if ($child.ExitCode -ne 0) {
        throw "Window guard $Mode/$Policy failed with exit code $($child.ExitCode): $stderr"
    }
    if ($Mode -eq 'command') {
        return ($stdout.Trim() | ConvertFrom-Json)
    }
}

function Invoke-FixtureActivation([Diagnostics.Process]$Fixture) {
    $Fixture.StandardInput.WriteLine('activate')
    $Fixture.StandardInput.Flush()
    if ($Fixture.StandardOutput.ReadLine() -ne 'activated') {
        throw "Window fixture activation failed: $($Fixture.StandardError.ReadToEnd())"
    }
}

function Stop-WindowFixture([Diagnostics.Process]$Fixture) {
    if ($null -eq $Fixture) {
        return
    }
    try {
        if (-not $Fixture.HasExited) {
            $Fixture.StandardInput.WriteLine('exit')
            $Fixture.StandardInput.Flush()
            if (-not $Fixture.WaitForExit(3000)) {
                $Fixture.Kill()
                $Fixture.WaitForExit()
            }
        }
    } catch {
        try { $Fixture.Kill() } catch { }
    }
    $Fixture.Dispose()
}

$temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ("carbon-window-guard-" + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
$fixtureExecutable = Join-Path $temporaryDirectory 'CarbonWindowGuardFixture.exe'
$owner = $null
$target = $null
try {
    Add-Type `
        -TypeDefinition $fixtureSource `
        -Language CSharp `
        -ReferencedAssemblies @('System.Windows.Forms', 'System.Drawing') `
        -OutputAssembly $fixtureExecutable `
        -OutputType ConsoleApplication
    Add-Type -TypeDefinition $probeSource -Language CSharp

    $owner = Start-WindowFixture $fixtureExecutable
    $target = Start-WindowFixture $fixtureExecutable

    Invoke-FixtureActivation $owner
    if (-not [CarbonWindowGuardProbe]::WaitForForeground($owner.Id, 3000)) {
        throw 'Could not establish the foreground owner precondition'
    }

    Invoke-Guard $target 'spawn' 'active'
    $parked = Invoke-Guard $target 'command' 'parked'
    $blocked = [CarbonWindowGuardProbe]::WatchForbiddenForeground(
        $owner.Id,
        $target.Id,
        $target.StandardInput,
        750
    )
    if ($target.StandardOutput.ReadLine() -ne 'activated') {
        throw "Parked fixture did not attempt activation: $($target.StandardError.ReadToEnd())"
    }
    if (-not $blocked) {
        throw 'The parked window became foreground after programmatic self-activation'
    }

    $active = Invoke-Guard $target 'command' 'active'
    Invoke-FixtureActivation $target
    if (-not [CarbonWindowGuardProbe]::WaitForForeground($target.Id, 3000)) {
        throw 'The unparked window remained blocked from foreground activation'
    }

    [PSCustomObject]@{
        parked_policy = $parked.policy
        parked_guarded_threads = $parked.guarded_threads
        active_policy = $active.policy
        active_guarded_threads = $active.guarded_threads
        self_activation_blocked = $blocked
        active_self_activation_allowed = $true
    } | ConvertTo-Json -Compress
} finally {
    if ($null -ne $target) {
        try { Invoke-Guard $target 'command' 'active' | Out-Null } catch { }
    }
    Stop-WindowFixture $target
    Stop-WindowFixture $owner
    Start-Sleep -Milliseconds 250
    Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
}
