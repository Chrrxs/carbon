[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("focus", "assert", "assert-not", "watch", "wait", "signal")]
    [string] $Action,

    [Parameter(Mandatory = $true)]
    [uint32] $StudioProcessId,

    [uint32] $ForbiddenProcessId = 0,

    [string] $ReadyPath = "",

    [string] $DonePath = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ($Action -eq "wait") {
    if ([string]::IsNullOrWhiteSpace($ReadyPath)) {
        throw "wait requires a ready path"
    }

    $readyDeadline = [DateTime]::UtcNow.AddSeconds(30)
    while (-not (Test-Path -LiteralPath $ReadyPath -PathType Leaf) -and
        [DateTime]::UtcNow -lt $readyDeadline) {
        Start-Sleep -Milliseconds 20
    }
    if (-not (Test-Path -LiteralPath $ReadyPath -PathType Leaf)) {
        throw "Foreground event watcher did not become ready"
    }

    Write-Output "Foreground event watcher is ready"
    exit 0
}

if ($Action -eq "signal") {
    if ([string]::IsNullOrWhiteSpace($DonePath)) {
        throw "signal requires a done path"
    }

    Set-Content -LiteralPath $DonePath -Value "done" -NoNewline
    Write-Output "Foreground event watch completion signaled"
    exit 0
}

Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Threading;

public static class CarbonForegroundProbe {
    private const uint EVENT_SYSTEM_FOREGROUND = 3;
    private const uint WINEVENT_OUTOFCONTEXT = 0;
    private const uint PM_REMOVE = 1;

    private delegate void WinEventDelegate(
        IntPtr hook,
        uint eventType,
        IntPtr window,
        int objectId,
        int childId,
        uint eventThread,
        uint eventTime
    );

    [StructLayout(LayoutKind.Sequential)]
    private struct Point {
        public int X;
        public int Y;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct Message {
        public IntPtr Window;
        public uint Id;
        public UIntPtr WParam;
        public IntPtr LParam;
        public uint Time;
        public Point Cursor;
    }

    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();

    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);

    [DllImport("kernel32.dll")]
    public static extern uint GetCurrentThreadId();

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool AttachThreadInput(uint attachThreadId, uint attachToThreadId, bool attach);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool ShowWindowAsync(IntPtr window, int command);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool BringWindowToTop(IntPtr window);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool SetForegroundWindow(IntPtr window);

    [DllImport("user32.dll")]
    private static extern IntPtr SetWinEventHook(
        uint eventMin,
        uint eventMax,
        IntPtr eventHookModule,
        WinEventDelegate callback,
        uint processId,
        uint threadId,
        uint flags
    );

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
        uint removeMessage
    );

    [DllImport("user32.dll")]
    private static extern IntPtr DispatchMessage(ref Message message);

    public static int WatchForbiddenForeground(
        uint expectedProcessId,
        uint forbiddenProcessId,
        string readyPath,
        string donePath,
        int timeoutMilliseconds
    ) {
        bool sawForbiddenProcess = false;
        WinEventDelegate callback = delegate(
            IntPtr hook,
            uint eventType,
            IntPtr window,
            int objectId,
            int childId,
            uint eventThread,
            uint eventTime
        ) {
            uint processId;
            GetWindowThreadProcessId(window, out processId);
            if (processId == forbiddenProcessId)
                sawForbiddenProcess = true;
        };

        IntPtr eventHook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            IntPtr.Zero,
            callback,
            0,
            0,
            WINEVENT_OUTOFCONTEXT
        );
        if (eventHook == IntPtr.Zero)
            return 4;

        try {
            uint foregroundProcessId;
            GetWindowThreadProcessId(GetForegroundWindow(), out foregroundProcessId);
            if (foregroundProcessId != expectedProcessId)
                return 3;

            if (File.Exists(readyPath))
                File.Delete(readyPath);
            if (File.Exists(donePath))
                File.Delete(donePath);
            File.WriteAllText(readyPath, "ready");

            DateTime timeout = DateTime.UtcNow.AddMilliseconds(timeoutMilliseconds);
            DateTime? doneDeadline = null;
            while (DateTime.UtcNow < timeout) {
                Message message;
                while (PeekMessage(out message, IntPtr.Zero, 0, 0, PM_REMOVE))
                    DispatchMessage(ref message);

                if (sawForbiddenProcess)
                    return 1;

                if (!doneDeadline.HasValue && File.Exists(donePath))
                    doneDeadline = DateTime.UtcNow.AddMilliseconds(500);
                if (doneDeadline.HasValue && DateTime.UtcNow >= doneDeadline.Value)
                    return 0;

                Thread.Sleep(1);
            }

            return 2;
        }
        finally {
            UnhookWinEvent(eventHook);
            GC.KeepAlive(callback);
        }
    }
}
'@

