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
using System.Windows.Forms;

internal static class CarbonWindowGuardFixture
{
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
            form.Left = -10000;
            form.Top = -10000;

            System.Threading.Thread commands = null;
            form.Shown += delegate
            {
                Console.WriteLine("ready");
                Console.Out.Flush();
                commands = new System.Threading.Thread(delegate()
                {
                    string command;
                    while ((command = Console.ReadLine()) != null)
                    {
                        if (string.Equals(command, "exit", StringComparison.Ordinal))
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
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;

public static class CarbonWindowGuardProbe
{
    private const int HcbtActivate = 5;

    [StructLayout(LayoutKind.Sequential)]
    private struct FileTime
    {
        public uint Low;
        public uint High;
    }

    [UnmanagedFunctionPointer(CallingConvention.Winapi)]
    private delegate IntPtr HookCallback(int code, UIntPtr window, IntPtr activation);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr LoadLibraryW(string path);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr GetProcAddress(IntPtr module, string name);

    [DllImport("kernel32.dll")]
    private static extern bool FreeLibrary(IntPtr module);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetProcessTimes(
        IntPtr process,
        out FileTime creation,
        out FileTime exit,
        out FileTime kernel,
        out FileTime user);

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

    public static bool StrictHookBlocksActivation(string hookLibrary)
    {
        IntPtr module = LoadLibraryW(hookLibrary);
        if (module == IntPtr.Zero)
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(), "could not load strict window hook fixture");
        }
        try
        {
            IntPtr address = GetProcAddress(module, "CarbonWindowGuardHook");
            if (address == IntPtr.Zero)
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(), "strict window hook export is missing");
            }
            HookCallback callback = (HookCallback)Marshal.GetDelegateForFunctionPointer(address, typeof(HookCallback));
            return callback(HcbtActivate, UIntPtr.Zero, IntPtr.Zero) == new IntPtr(1);
        }
        finally
        {
            FreeLibrary(module);
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
$target = $null
try {
    Add-Type `
        -TypeDefinition $fixtureSource `
        -Language CSharp `
        -ReferencedAssemblies @('System.Windows.Forms', 'System.Drawing') `
        -OutputAssembly $fixtureExecutable `
        -OutputType ConsoleApplication
    Add-Type -TypeDefinition $probeSource -Language CSharp

    $strictHookBlocksActivation = [CarbonWindowGuardProbe]::StrictHookBlocksActivation($HookLibrary)
    if (-not $strictHookBlocksActivation) {
        throw 'The strict window hook fixture did not veto HCBT_ACTIVATE'
    }

    $target = Start-WindowFixture $fixtureExecutable
    Invoke-Guard $target 'spawn' 'active'
    $parked = Invoke-Guard $target 'command' 'parked'
    $active = Invoke-Guard $target 'command' 'active'

    [PSCustomObject]@{
        parked_policy = $parked.policy
        parked_guarded_threads = $parked.guarded_threads
        active_policy = $active.policy
        active_guarded_threads = $active.guarded_threads
        strict_hook_blocks_activation = $strictHookBlocksActivation
    } | ConvertTo-Json -Compress
} finally {
    if ($null -ne $target) {
        try { Invoke-Guard $target 'command' 'active' | Out-Null } catch { }
    }
    Stop-WindowFixture $target
    Start-Sleep -Milliseconds 250
    Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
}
