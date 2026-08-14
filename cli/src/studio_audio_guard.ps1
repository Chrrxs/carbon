[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('guard', 'spawn', 'command', 'compile')]
    [string]$Mode,

    [uint32]$TargetProcessId = 0,
    [string]$ExecutableBase64 = '',
    [int64]$CreationFileTime = 0,

    [ValidateSet('muted', 'audible')]
    [string]$Policy = 'muted',

    [int]$ConnectTimeoutMilliseconds = 5000
)

$ErrorActionPreference = 'Stop'

function Get-ExpectedExecutable {
    if ([string]::IsNullOrEmpty($ExecutableBase64)) {
        throw 'Carbon Studio audio guard requires an executable identity'
    }
    [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($ExecutableBase64))
}

function Assert-ExactProcess([string]$ExpectedExecutable) {
    $process = Get-Process -Id $TargetProcessId -ErrorAction SilentlyContinue
    if ($null -eq $process) {
        throw "Roblox Studio process $TargetProcessId is no longer running"
    }
    $process.Refresh()
    if (-not [string]::Equals($process.Path, $ExpectedExecutable, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Roblox Studio process $TargetProcessId path no longer matches"
    }
    if ($process.StartTime.ToUniversalTime().ToFileTimeUtc() -ne $CreationFileTime) {
        throw "Roblox Studio process $TargetProcessId creation time no longer matches"
    }
}

if ($Mode -eq 'command') {
    $expectedExecutable = Get-ExpectedExecutable
    Assert-ExactProcess $expectedExecutable
    $pipeName = "carbon-studio-audio-$TargetProcessId-$CreationFileTime"
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
            throw "Carbon Studio audio guard did not become ready for PID $TargetProcessId within $ConnectTimeoutMilliseconds ms"
        }
        $writer = [IO.StreamWriter]::new($pipe, [Text.UTF8Encoding]::new($false), 1024, $true)
        $reader = [IO.StreamReader]::new($pipe, [Text.UTF8Encoding]::new($false), $false, 1024, $true)
        try {
            $writer.AutoFlush = $true
            $writer.WriteLine($Policy)
            $response = $reader.ReadLine()
            if ([string]::IsNullOrEmpty($response)) {
                throw 'Carbon Studio audio guard returned no policy acknowledgement'
            }
            $report = ConvertFrom-Json -InputObject $response
            if ($report.policy -ne $Policy) {
                throw "Carbon Studio audio guard acknowledged unexpected policy '$($report.policy)'"
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
    $quotedScript = '"' + $PSCommandPath + '"'
    $arguments = @(
        '-Mta',
        '-NoProfile',
        '-NonInteractive',
        '-ExecutionPolicy',
        'Bypass',
        '-File',
        $quotedScript,
        '-Mode',
        'guard',
        '-TargetProcessId',
        $TargetProcessId.ToString(),
        '-ExecutableBase64',
        $ExecutableBase64,
        '-CreationFileTime',
        $CreationFileTime.ToString(),
        '-Policy',
        $Policy
    )
    $start = @{
        FilePath = (Join-Path $PSHOME 'powershell.exe')
        ArgumentList = $arguments
        WindowStyle = 'Hidden'
    }
    Start-Process @start | Out-Null
    exit 0
}

$source = @'
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

namespace CarbonStudioAudioGuard
{
    internal enum EDataFlow
    {
        Render = 0,
        Capture = 1,
        All = 2
    }

    internal enum ERole
    {
        Console = 0,
        Multimedia = 1,
        Communications = 2
    }

    internal enum AudioSessionState
    {
        Inactive = 0,
        Active = 1,
        Expired = 2
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct PropertyKey
    {
        internal Guid FormatId;
        internal uint PropertyId;
    }

    [ComImport]
    [Guid("A95664D2-9614-4F35-A746-DE8DB63617E6")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IMMDeviceEnumerator
    {
        [PreserveSig]
        int EnumAudioEndpoints(EDataFlow dataFlow, uint stateMask, out IMMDeviceCollection devices);

        [PreserveSig]
        int GetDefaultAudioEndpoint(EDataFlow dataFlow, ERole role, out IMMDevice device);

        [PreserveSig]
        int GetDevice([MarshalAs(UnmanagedType.LPWStr)] string id, out IMMDevice device);

        [PreserveSig]
        int RegisterEndpointNotificationCallback(IMMNotificationClient client);

        [PreserveSig]
        int UnregisterEndpointNotificationCallback(IMMNotificationClient client);
    }

    [ComImport]
    [Guid("0BD7A1BE-7A1A-44DB-8397-CC5392387B5E")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IMMDeviceCollection
    {
        [PreserveSig]
        int GetCount(out uint count);

        [PreserveSig]
        int Item(uint index, out IMMDevice device);
    }

    [ComImport]
    [Guid("D666063F-1587-4E43-81F1-B948E807363F")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IMMDevice
    {
        [PreserveSig]
        int Activate(ref Guid interfaceId, uint classContext, IntPtr activationParameters,
            [MarshalAs(UnmanagedType.IUnknown)] out object activatedInterface);

        [PreserveSig]
        int OpenPropertyStore(uint storageAccess, out IntPtr properties);

        [PreserveSig]
        int GetId([MarshalAs(UnmanagedType.LPWStr)] out string id);

        [PreserveSig]
        int GetState(out uint state);
    }

    [Guid("7991EEC9-7E89-4D85-8390-6C703CEC60C0")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IMMNotificationClient
    {
        [PreserveSig]
        int OnDeviceStateChanged([MarshalAs(UnmanagedType.LPWStr)] string deviceId, uint newState);

        [PreserveSig]
        int OnDeviceAdded([MarshalAs(UnmanagedType.LPWStr)] string deviceId);

        [PreserveSig]
        int OnDeviceRemoved([MarshalAs(UnmanagedType.LPWStr)] string deviceId);

        [PreserveSig]
        int OnDefaultDeviceChanged(EDataFlow flow, ERole role,
            [MarshalAs(UnmanagedType.LPWStr)] string defaultDeviceId);

        [PreserveSig]
        int OnPropertyValueChanged([MarshalAs(UnmanagedType.LPWStr)] string deviceId, PropertyKey key);
    }

    [ComImport]
    [Guid("77AA99A0-1BD6-484F-8BC7-2C654C9A9B6F")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IAudioSessionManager2
    {
        [PreserveSig]
        int GetAudioSessionControl(ref Guid sessionId, uint streamFlags, out IAudioSessionControl sessionControl);

        [PreserveSig]
        int GetSimpleAudioVolume(ref Guid sessionId, uint streamFlags, out ISimpleAudioVolume audioVolume);

        [PreserveSig]
        int GetSessionEnumerator(out IAudioSessionEnumerator sessionEnumerator);

        [PreserveSig]
        int RegisterSessionNotification(IAudioSessionNotification sessionNotification);

        [PreserveSig]
        int UnregisterSessionNotification(IAudioSessionNotification sessionNotification);

        [PreserveSig]
        int RegisterDuckNotification([MarshalAs(UnmanagedType.LPWStr)] string sessionId,
            [MarshalAs(UnmanagedType.IUnknown)] object duckNotification);

        [PreserveSig]
        int UnregisterDuckNotification([MarshalAs(UnmanagedType.IUnknown)] object duckNotification);
    }

    [ComImport]
    [Guid("E2F5BB11-0570-40CA-ACDD-3AA01277DEE8")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IAudioSessionEnumerator
    {
        [PreserveSig]
        int GetCount(out int sessionCount);

        [PreserveSig]
        int GetSession(int sessionIndex, out IAudioSessionControl sessionControl);
    }

    [ComImport]
    [Guid("F4B1A599-7266-4319-A8CA-E70ACB11E8CD")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IAudioSessionControl
    {
        [PreserveSig]
        int GetState(out AudioSessionState state);

        [PreserveSig]
        int GetDisplayName([MarshalAs(UnmanagedType.LPWStr)] out string displayName);

        [PreserveSig]
        int SetDisplayName([MarshalAs(UnmanagedType.LPWStr)] string displayName, ref Guid eventContext);

        [PreserveSig]
        int GetIconPath([MarshalAs(UnmanagedType.LPWStr)] out string iconPath);

        [PreserveSig]
        int SetIconPath([MarshalAs(UnmanagedType.LPWStr)] string iconPath, ref Guid eventContext);

        [PreserveSig]
        int GetGroupingParam(out Guid groupingId);

        [PreserveSig]
        int SetGroupingParam(ref Guid groupingId, ref Guid eventContext);

        [PreserveSig]
        int RegisterAudioSessionNotification([MarshalAs(UnmanagedType.IUnknown)] object sessionEvents);

        [PreserveSig]
        int UnregisterAudioSessionNotification([MarshalAs(UnmanagedType.IUnknown)] object sessionEvents);
    }

    [ComImport]
    [Guid("BFB7FF88-7239-4FC9-8FA2-07C950BE9C6D")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IAudioSessionControl2
    {
        [PreserveSig]
        int GetState(out AudioSessionState state);

        [PreserveSig]
        int GetDisplayName([MarshalAs(UnmanagedType.LPWStr)] out string displayName);

        [PreserveSig]
        int SetDisplayName([MarshalAs(UnmanagedType.LPWStr)] string displayName, ref Guid eventContext);

        [PreserveSig]
        int GetIconPath([MarshalAs(UnmanagedType.LPWStr)] out string iconPath);

        [PreserveSig]
        int SetIconPath([MarshalAs(UnmanagedType.LPWStr)] string iconPath, ref Guid eventContext);

        [PreserveSig]
        int GetGroupingParam(out Guid groupingId);

        [PreserveSig]
        int SetGroupingParam(ref Guid groupingId, ref Guid eventContext);

        [PreserveSig]
        int RegisterAudioSessionNotification([MarshalAs(UnmanagedType.IUnknown)] object sessionEvents);

        [PreserveSig]
        int UnregisterAudioSessionNotification([MarshalAs(UnmanagedType.IUnknown)] object sessionEvents);

        [PreserveSig]
        int GetSessionIdentifier([MarshalAs(UnmanagedType.LPWStr)] out string sessionIdentifier);

        [PreserveSig]
        int GetSessionInstanceIdentifier([MarshalAs(UnmanagedType.LPWStr)] out string sessionInstanceIdentifier);

        [PreserveSig]
        int GetProcessId(out uint processId);

        [PreserveSig]
        int IsSystemSoundsSession();

        [PreserveSig]
        int SetDuckingPreference([MarshalAs(UnmanagedType.Bool)] bool optOut);
    }

    [ComImport]
    [Guid("87CE5498-68D6-44E5-9215-6DA47EF883D8")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface ISimpleAudioVolume
    {
        [PreserveSig]
        int SetMasterVolume(float level, ref Guid eventContext);

        [PreserveSig]
        int GetMasterVolume(out float level);

        [PreserveSig]
        int SetMute([MarshalAs(UnmanagedType.Bool)] bool muted, ref Guid eventContext);

        [PreserveSig]
        int GetMute([MarshalAs(UnmanagedType.Bool)] out bool muted);
    }

    [Guid("641DD20B-4D41-49CC-ABA3-174B9477BB08")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IAudioSessionNotification
    {
        [PreserveSig]
        int OnSessionCreated(IAudioSessionControl newSession);
    }

    [ComImport]
    [Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")]
    internal class MMDeviceEnumerator
    {
    }

    internal sealed class AudioSessionHandle
    {
        internal readonly string EndpointId;
        internal readonly string Key;
        internal readonly IAudioSessionControl Control;
        internal readonly ISimpleAudioVolume Volume;

        internal AudioSessionHandle(string endpointId, string key, IAudioSessionControl control,
            ISimpleAudioVolume volume)
        {
            EndpointId = endpointId;
            Key = key;
            Control = control;
            Volume = volume;
        }
    }

    [ComVisible(true)]
    [ClassInterface(ClassInterfaceType.None)]
    internal sealed class SessionNotification : IAudioSessionNotification
    {
        private readonly AudioGuard guard;
        private readonly string endpointId;

        internal SessionNotification(AudioGuard guard, string endpointId)
        {
            this.guard = guard;
            this.endpointId = endpointId;
        }

        public int OnSessionCreated(IAudioSessionControl newSession)
        {
            guard.ObserveSession(endpointId, newSession);
            return 0;
        }
    }

    [ComVisible(true)]
    [ClassInterface(ClassInterfaceType.None)]
    internal sealed class EndpointNotification : IMMNotificationClient
    {
        private readonly AudioGuard guard;

        internal EndpointNotification(AudioGuard guard)
        {
            this.guard = guard;
        }

        public int OnDeviceStateChanged(string deviceId, uint newState)
        {
            guard.MarkDevicesDirty();
            return 0;
        }

        public int OnDeviceAdded(string deviceId)
        {
            guard.MarkDevicesDirty();
            return 0;
        }

        public int OnDeviceRemoved(string deviceId)
        {
            guard.MarkDevicesDirty();
            return 0;
        }

        public int OnDefaultDeviceChanged(EDataFlow flow, ERole role, string defaultDeviceId)
        {
            guard.MarkDevicesDirty();
            return 0;
        }

        public int OnPropertyValueChanged(string deviceId, PropertyKey key)
        {
            return 0;
        }
    }

    internal sealed class EndpointRegistration : IDisposable
    {
        private readonly AudioGuard guard;
        private readonly IAudioSessionManager2 manager;
        private readonly SessionNotification notification;

        internal readonly string Id;

        internal EndpointRegistration(AudioGuard guard, string id, IAudioSessionManager2 manager)
        {
            this.guard = guard;
            Id = id;
            this.manager = manager;
            notification = new SessionNotification(guard, id);
            AudioGuard.Check(manager.RegisterSessionNotification(notification));
            RefreshSessions();
        }

        internal void RefreshSessions()
        {
            IAudioSessionEnumerator sessions;
            AudioGuard.Check(manager.GetSessionEnumerator(out sessions));
            int count;
            AudioGuard.Check(sessions.GetCount(out count));
            for (int index = 0; index < count; index++)
            {
                IAudioSessionControl session;
                if (sessions.GetSession(index, out session) >= 0 && session != null)
                {
                    guard.ObserveSession(Id, session);
                }
            }
        }

        public void Dispose()
        {
            try
            {
                manager.UnregisterSessionNotification(notification);
            }
            catch
            {
            }
        }
    }

    internal sealed class AudioGuard : IDisposable
    {
        private const uint DeviceStateActive = 0x00000001;
        private const uint ClassContextAll = 0x00000017;
        private static readonly Guid AudioSessionManager2Id =
            new Guid("77AA99A0-1BD6-484F-8BC7-2C654C9A9B6F");

        private readonly object sync = new object();
        private readonly uint targetProcessId;
        private readonly string ledgerPath;
        private readonly IMMDeviceEnumerator deviceEnumerator;
        private readonly EndpointNotification endpointNotification;
        private readonly Dictionary<string, EndpointRegistration> endpoints =
            new Dictionary<string, EndpointRegistration>(StringComparer.Ordinal);
        private readonly Dictionary<string, AudioSessionHandle> sessions =
            new Dictionary<string, AudioSessionHandle>(StringComparer.Ordinal);
        private readonly HashSet<string> carbonOwnedMutes =
            new HashSet<string>(StringComparer.Ordinal);
        private Guid eventContext = new Guid("7552A64B-A35C-4D4E-84B0-A2FB1DD81B02");
        private bool muted;
        private int devicesDirty = 1;
        private DateTime lastSessionRefresh = DateTime.MinValue;

        internal AudioGuard(uint targetProcessId, long creationFileTime, bool initiallyMuted)
        {
            this.targetProcessId = targetProcessId;
            muted = initiallyMuted;
            string stateDirectory = Path.Combine(
                Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
                "Carbon",
                "audio-guards");
            Directory.CreateDirectory(stateDirectory);
            ledgerPath = Path.Combine(
                stateDirectory,
                targetProcessId.ToString() + "-" + creationFileTime.ToString() + ".owned");
            LoadLedger();
            deviceEnumerator = (IMMDeviceEnumerator)new MMDeviceEnumerator();
            endpointNotification = new EndpointNotification(this);
            Check(deviceEnumerator.RegisterEndpointNotificationCallback(endpointNotification));
            RefreshEndpoints();
        }

        internal static void Check(int result)
        {
            if (result < 0)
            {
                Marshal.ThrowExceptionForHR(result);
            }
        }

        internal void MarkDevicesDirty()
        {
            Interlocked.Exchange(ref devicesDirty, 1);
        }

        internal void ObserveSession(string endpointId, IAudioSessionControl control)
        {
            try
            {
                IAudioSessionControl2 control2 = (IAudioSessionControl2)control;
                uint processId;
                Check(control2.GetProcessId(out processId));
                if (processId != targetProcessId)
                {
                    return;
                }

                string instanceId;
                Check(control2.GetSessionInstanceIdentifier(out instanceId));
                string key = endpointId + "\n" + instanceId;
                ISimpleAudioVolume volume = (ISimpleAudioVolume)control;
                lock (sync)
                {
                    AudioSessionHandle handle;
                    if (!sessions.TryGetValue(key, out handle))
                    {
                        handle = new AudioSessionHandle(endpointId, key, control, volume);
                        sessions.Add(key, handle);
                    }
                    ApplyPolicy(handle);
                }
            }
            catch
            {
            }
        }

        internal void Pulse()
        {
            if (Interlocked.Exchange(ref devicesDirty, 0) != 0)
            {
                try
                {
                    RefreshEndpoints();
                }
                catch
                {
                    MarkDevicesDirty();
                }
            }

            if ((DateTime.UtcNow - lastSessionRefresh).TotalSeconds >= 1)
            {
                lastSessionRefresh = DateTime.UtcNow;
                List<EndpointRegistration> snapshot;
                lock (sync)
                {
                    snapshot = new List<EndpointRegistration>(endpoints.Values);
                }
                foreach (EndpointRegistration endpoint in snapshot)
                {
                    try
                    {
                        endpoint.RefreshSessions();
                    }
                    catch
                    {
                        MarkDevicesDirty();
                    }
                }
            }
        }

        internal string SetPolicy(bool shouldMute)
        {
            RefreshEndpoints();
            List<EndpointRegistration> endpointSnapshot;
            lock (sync)
            {
                endpointSnapshot = new List<EndpointRegistration>(endpoints.Values);
            }
            foreach (EndpointRegistration endpoint in endpointSnapshot)
            {
                try
                {
                    endpoint.RefreshSessions();
                }
                catch
                {
                    MarkDevicesDirty();
                }
            }

            int changed = 0;
            int matched;
            lock (sync)
            {
                muted = shouldMute;
                foreach (AudioSessionHandle session in sessions.Values)
                {
                    if (ApplyPolicy(session))
                    {
                        changed++;
                    }
                }
                matched = sessions.Count;
            }
            return "{\"policy\":\"" + (shouldMute ? "muted" : "audible") +
                "\",\"matched_sessions\":" + matched.ToString() +
                ",\"changed_sessions\":" + changed.ToString() + "}";
        }

        private bool ApplyPolicy(AudioSessionHandle session)
        {
            try
            {
                bool isMuted;
                Check(session.Volume.GetMute(out isMuted));
                if (muted)
                {
                    if (!isMuted)
                    {
                        Check(session.Volume.SetMute(true, ref eventContext));
                        if (carbonOwnedMutes.Add(session.Key))
                        {
                            PersistLedger();
                        }
                        return true;
                    }
                    return false;
                }

                if (!carbonOwnedMutes.Contains(session.Key))
                {
                    return false;
                }
                if (isMuted)
                {
                    Check(session.Volume.SetMute(false, ref eventContext));
                }
                carbonOwnedMutes.Remove(session.Key);
                PersistLedger();
                return isMuted;
            }
            catch
            {
                return false;
            }
        }

        private void RefreshEndpoints()
        {
            IMMDeviceCollection collection;
            Check(deviceEnumerator.EnumAudioEndpoints(EDataFlow.Render, DeviceStateActive, out collection));
            uint count;
            Check(collection.GetCount(out count));
            HashSet<string> active = new HashSet<string>(StringComparer.Ordinal);

            for (uint index = 0; index < count; index++)
            {
                IMMDevice device;
                Check(collection.Item(index, out device));
                string id;
                Check(device.GetId(out id));
                active.Add(id);

                lock (sync)
                {
                    if (endpoints.ContainsKey(id))
                    {
                        continue;
                    }
                }

                object activated;
                Guid managerId = AudioSessionManager2Id;
                Check(device.Activate(ref managerId, ClassContextAll, IntPtr.Zero, out activated));
                EndpointRegistration registration =
                    new EndpointRegistration(this, id, (IAudioSessionManager2)activated);
                lock (sync)
                {
                    if (!endpoints.ContainsKey(id))
                    {
                        endpoints.Add(id, registration);
                        registration = null;
                    }
                }
                if (registration != null)
                {
                    registration.Dispose();
                }
            }

            List<EndpointRegistration> removed = new List<EndpointRegistration>();
            lock (sync)
            {
                List<string> removedIds = new List<string>();
                foreach (KeyValuePair<string, EndpointRegistration> pair in endpoints)
                {
                    if (!active.Contains(pair.Key))
                    {
                        removedIds.Add(pair.Key);
                        removed.Add(pair.Value);
                    }
                }
                foreach (string id in removedIds)
                {
                    endpoints.Remove(id);
                }

                List<string> removedSessions = new List<string>();
                foreach (KeyValuePair<string, AudioSessionHandle> pair in sessions)
                {
                    if (!active.Contains(pair.Value.EndpointId))
                    {
                        removedSessions.Add(pair.Key);
                    }
                }
                foreach (string key in removedSessions)
                {
                    sessions.Remove(key);
                }
            }
            foreach (EndpointRegistration endpoint in removed)
            {
                endpoint.Dispose();
            }
        }

        private void LoadLedger()
        {
            if (!File.Exists(ledgerPath))
            {
                return;
            }
            foreach (string encoded in File.ReadAllLines(ledgerPath))
            {
                try
                {
                    carbonOwnedMutes.Add(Encoding.UTF8.GetString(Convert.FromBase64String(encoded)));
                }
                catch
                {
                }
            }
        }

        private void PersistLedger()
        {
            if (carbonOwnedMutes.Count == 0)
            {
                try
                {
                    File.Delete(ledgerPath);
                }
                catch
                {
                }
                return;
            }

            List<string> encoded = new List<string>();
            foreach (string key in carbonOwnedMutes)
            {
                encoded.Add(Convert.ToBase64String(Encoding.UTF8.GetBytes(key)));
            }
            string temporary = ledgerPath + "." + Guid.NewGuid().ToString("N") + ".tmp";
            File.WriteAllLines(temporary, encoded.ToArray(), new UTF8Encoding(false));
            if (File.Exists(ledgerPath))
            {
                File.Replace(temporary, ledgerPath, null);
            }
            else
            {
                File.Move(temporary, ledgerPath);
            }
        }

        public void Dispose()
        {
            try
            {
                SetPolicy(false);
            }
            catch
            {
            }
            try
            {
                deviceEnumerator.UnregisterEndpointNotificationCallback(endpointNotification);
            }
            catch
            {
            }
            List<EndpointRegistration> snapshot;
            lock (sync)
            {
                snapshot = new List<EndpointRegistration>(endpoints.Values);
                endpoints.Clear();
                sessions.Clear();
            }
            foreach (EndpointRegistration endpoint in snapshot)
            {
                endpoint.Dispose();
            }
        }
    }

    public static class Program
    {
        private static bool IsExactProcess(uint processId, string expectedExecutable, long creationFileTime)
        {
            try
            {
                using (Process process = Process.GetProcessById((int)processId))
                {
                    return string.Equals(
                            process.MainModule.FileName,
                            expectedExecutable,
                            StringComparison.OrdinalIgnoreCase) &&
                        process.StartTime.ToUniversalTime().ToFileTimeUtc() == creationFileTime &&
                        !process.HasExited;
                }
            }
            catch
            {
                return false;
            }
        }

        public static void Run(uint processId, string expectedExecutable, long creationFileTime,
            string initialPolicy)
        {
            string identity = processId.ToString() + "-" + creationFileTime.ToString();
            string pipeName = "carbon-studio-audio-" + identity;
            bool ownsMutex = false;
            using (Mutex singleton = new Mutex(true, "Local\\CarbonStudioAudio-" + identity, out ownsMutex))
            {
                if (!ownsMutex)
                {
                    return;
                }
                try
                {
                    if (!IsExactProcess(processId, expectedExecutable, creationFileTime))
                    {
                        return;
                    }
                    using (AudioGuard guard = new AudioGuard(
                        processId,
                        creationFileTime,
                        string.Equals(initialPolicy, "muted", StringComparison.Ordinal)))
                    {
                        while (IsExactProcess(processId, expectedExecutable, creationFileTime))
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
                                    guard.Pulse();
                                    if (!IsExactProcess(processId, expectedExecutable, creationFileTime))
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
                                    if (string.Equals(command, "muted", StringComparison.Ordinal))
                                    {
                                        writer.WriteLine(guard.SetPolicy(true));
                                    }
                                    else if (string.Equals(command, "audible", StringComparison.Ordinal))
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

Add-Type -TypeDefinition $source -Language CSharp
if ($Mode -eq 'compile') {
    Write-Output 'ok'
    exit 0
}

$expectedExecutable = Get-ExpectedExecutable
Assert-ExactProcess $expectedExecutable
[CarbonStudioAudioGuard.Program]::Run(
    $TargetProcessId,
    $expectedExecutable,
    $CreationFileTime,
    $Policy
)
