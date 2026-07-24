using RML.Interop;

namespace Roblox;

/// <summary>
/// Provides privileged ordering operations that preserve the existing Roblox
/// parent-child relationships.
/// </summary>
public static class InstanceOrdering
{
    /// <summary>
    /// Reorders the complete direct-child vector of <paramref name="parent"/>
    /// without assigning any child's Parent property.
    /// </summary>
    /// <exception cref="InvalidOperationException">
    /// The supplied children are not the exact current, duplicate-free child set,
    /// or the native runtime rejected the operation.
    /// </exception>
    public static void ReorderChildren(Instance parent, IReadOnlyList<Instance> children)
    {
        ArgumentNullException.ThrowIfNull(parent);
        ArgumentNullException.ThrowIfNull(children);

        var childHandles = new nuint[children.Count];
        for (var index = 0; index < children.Count; ++index)
        {
            var child = children[index];
            ArgumentNullException.ThrowIfNull(child);
            childHandles[index] = ((IInteropInstance)child).InteropHandle;
        }

        var parentHandle = ((IInteropInstance)parent).InteropHandle;
        if (!Interop.ReorderInstanceChildren(parentHandle, childHandles))
        {
            throw new InvalidOperationException(
                "Native runtime rejected the requested complete child ordering.");
        }
    }
}