function Get-ForegroundProcessId {
    $foregroundProcessId = [uint32] 0
    $window = [CarbonForegroundProbe]::GetForegroundWindow()
    [CarbonForegroundProbe]::GetWindowThreadProcessId($window, [ref] $foregroundProcessId) | Out-Null
    return $foregroundProcessId
}

$studio = Get-Process -Id $StudioProcessId -ErrorAction Stop
if ($studio.ProcessName -ne "RobloxStudioBeta") {
    throw "Process $StudioProcessId is not Roblox Studio"
}

if ($Action -eq "watch") {
    if ($ForbiddenProcessId -eq 0 -or [string]::IsNullOrWhiteSpace($ReadyPath) -or
        [string]::IsNullOrWhiteSpace($DonePath)) {
        throw "watch requires a forbidden Studio process and ready/done paths"
    }

    $forbiddenStudio = Get-Process -Id $ForbiddenProcessId -ErrorAction Stop
    if ($forbiddenStudio.ProcessName -ne "RobloxStudioBeta") {
        throw "Process $ForbiddenProcessId is not Roblox Studio"
    }
}

if ($Action -eq "focus") {
    $studio.Refresh()
    $studioWindow = $studio.MainWindowHandle
    if ($studioWindow -eq [IntPtr]::Zero) {
        throw "Roblox Studio process $StudioProcessId does not have a main window"
    }

    # SetForegroundWindow is intentionally restricted when another process owns
    # the foreground. Join that window's input queue for this short operation so
    # the release regression can deterministically establish its precondition.
    $foregroundWindow = [CarbonForegroundProbe]::GetForegroundWindow()
    $foregroundProcessId = [uint32] 0
    $foregroundThreadId = [CarbonForegroundProbe]::GetWindowThreadProcessId(
        $foregroundWindow,
        [ref] $foregroundProcessId
    )
    $currentThreadId = [CarbonForegroundProbe]::GetCurrentThreadId()
    $inputAttached = $false

    try {
        if ($foregroundThreadId -ne 0 -and $foregroundThreadId -ne $currentThreadId) {
            $inputAttached = [CarbonForegroundProbe]::AttachThreadInput(
                $currentThreadId,
                $foregroundThreadId,
                $true
            )
            if (-not $inputAttached) {
                throw "Could not attach to the current foreground input queue"
            }
        }

        # SW_RESTORE makes this work even if the secondary Studio was minimized.
        [CarbonForegroundProbe]::ShowWindowAsync($studioWindow, 9) | Out-Null
        [CarbonForegroundProbe]::BringWindowToTop($studioWindow) | Out-Null
        [CarbonForegroundProbe]::SetForegroundWindow($studioWindow) | Out-Null
    }
    finally {
        if ($inputAttached) {
            [CarbonForegroundProbe]::AttachThreadInput(
                $currentThreadId,
                $foregroundThreadId,
                $false
            ) | Out-Null
        }
    }

}

if ($Action -eq "assert-not") {
    $foregroundProcessId = Get-ForegroundProcessId
    if ($foregroundProcessId -eq $StudioProcessId) {
        throw "Managed test Studio process $StudioProcessId unexpectedly owns the foreground"
    }

    Write-Output "Managed test Studio process $StudioProcessId does not own the foreground"
    exit 0
}

$deadline = [DateTime]::UtcNow.AddSeconds(5)
while ((Get-ForegroundProcessId) -ne $StudioProcessId -and [DateTime]::UtcNow -lt $deadline) {
    Start-Sleep -Milliseconds 50
}

$foregroundProcessId = Get-ForegroundProcessId
if ($foregroundProcessId -ne $StudioProcessId) {
    $foreground = Get-Process -Id $foregroundProcessId -ErrorAction SilentlyContinue
    $foregroundName = if ($null -eq $foreground) { "unknown" } else { $foreground.ProcessName }
    throw "Expected Roblox Studio process $StudioProcessId to remain foreground; found $foregroundProcessId ($foregroundName)"
}

Write-Output "Roblox Studio process $StudioProcessId owns the foreground"

if ($Action -eq "watch") {
    $watchResult = [CarbonForegroundProbe]::WatchForbiddenForeground(
        $StudioProcessId,
        $ForbiddenProcessId,
        $ReadyPath,
        $DonePath,
        60000
    )
    switch ($watchResult) {
        0 { Write-Output "Forbidden Studio process $ForbiddenProcessId never owned the foreground" }
        1 { throw "Forbidden Studio process $ForbiddenProcessId became foreground" }
        2 { throw "Timed out waiting for the foreground watch completion signal" }
        3 { throw "Expected Studio process $StudioProcessId lost foreground before the watcher became ready" }
        4 { throw "Could not install the foreground event watcher" }
        default { throw "Unexpected foreground watcher result $watchResult" }
    }
}
