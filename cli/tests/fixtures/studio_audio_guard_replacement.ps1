[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$GuardScript
)

$ErrorActionPreference = 'Stop'

$fixtureSource = @'
using System;
using System.IO;
using System.Media;

internal static class CarbonAudioReplacementFixture
{
    private static MemoryStream stream;
    private static SoundPlayer player;

    private static MemoryStream SilentWave()
    {
        const int sampleRate = 8000;
        const short channels = 1;
        const short bitsPerSample = 16;
        int dataLength = sampleRate * channels * (bitsPerSample / 8);
        var result = new MemoryStream(44 + dataLength);
        using (var writer = new BinaryWriter(result, System.Text.Encoding.ASCII, true))
        {
            writer.Write(System.Text.Encoding.ASCII.GetBytes("RIFF"));
            writer.Write(36 + dataLength);
            writer.Write(System.Text.Encoding.ASCII.GetBytes("WAVE"));
            writer.Write(System.Text.Encoding.ASCII.GetBytes("fmt "));
            writer.Write(16);
            writer.Write((short)1);
            writer.Write(channels);
            writer.Write(sampleRate);
            writer.Write(sampleRate * channels * (bitsPerSample / 8));
            writer.Write((short)(channels * (bitsPerSample / 8)));
            writer.Write(bitsPerSample);
            writer.Write(System.Text.Encoding.ASCII.GetBytes("data"));
            writer.Write(dataLength);
            writer.Write(new byte[dataLength]);
        }
        result.Position = 0;
        return result;
    }

    private static void StartAudio()
    {
        stream = SilentWave();
        player = new SoundPlayer(stream);
        player.Load();
        player.PlayLooping();
    }

    private static void StopAudio()
    {
        if (player != null)
        {
            player.Stop();
            player.Dispose();
            player = null;
        }
        if (stream != null)
        {
            stream.Dispose();
            stream = null;
        }
        GC.Collect();
        GC.WaitForPendingFinalizers();
        GC.Collect();
    }

    public static void Main()
    {
        StartAudio();
        Console.WriteLine("ready");
        Console.Out.Flush();
        string command;
        while ((command = Console.ReadLine()) != null)
        {
            if (string.Equals(command, "exit", StringComparison.Ordinal))
            {
                break;
            }
        }
        StopAudio();
    }
}
'@

$probeSource = @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;

public sealed class CarbonAudioSessionSnapshot
{
    public string EndpointId { get; set; }
    public string SessionId { get; set; }
    public string InstanceId { get; set; }
    public bool Muted { get; set; }
}

public static class CarbonAudioReplacementProbe
{
    private const uint DeviceStateActive = 0x00000001;
    private const uint ClassContextAll = 0x00000017;
    private const uint ProcessQueryLimitedInformation = 0x00001000;

    private enum EDataFlow { Render = 0, Capture = 1, All = 2 }
    private enum ERole { Console = 0, Multimedia = 1, Communications = 2 }
    private enum AudioSessionState { Inactive = 0, Active = 1, Expired = 2 }

