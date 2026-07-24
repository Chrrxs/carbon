namespace RML.Interop;

/// <summary>
/// Exposes the native handle needed to marshal an engine object through the interop ABI.
/// </summary>
public interface IInteropInstance
{
    nuint InteropHandle { get; }
}
