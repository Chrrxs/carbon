using InteropApi = RML.Interop.Interop;

namespace RML.Core.Api;

/// <summary>
/// Narrow Studio operations used by installed-runtime qualification.
/// </summary>
public static class StudioDiagnostics
{
    /// <summary>
    /// Queues Studio's File &gt; Save action for the already-open local document.
    /// The operation deliberately accepts no destination and cannot publish to Roblox.
    /// </summary>
    public static bool QueueLocalPlaceSaveForTesting() => InteropApi.QueueStudioLocalPlaceSave();
}