    [ComImport]
    [Guid("A95664D2-9614-4F35-A746-DE8DB63617E6")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IMMDeviceEnumerator
    {
        [PreserveSig] int EnumAudioEndpoints(EDataFlow dataFlow, uint stateMask, out IMMDeviceCollection devices);
        [PreserveSig] int GetDefaultAudioEndpoint(EDataFlow dataFlow, ERole role, out IMMDevice device);
        [PreserveSig] int GetDevice([MarshalAs(UnmanagedType.LPWStr)] string id, out IMMDevice device);
        [PreserveSig] int RegisterEndpointNotificationCallback(IntPtr client);
        [PreserveSig] int UnregisterEndpointNotificationCallback(IntPtr client);
    }

    [ComImport]
    [Guid("0BD7A1BE-7A1A-44DB-8397-CC5392387B5E")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IMMDeviceCollection
    {
        [PreserveSig] int GetCount(out uint count);
        [PreserveSig] int Item(uint index, out IMMDevice device);
    }

    [ComImport]
    [Guid("D666063F-1587-4E43-81F1-B948E807363F")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IMMDevice
    {
        [PreserveSig] int Activate(ref Guid interfaceId, uint classContext, IntPtr activationParameters,
            [MarshalAs(UnmanagedType.IUnknown)] out object activatedInterface);
        [PreserveSig] int OpenPropertyStore(uint storageAccess, out IntPtr properties);
        [PreserveSig] int GetId([MarshalAs(UnmanagedType.LPWStr)] out string id);
        [PreserveSig] int GetState(out uint state);
    }

    [ComImport]
    [Guid("77AA99A0-1BD6-484F-8BC7-2C654C9A9B6F")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IAudioSessionManager2
    {
        [PreserveSig] int GetAudioSessionControl(ref Guid sessionId, uint streamFlags,
            out IAudioSessionControl sessionControl);
        [PreserveSig] int GetSimpleAudioVolume(ref Guid sessionId, uint streamFlags,
            out ISimpleAudioVolume audioVolume);
        [PreserveSig] int GetSessionEnumerator(out IAudioSessionEnumerator sessionEnumerator);
        [PreserveSig] int RegisterSessionNotification(IntPtr sessionNotification);
        [PreserveSig] int UnregisterSessionNotification(IntPtr sessionNotification);
        [PreserveSig] int RegisterDuckNotification([MarshalAs(UnmanagedType.LPWStr)] string sessionId,
            [MarshalAs(UnmanagedType.IUnknown)] object duckNotification);
        [PreserveSig] int UnregisterDuckNotification([MarshalAs(UnmanagedType.IUnknown)] object duckNotification);
    }

    [ComImport]
    [Guid("E2F5BB11-0570-40CA-ACDD-3AA01277DEE8")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IAudioSessionEnumerator
    {
        [PreserveSig] int GetCount(out int sessionCount);
        [PreserveSig] int GetSession(int sessionIndex, out IAudioSessionControl sessionControl);
    }

    [ComImport]
    [Guid("F4B1A599-7266-4319-A8CA-E70ACB11E8CD")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IAudioSessionControl
    {
        [PreserveSig] int GetState(out AudioSessionState state);
        [PreserveSig] int GetDisplayName([MarshalAs(UnmanagedType.LPWStr)] out string displayName);
        [PreserveSig] int SetDisplayName([MarshalAs(UnmanagedType.LPWStr)] string displayName, ref Guid eventContext);
        [PreserveSig] int GetIconPath([MarshalAs(UnmanagedType.LPWStr)] out string iconPath);
        [PreserveSig] int SetIconPath([MarshalAs(UnmanagedType.LPWStr)] string iconPath, ref Guid eventContext);
        [PreserveSig] int GetGroupingParam(out Guid groupingId);
        [PreserveSig] int SetGroupingParam(ref Guid groupingId, ref Guid eventContext);
        [PreserveSig] int RegisterAudioSessionNotification([MarshalAs(UnmanagedType.IUnknown)] object sessionEvents);
        [PreserveSig] int UnregisterAudioSessionNotification([MarshalAs(UnmanagedType.IUnknown)] object sessionEvents);
    }

    [ComImport]
    [Guid("BFB7FF88-7239-4FC9-8FA2-07C950BE9C6D")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IAudioSessionControl2
    {
        [PreserveSig] int GetState(out AudioSessionState state);
        [PreserveSig] int GetDisplayName([MarshalAs(UnmanagedType.LPWStr)] out string displayName);
        [PreserveSig] int SetDisplayName([MarshalAs(UnmanagedType.LPWStr)] string displayName, ref Guid eventContext);
        [PreserveSig] int GetIconPath([MarshalAs(UnmanagedType.LPWStr)] out string iconPath);
        [PreserveSig] int SetIconPath([MarshalAs(UnmanagedType.LPWStr)] string iconPath, ref Guid eventContext);
        [PreserveSig] int GetGroupingParam(out Guid groupingId);
        [PreserveSig] int SetGroupingParam(ref Guid groupingId, ref Guid eventContext);
        [PreserveSig] int RegisterAudioSessionNotification([MarshalAs(UnmanagedType.IUnknown)] object sessionEvents);
        [PreserveSig] int UnregisterAudioSessionNotification([MarshalAs(UnmanagedType.IUnknown)] object sessionEvents);
        [PreserveSig] int GetSessionIdentifier([MarshalAs(UnmanagedType.LPWStr)] out string sessionIdentifier);
        [PreserveSig] int GetSessionInstanceIdentifier(
            [MarshalAs(UnmanagedType.LPWStr)] out string sessionInstanceIdentifier);
        [PreserveSig] int GetProcessId(out uint processId);
        [PreserveSig] int IsSystemSoundsSession();
        [PreserveSig] int SetDuckingPreference([MarshalAs(UnmanagedType.Bool)] bool optOut);
    }

    [ComImport]
    [Guid("87CE5498-68D6-44E5-9215-6DA47EF883D8")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface ISimpleAudioVolume
    {
        [PreserveSig] int SetMasterVolume(float level, ref Guid eventContext);
        [PreserveSig] int GetMasterVolume(out float level);
        [PreserveSig] int SetMute([MarshalAs(UnmanagedType.Bool)] bool muted, ref Guid eventContext);
        [PreserveSig] int GetMute([MarshalAs(UnmanagedType.Bool)] out bool muted);
    }

    [ComImport]
    [Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")]
    private class MMDeviceEnumerator { }

    [StructLayout(LayoutKind.Sequential)]
    private struct FileTime
    {
        internal uint Low;
        internal uint High;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr OpenProcess(uint desiredAccess, bool inheritHandle, uint processId);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool GetProcessTimes(IntPtr process, out FileTime creation, out FileTime exit,
        out FileTime kernel, out FileTime user);

    [DllImport("kernel32.dll")]
    private static extern bool CloseHandle(IntPtr handle);

    private static void Check(int result)
    {
        if (result < 0) Marshal.ThrowExceptionForHR(result);
    }

    public static long CreationFileTime(uint processId)
    {
        IntPtr process = OpenProcess(ProcessQueryLimitedInformation, false, processId);
        if (process == IntPtr.Zero)
            throw new Win32Exception(Marshal.GetLastWin32Error(), "OpenProcess failed");
        try
        {
            FileTime creation;
            FileTime exit;
            FileTime kernel;
            FileTime user;
            if (!GetProcessTimes(process, out creation, out exit, out kernel, out user))
                throw new Win32Exception(Marshal.GetLastWin32Error(), "GetProcessTimes failed");
            return unchecked((long)(((ulong)creation.High << 32) | creation.Low));
        }
        finally
        {
            CloseHandle(process);
        }
    }

    public static CarbonAudioSessionSnapshot[] Find(uint targetProcessId)
    {
        var result = new List<CarbonAudioSessionSnapshot>();
        var enumerator = (IMMDeviceEnumerator)new MMDeviceEnumerator();
        IMMDeviceCollection devices;
        Check(enumerator.EnumAudioEndpoints(EDataFlow.Render, DeviceStateActive, out devices));
        uint deviceCount;
        Check(devices.GetCount(out deviceCount));
        Guid managerId = new Guid("77AA99A0-1BD6-484F-8BC7-2C654C9A9B6F");
        for (uint deviceIndex = 0; deviceIndex < deviceCount; deviceIndex++)
        {
            IMMDevice device;
            Check(devices.Item(deviceIndex, out device));
            string endpointId;
            Check(device.GetId(out endpointId));
            object activated;
            Check(device.Activate(ref managerId, ClassContextAll, IntPtr.Zero, out activated));
            IAudioSessionEnumerator sessions;
            Check(((IAudioSessionManager2)activated).GetSessionEnumerator(out sessions));
            int sessionCount;
            Check(sessions.GetCount(out sessionCount));
            for (int sessionIndex = 0; sessionIndex < sessionCount; sessionIndex++)
            {
                IAudioSessionControl control;
                if (sessions.GetSession(sessionIndex, out control) < 0 || control == null) continue;
                var control2 = (IAudioSessionControl2)control;
                uint processId;
                if (control2.GetProcessId(out processId) < 0 || processId != targetProcessId)
                {
                    Marshal.FinalReleaseComObject(control);
                    continue;
                }
                string sessionId;
                string instanceId;
                bool muted;
                Check(control2.GetSessionIdentifier(out sessionId));
                Check(control2.GetSessionInstanceIdentifier(out instanceId));
                Check(((ISimpleAudioVolume)control).GetMute(out muted));
                result.Add(new CarbonAudioSessionSnapshot {
                    EndpointId = endpointId,
                    SessionId = sessionId,
                    InstanceId = instanceId,
                    Muted = muted
                });
                Marshal.FinalReleaseComObject(control);
            }
            Marshal.FinalReleaseComObject(sessions);
            Marshal.FinalReleaseComObject(activated);
            Marshal.FinalReleaseComObject(device);
        }
        Marshal.FinalReleaseComObject(devices);
        Marshal.FinalReleaseComObject(enumerator);
        GC.Collect();
        GC.WaitForPendingFinalizers();
        return result.ToArray();
    }

    public static int SetMute(uint targetProcessId, bool shouldMute)
    {
        int changed = 0;
        var enumerator = (IMMDeviceEnumerator)new MMDeviceEnumerator();
        IMMDeviceCollection devices;
        Check(enumerator.EnumAudioEndpoints(EDataFlow.Render, DeviceStateActive, out devices));
        uint deviceCount;
        Check(devices.GetCount(out deviceCount));
        Guid managerId = new Guid("77AA99A0-1BD6-484F-8BC7-2C654C9A9B6F");
        Guid eventContext = Guid.NewGuid();
        for (uint deviceIndex = 0; deviceIndex < deviceCount; deviceIndex++)
        {
            IMMDevice device;
            Check(devices.Item(deviceIndex, out device));
            object activated;
            Check(device.Activate(ref managerId, ClassContextAll, IntPtr.Zero, out activated));
            IAudioSessionEnumerator sessions;
            Check(((IAudioSessionManager2)activated).GetSessionEnumerator(out sessions));
            int sessionCount;
            Check(sessions.GetCount(out sessionCount));
            for (int sessionIndex = 0; sessionIndex < sessionCount; sessionIndex++)
            {
                IAudioSessionControl control;
                if (sessions.GetSession(sessionIndex, out control) < 0 || control == null) continue;
                uint processId;
                if (((IAudioSessionControl2)control).GetProcessId(out processId) >= 0 &&
                    processId == targetProcessId)
                {
                    ISimpleAudioVolume volume = (ISimpleAudioVolume)control;
                    bool muted;
                    Check(volume.GetMute(out muted));
                    if (muted != shouldMute)
                    {
                        Check(volume.SetMute(shouldMute, ref eventContext));
                        changed++;
                    }
                }
                Marshal.FinalReleaseComObject(control);
            }
            Marshal.FinalReleaseComObject(sessions);
            Marshal.FinalReleaseComObject(activated);
            Marshal.FinalReleaseComObject(device);
        }
        Marshal.FinalReleaseComObject(devices);
        Marshal.FinalReleaseComObject(enumerator);
        GC.Collect();
        GC.WaitForPendingFinalizers();
        return changed;
    }
}
'@

function Invoke-WithDeadline([scriptblock]$Operation, [scriptblock]$Accept, [string]$Description) {
    $deadline = [DateTime]::UtcNow.AddSeconds(12)
    do {
        $value = & $Operation
        if (& $Accept $value) {
            return $value
        }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for $Description"
}

function Invoke-Guard([string]$Mode, [string]$Policy, [int]$TimeoutMilliseconds = 10000) {
    $arguments = @(
        '-Mta',
        '-NoProfile',
        '-NonInteractive',
        '-ExecutionPolicy',
        'Bypass',
        '-File',
        $GuardScript,
        '-Mode',
        $Mode,
        '-TargetProcessId',
        $fixture.Id.ToString(),
        '-ExecutableBase64',
        $encodedExecutable,
        '-CreationFileTime',
        $creationFileTime.ToString(),
        '-Policy',
        $Policy,
        '-ConnectTimeoutMilliseconds',
        $TimeoutMilliseconds.ToString()
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
        throw "Could not start audio guard $Mode/$Policy"
    }
    $stdout = $child.StandardOutput.ReadToEnd()
    $stderr = $child.StandardError.ReadToEnd()
    $child.WaitForExit()
    if ($child.ExitCode -ne 0) {
        throw "Audio guard $Mode/$Policy failed with exit code $($child.ExitCode): $stderr"
    }
    if ($Mode -eq 'command') {
        return ($stdout.Trim() | ConvertFrom-Json)
    }
}

function Start-AudioFixture([string]$Executable) {
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
        throw 'Could not start the audio replacement fixture'
    }
    if ($process.StandardOutput.ReadLine() -ne 'ready') {
        throw "Audio replacement fixture did not become ready: $($process.StandardError.ReadToEnd())"
    }
    return $process
}

function Stop-AudioGuard([int]$TargetProcessId) {
    $escapedScript = [Regex]::Escape($GuardScript)
    $targetPattern = '-TargetProcessId\s+"?{0}(?:"|\s)' -f $TargetProcessId
    $guards = @(Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe'" | Where-Object {
        $_.CommandLine -match $escapedScript -and
            $_.CommandLine -match '-Mode\s+"?guard"?' -and
            $_.CommandLine -match $targetPattern
    })
    if ($guards.Count -ne 1) {
        throw "Expected one audio guard for PID $TargetProcessId, found $($guards.Count)"
    }
    $guardProcessId = $guards[0].ProcessId
    Stop-Process -Id $guardProcessId -Force
    Wait-Process -Id $guardProcessId -Timeout 10 -ErrorAction SilentlyContinue
    if (@(Get-CimInstance Win32_Process -Filter "ProcessId = $guardProcessId").Count -ne 0) {
        throw "Timed out waiting for audio guard PID $guardProcessId to stop"
    }
}

$temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ("carbon-audio-replacement-" + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
$fixtureExecutable = Join-Path $temporaryDirectory 'CarbonAudioReplacementFixture.exe'
$fixture = $null
$firstProcessId = 0
$firstCreationFileTime = 0
$secondProcessId = 0
$secondCreationFileTime = 0
try {
    Add-Type -TypeDefinition $fixtureSource -Language CSharp -OutputAssembly $fixtureExecutable -OutputType ConsoleApplication
    Add-Type -TypeDefinition $probeSource -Language CSharp

    $fixture = Start-AudioFixture $fixtureExecutable
    $creationFileTime = [CarbonAudioReplacementProbe]::CreationFileTime($fixture.Id)
    $firstProcessId = $fixture.Id
    $firstCreationFileTime = $creationFileTime
    $encodedExecutable = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($fixtureExecutable))
    Invoke-WithDeadline `
        { @([CarbonAudioReplacementProbe]::Find($fixture.Id)) } `
        { param($sessions) @($sessions).Count -eq 1 } `
        'the first fixture audio session' | Out-Null

    Invoke-Guard 'spawn' 'audible'
    $muted = Invoke-Guard 'command' 'muted'
    if ($muted.matched_sessions -lt 1 -or $muted.changed_sessions -ne 1) {
        throw "Carbon did not establish ownership of the first mute: $($muted | ConvertTo-Json -Compress)"
    }
    $firstMuted = Invoke-WithDeadline `
        { @([CarbonAudioReplacementProbe]::Find($fixture.Id)) } `
        { param($sessions) @($sessions).Count -eq 1 -and @($sessions)[0].Muted } `
        'Carbon to mute the first fixture audio session'

    Stop-AudioGuard $fixture.Id
    $fixture.Kill()
    if (-not $fixture.WaitForExit(5000)) {
        throw 'The first audio fixture did not stop'
    }
    $fixture.Dispose()
    $fixture = $null

    $fixture = Start-AudioFixture $fixtureExecutable
    $creationFileTime = [CarbonAudioReplacementProbe]::CreationFileTime($fixture.Id)
    $secondProcessId = $fixture.Id
    $secondCreationFileTime = $creationFileTime
    $replacement = Invoke-WithDeadline `
        { @([CarbonAudioReplacementProbe]::Find($fixture.Id)) } `
        {
            param($sessions)
            @($sessions).Count -eq 1 -and
                @($sessions)[0].SessionId -eq @($firstMuted)[0].SessionId -and
                @($sessions)[0].InstanceId -ne @($firstMuted)[0].InstanceId
        } `
        'a replacement fixture audio-session instance'
    if (-not @($replacement)[0].Muted) {
        throw 'The replacement audio session did not inherit Carbon''s mute, so the regression was not exercised'
    }

    Invoke-Guard 'spawn' 'audible'
    $audible = Invoke-Guard 'command' 'audible'
    $audibleJson = $audible | ConvertTo-Json -Compress
    $restored = Invoke-WithDeadline `
        { @([CarbonAudioReplacementProbe]::Find($fixture.Id)) } `
        { param($sessions) @($sessions).Count -eq 1 -and -not @($sessions)[0].Muted } `
        "Carbon to restore the replacement audio session after acknowledging $audibleJson"

    if ([CarbonAudioReplacementProbe]::SetMute($fixture.Id, $true) -ne 1) {
        throw 'The fixture could not establish a user-owned mute'
    }
    Invoke-WithDeadline `
        { @([CarbonAudioReplacementProbe]::Find($fixture.Id)) } `
        { param($sessions) @($sessions).Count -eq 1 -and @($sessions)[0].Muted } `
        'the fixture user mute to become visible' | Out-Null
    $userAudible = Invoke-Guard 'command' 'audible'
    $afterUserAudible = @([CarbonAudioReplacementProbe]::Find($fixture.Id))
    if ($afterUserAudible.Count -ne 1 -or -not $afterUserAudible[0].Muted) {
        throw "Carbon changed a user-owned mute: $($userAudible | ConvertTo-Json -Compress)"
    }
    [CarbonAudioReplacementProbe]::SetMute($fixture.Id, $false) | Out-Null

    [pscustomobject]@{
        policy = $audible.policy
        matched_sessions = $audible.matched_sessions
        changed_sessions = $audible.changed_sessions
        stable_session_preserved = @($restored)[0].SessionId -eq @($firstMuted)[0].SessionId
        instance_replaced = @($restored)[0].InstanceId -ne @($firstMuted)[0].InstanceId
        process_replaced = $fixture.Id -ne $firstProcessId
        muted_after_restore = @($restored)[0].Muted
        remaining_owned_mutes = $audible.remaining_owned_mutes
        failed_sessions = $audible.failed_sessions
        user_mute_preserved = $afterUserAudible[0].Muted
    } | ConvertTo-Json -Compress
} finally {
    if ($null -ne $fixture) {
        try {
            if (-not $fixture.HasExited) {
                $fixture.StandardInput.WriteLine('exit')
                $fixture.StandardInput.Flush()
                if (-not $fixture.WaitForExit(5000)) {
                    $fixture.Kill()
                    $fixture.WaitForExit()
                }
            }
        } catch {
            try { $fixture.Kill() } catch { }
        }
        $fixture.Dispose()
    }
    Start-Sleep -Milliseconds 250
    if ($firstProcessId -ne 0 -and $firstCreationFileTime -ne 0) {
        $ledger = Join-Path $env:LOCALAPPDATA "Carbon\audio-guards\$firstProcessId-$firstCreationFileTime.owned"
        Remove-Item -LiteralPath $ledger -Force -ErrorAction SilentlyContinue
    }
    if ($secondProcessId -ne 0 -and $secondCreationFileTime -ne 0) {
        $ledger = Join-Path $env:LOCALAPPDATA "Carbon\audio-guards\$secondProcessId-$secondCreationFileTime.owned"
        Remove-Item -LiteralPath $ledger -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
}
