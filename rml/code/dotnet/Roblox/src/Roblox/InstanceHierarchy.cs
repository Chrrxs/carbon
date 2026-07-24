namespace Roblox;

/// <summary>
/// Provides the compact native hierarchy snapshot used by managed verification.
/// It does not serialize instances or read any authored property payloads.
/// </summary>
public static class InstanceHierarchy
{
    public static nuint RuntimeHandle(Instance instance)
    {
        ArgumentNullException.ThrowIfNull(instance);
        return instance.Handle;
    }

    public static byte[] Read(
        Instance root,
        Instance? excludedRoot = null,
        bool includeCaptureMetadata = false)
    {
        ArgumentNullException.ThrowIfNull(root);
        return RML.Interop.Interop.ReadInstanceHierarchy(
            root.Handle,
            excludedRoot?.Handle ?? 0,
            includeCaptureMetadata);
    }
}
