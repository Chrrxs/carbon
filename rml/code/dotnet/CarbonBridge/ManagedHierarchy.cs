using System.Buffers.Binary;
using System.Diagnostics.CodeAnalysis;
using System.Globalization;
using System.Text;

namespace Carbon.RmlBridge;

internal sealed record ManagedSourceNode(
    string SourceId,
    string ParentSourceId,
    string ClassName,
    string Name,
    int ParentIndex = -1,
    int ShapeId = -1,
    int ChildShapeMode = -1);

internal sealed record ManagedRuntimeNode(
    string DebugId,
    string ParentDebugId,
    string ClassName,
    string Name,
    nuint Handle = 0,
    int ParentIndex = -1,
    byte PersistenceFlags = 0);

internal sealed record ManagedRuntimeReference(
    int OwnerIndex,
    string Property,
    nuint TargetHandle);

internal sealed record ManagedRuntimeContentObject(
	int OwnerIndex,
	string Property);

internal sealed record ManagedRuntimeHierarchyPayload(
    IReadOnlyList<ManagedRuntimeNode> Nodes,
    IReadOnlyList<ManagedRuntimeReference> References,
	IReadOnlyList<ManagedRuntimeContentObject> ContentObjects);

internal readonly record struct CaptureRuntimeNode(
    nuint Handle,
    int ParentIndex,
    string ClassName,
    string Name,
    byte PersistenceFlags);

internal readonly record struct CaptureRuntimeReference(
    int OwnerIndex,
    string Property,
    nuint TargetHandle);

internal readonly record struct CaptureRuntimeContentObject(
	int OwnerIndex,
	string Property);

internal sealed record CaptureRuntimeHierarchyPayload(
    CaptureRuntimeNode[] Nodes,
    CaptureRuntimeReference[] References,
	CaptureRuntimeContentObject[] ContentObjects);

internal sealed record ManagedHierarchyBinding(
    string SourceId,
    string DebugId,
    string RootSourceId,
    string RootDebugId);

internal readonly record struct ManagedHierarchyMatch(
    string SourceId,
    string DebugId,
    string RootSourceId,
    string RootDebugId);

internal sealed record ManagedHierarchyChange(
    string Kind,
    string DebugId,
    string? RootDebugId,
    string ClassName = "unknown",
    string Name = "unknown",
    string RootClassName = "unknown",
    string RootName = "unknown",
    string PropertyName = "unknown");

internal static class ManagedHierarchy
{
    internal static HashSet<string> ExpandOwnedSourceIds(
        IReadOnlyList<ManagedSourceNode> source,
        IEnumerable<string> ownershipRoots)
    {
        var roots = ownershipRoots.ToHashSet(StringComparer.Ordinal);
        var owned = new HashSet<string>(StringComparer.Ordinal);
        for (var index = 0; index < source.Count; index++)
        {
            var node = source[index];
            if (roots.Contains(node.SourceId)
                || (node.ParentIndex >= 0
                    && owned.Contains(source[node.ParentIndex].SourceId)))
            {
                owned.Add(node.SourceId);
            }
        }
        return owned;
    }

    internal static int UniqueClassNameIndex(
        IReadOnlyList<ManagedSourceNode> source,
        string className,
        string name)
    {
        var match = -1;
        for (var index = 1; index < source.Count; index++)
        {
            if (!string.Equals(source[index].ClassName, className, StringComparison.Ordinal)
                || !string.Equals(source[index].Name, name, StringComparison.Ordinal))
            {
                continue;
            }
            if (match >= 0)
            {
                return -1;
            }
            match = index;
        }
        return match;
    }

    internal sealed record RuntimeShapeIndex(
        int[] ShapeByIndex,
        List<CanonicalShape> Shapes);

    private static readonly byte[] MagicV4 = "CARBONID4"u8.ToArray();
	private static readonly byte[] RuntimeMagicV5 = "RMLHIER5"u8.ToArray();
    internal const byte RuntimeSerializable = 1 << 0;
    internal const byte RuntimeArchivable = 1 << 1;
    internal const byte RuntimePersistent = RuntimeSerializable | RuntimeArchivable;

    internal static bool IsInternalDataModelRoot([NotNullWhen(false)] string? publicClassName) =>
        string.IsNullOrEmpty(publicClassName);

    internal static IEnumerable<ManagedRuntimeNode> RuntimeOnlyRoots(
        IReadOnlyList<ManagedRuntimeNode> runtime,
        string runtimeRootDebugId) => runtime
            .Where(node => node.ParentDebugId == runtimeRootDebugId
                && IsKnownRuntimeOnlyRoot(node.ClassName, node.Name));

    internal static bool IsKnownRuntimeOnlyRoot(string className, string name) =>
        (string.Equals(className, "CoreGui", StringComparison.Ordinal)
            && string.Equals(name, "CoreGui", StringComparison.Ordinal))
        || (string.Equals(className, "RobloxPluginGuiService", StringComparison.Ordinal)
            && string.Equals(name, "RobloxPluginGuiService", StringComparison.Ordinal))
        || (string.Equals(className, "VisualizationModeService", StringComparison.Ordinal)
            && string.Equals(name, "VisualizationModeService", StringComparison.Ordinal))
        || (string.Equals(className, "StudioSdkService", StringComparison.Ordinal)
            && string.Equals(name, "StudioSdkService", StringComparison.Ordinal))
        || (string.Equals(className, "Stats", StringComparison.Ordinal)
            && string.Equals(name, "Stats", StringComparison.Ordinal));

    internal static string RuntimeIdentity(nuint handle) => $"native:{handle:x}";

    internal static IReadOnlyList<ManagedHierarchyMatch> SourceRootMatches(
        IReadOnlyList<ManagedSourceNode> source,
        IReadOnlyList<ManagedHierarchyMatch> matches)
    {
        if (source.Count != matches.Count || source.Count == 0)
        {
            throw new InvalidDataException("managed source root bindings are incomplete");
        }
        var sourceRootId = source[0].SourceId;
        var roots = new List<ManagedHierarchyMatch>();
        for (var index = 0; index < source.Count; index++)
        {
            if (!string.Equals(source[index].SourceId, matches[index].SourceId, StringComparison.Ordinal))
            {
                throw new InvalidDataException("managed source root bindings are out of order");
            }
            if (index > 0
                && string.Equals(source[index].ParentSourceId, sourceRootId, StringComparison.Ordinal))
            {
                roots.Add(matches[index]);
            }
        }
        return roots;
    }

    internal static bool TryParseRuntimeIdentity(string value, out nuint handle)
    {
        handle = 0;
        return value.StartsWith("native:", StringComparison.Ordinal)
            && nuint.TryParse(
                value.AsSpan("native:".Length),
                NumberStyles.AllowHexSpecifier,
                CultureInfo.InvariantCulture,
                out handle)
            && handle != 0
            && string.Equals(value, RuntimeIdentity(handle), StringComparison.Ordinal);
    }

    internal static void ValidatePreVerificationChanges(
        IReadOnlyList<ManagedHierarchyChange> changes,
        IReadOnlySet<string> runtimeOnlyRootDebugIds)
    {
        const int diagnosticLimit = 16;
        var runtimeOnlyAddedDebugIds = new Dictionary<string, string>(StringComparer.Ordinal);
        var rejected = new List<string>(diagnosticLimit);
        var rejectedCount = 0;
        foreach (var change in changes)
        {
            if (string.Equals(change.Kind, "Add", StringComparison.Ordinal)
                && change.DebugId.Length != 0
                && change.RootDebugId is { } rootDebugId
                && !string.Equals(change.DebugId, rootDebugId, StringComparison.Ordinal)
                && runtimeOnlyRootDebugIds.Contains(rootDebugId)
                && runtimeOnlyAddedDebugIds.TryAdd(change.DebugId, rootDebugId))
            {
                continue;
            }
            if (string.Equals(change.Kind, "Property", StringComparison.Ordinal)
                && change.RootDebugId is { } propertyRootDebugId
                && runtimeOnlyAddedDebugIds.TryGetValue(change.DebugId, out var addedRootDebugId)
                && string.Equals(propertyRootDebugId, addedRootDebugId, StringComparison.Ordinal))
            {
                continue;
            }
            var changeDescription = string.Equals(change.Kind, "Property", StringComparison.Ordinal)
                ? $"{change.Kind} {change.ClassName} {change.Name}.{change.PropertyName}"
                : $"{change.Kind} {change.ClassName} {change.Name}";
            rejectedCount++;
            if (rejected.Count < diagnosticLimit)
            {
                rejected.Add(
                    $"{changeDescription} under " +
                    $"{change.RootClassName} {change.RootName} {change.RootDebugId ?? "unknown root"}");
            }
        }
        if (rejectedCount != 0)
        {
            var diagnostic = string.Join("; ", rejected);
            if (rejectedCount > rejected.Count)
            {
                diagnostic += $"; ... {rejectedCount - rejected.Count} more";
            }
            throw new InvalidOperationException(
                $"the edit hierarchy changed before managed verification ({diagnostic})");
        }
    }

    internal static IReadOnlyList<ManagedSourceNode> Parse(byte[] payload)
    {
        var offset = 0;
        ReadOnlySpan<byte> Read(int length)
        {
            if (length < 0 || offset > payload.Length - length)
            {
                throw new InvalidDataException("managed hierarchy payload is truncated");
            }
            var value = payload.AsSpan(offset, length);
            offset += length;
            return value;
        }

        var magic = Read(MagicV4.Length);
        if (!magic.SequenceEqual(MagicV4))
        {
            throw new InvalidDataException("managed hierarchy payload has the wrong protocol magic");
        }
        var count = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
        if (count == 0 || count > 20_000_000)
        {
            throw new InvalidDataException("managed hierarchy instance count is invalid");
        }
        var nodes = new List<ManagedSourceNode>(checked((int)count));
        var sourceIds = new HashSet<string>(checked((int)count), StringComparer.Ordinal);
        for (var index = 0; index < count; index++)
        {
            var sourceId = Convert.ToHexStringLower(Read(16));
            var encodedParentIndex = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
            var parentSourceId = encodedParentIndex == uint.MaxValue
                ? string.Empty
                : encodedParentIndex < index
                    ? nodes[checked((int)encodedParentIndex)].SourceId
                    : throw new InvalidDataException(
                        "managed source parent index does not precede its child");
            var shapeId = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
            if (shapeId > int.MaxValue || shapeId >= count)
            {
                throw new InvalidDataException("managed hierarchy shape identity is invalid");
            }
            var childShapeMode = Read(1)[0];
            if (childShapeMode > 1)
            {
                throw new InvalidDataException("managed hierarchy child shape mode is invalid");
            }
            var classLength = BinaryPrimitives.ReadUInt16LittleEndian(Read(sizeof(ushort)));
            var nameLength = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
            if (nameLength > int.MaxValue)
            {
                throw new InvalidDataException("managed hierarchy name exceeds the protocol limit");
            }
            var className = Encoding.UTF8.GetString(Read(classLength));
            var name = Encoding.UTF8.GetString(Read((int)nameLength));
            var parentIndex = -1;
            if (index == 0)
            {
                if (parentSourceId.Length != 0)
                {
                    throw new InvalidDataException("managed source root unexpectedly has a parent");
                }
            }
            else
            {
                parentIndex = checked((int)encodedParentIndex);
            }
            if (!sourceIds.Add(sourceId))
            {
                throw new InvalidDataException($"managed source duplicated identity {sourceId}");
            }
            nodes.Add(new(
                sourceId,
                parentSourceId,
                className,
                name,
                parentIndex,
                checked((int)shapeId),
                childShapeMode));
        }
        if (offset != payload.Length)
        {
            throw new InvalidDataException("managed hierarchy payload has trailing bytes");
        }
        return nodes;
    }

    internal static IReadOnlyList<ManagedRuntimeNode> ParseRuntime(byte[] payload) =>
        ParseRuntimePayload(payload).Nodes;

    internal static ManagedRuntimeHierarchyPayload ParseRuntimePayload(byte[] payload)
    {
        var offset = 0;
        ReadOnlySpan<byte> Read(int length)
        {
            if (length < 0 || offset > payload.Length - length)
            {
                throw new InvalidDataException("managed runtime hierarchy payload is truncated");
            }
            var value = payload.AsSpan(offset, length);
            offset += length;
            return value;
        }

        var magic = Read(RuntimeMagicV5.Length);
        if (!magic.SequenceEqual(RuntimeMagicV5))
        {
            throw new InvalidDataException("managed runtime hierarchy payload has the wrong protocol magic");
        }
        var count = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
        if (count == 0 || count > 20_000_000)
        {
            throw new InvalidDataException("managed runtime hierarchy instance count is invalid");
        }
        var nodes = new List<ManagedRuntimeNode>(checked((int)count));
        var handles = new HashSet<nuint>();
        for (var index = 0; index < count; index++)
        {
            var handle = (nuint)BinaryPrimitives.ReadUInt64LittleEndian(Read(sizeof(ulong)));
            var parentIndex = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
            var persistenceFlags = Read(1)[0];
            if ((persistenceFlags & ~RuntimePersistent) != 0)
            {
                throw new InvalidDataException("managed runtime hierarchy persistence flags are invalid");
            }
            var classLength = BinaryPrimitives.ReadUInt16LittleEndian(Read(sizeof(ushort)));
            var nameLength = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
            if (handle == 0 || !handles.Add(handle))
            {
                throw new InvalidDataException("managed runtime hierarchy contains an invalid instance handle");
            }
            if ((index == 0 && parentIndex != uint.MaxValue)
                || (index > 0 && parentIndex >= index))
            {
                throw new InvalidDataException("managed runtime hierarchy parent order is invalid");
            }
            if (nameLength > int.MaxValue)
            {
                throw new InvalidDataException("managed runtime hierarchy name exceeds the protocol limit");
            }
            var className = Encoding.UTF8.GetString(Read(classLength));
            var name = Encoding.UTF8.GetString(Read((int)nameLength));
            if (className.Length == 0)
            {
                throw new InvalidDataException("managed runtime hierarchy identity is empty");
            }
            var runtimeIdentity = RuntimeIdentity(handle);
            nodes.Add(new(
                runtimeIdentity,
                index == 0 ? string.Empty : nodes[(int)parentIndex].DebugId,
                className,
                name,
                handle,
                index == 0 ? -1 : checked((int)parentIndex),
                persistenceFlags));
        }
        var references = new List<ManagedRuntimeReference>();
        var referenceCount = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
        if (referenceCount > 100_000_000)
        {
            throw new InvalidDataException("managed runtime reference count is invalid");
        }
        references.Capacity = checked((int)referenceCount);
        var ownersAndProperties = new HashSet<(uint Owner, string Property)>();
        for (var index = 0; index < referenceCount; index++)
        {
            var ownerIndex = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
            var targetHandle = (nuint)BinaryPrimitives.ReadUInt64LittleEndian(Read(sizeof(ulong)));
            var propertyLength = BinaryPrimitives.ReadUInt16LittleEndian(Read(sizeof(ushort)));
            var property = Encoding.UTF8.GetString(Read(propertyLength));
            if (ownerIndex >= count || property.Length == 0
                || !ownersAndProperties.Add((ownerIndex, property)))
            {
                throw new InvalidDataException("managed runtime reference identity is invalid");
            }
            references.Add(new(checked((int)ownerIndex), property, targetHandle));
        }
		var contentObjects = new List<ManagedRuntimeContentObject>();
		var blockerCount = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
		if (blockerCount > 100_000_000)
		{
			throw new InvalidDataException("managed runtime Content.Object blocker count is invalid");
		}
		contentObjects.Capacity = checked((int)blockerCount);
		var contentOwnersAndProperties = new HashSet<(uint Owner, string Property)>();
		for (var index = 0; index < blockerCount; index++)
		{
			var ownerIndex = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
			var propertyLength = BinaryPrimitives.ReadUInt16LittleEndian(Read(sizeof(ushort)));
			var property = Encoding.UTF8.GetString(Read(propertyLength));
			if (ownerIndex >= count || property.Length == 0
				|| !contentOwnersAndProperties.Add((ownerIndex, property)))
			{
				throw new InvalidDataException("managed runtime Content.Object blocker identity is invalid");
			}
			contentObjects.Add(new(checked((int)ownerIndex), property));
		}
        if (offset != payload.Length)
        {
            throw new InvalidDataException("managed runtime hierarchy payload has trailing bytes");
        }
		return new(nodes, references, contentObjects);
    }

    internal static CaptureRuntimeHierarchyPayload ParseCaptureRuntimePayload(
        byte[] payload,
        CancellationToken cancellationToken = default)
    {
        var offset = 0;
        ReadOnlySpan<byte> Read(int length)
        {
            if (length < 0 || offset > payload.Length - length)
            {
                throw new InvalidDataException("capture runtime hierarchy payload is truncated");
            }
            var value = payload.AsSpan(offset, length);
            offset += length;
            return value;
        }

		var magic = Read(RuntimeMagicV5.Length);
		if (!magic.SequenceEqual(RuntimeMagicV5))
        {
            throw new InvalidDataException(
				"capture runtime hierarchy payload requires RMLHIER5 Content.Object metadata");
        }
        var count = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
        if (count == 0 || count > 20_000_000)
        {
            throw new InvalidDataException("capture runtime hierarchy instance count is invalid");
        }

        var nodeCount = checked((int)count);
        var nodes = new CaptureRuntimeNode[nodeCount];
        var handles = new ulong[nodeCount];
        var classNames = new Utf8StringPool();
        for (var index = 0; index < nodeCount; index++)
        {
            if ((index & 0xfff) == 0)
            {
                cancellationToken.ThrowIfCancellationRequested();
            }
            var encodedHandle = BinaryPrimitives.ReadUInt64LittleEndian(Read(sizeof(ulong)));
            var parentIndex = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
            var persistenceFlags = Read(1)[0];
            if ((persistenceFlags & ~RuntimePersistent) != 0)
            {
                throw new InvalidDataException(
                    "capture runtime hierarchy persistence flags are invalid");
            }
            var classLength = BinaryPrimitives.ReadUInt16LittleEndian(Read(sizeof(ushort)));
            var nameLength = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
            if (encodedHandle == 0)
            {
                throw new InvalidDataException(
                    "capture runtime hierarchy contains an invalid instance handle");
            }
            if ((index == 0 && parentIndex != uint.MaxValue)
                || (index > 0 && parentIndex >= index))
            {
                throw new InvalidDataException("capture runtime hierarchy parent order is invalid");
            }
            if (classLength == 0)
            {
                throw new InvalidDataException("capture runtime hierarchy identity is empty");
            }
            if (nameLength > int.MaxValue)
            {
                throw new InvalidDataException(
                    "capture runtime hierarchy name exceeds the protocol limit");
            }

            var className = classNames.Intern(Read(classLength));
            var name = Encoding.UTF8.GetString(Read(checked((int)nameLength)));
            handles[index] = encodedHandle;
            nodes[index] = new(
                checked((nuint)encodedHandle),
                index == 0 ? -1 : checked((int)parentIndex),
                className,
                name,
                persistenceFlags);
        }
        Array.Sort(handles);
        for (var index = 1; index < handles.Length; index++)
        {
            if (handles[index] == handles[index - 1])
            {
                throw new InvalidDataException(
                    "capture runtime hierarchy contains an invalid instance handle");
            }
        }

        var referenceCount = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
        if (referenceCount > 100_000_000)
        {
            throw new InvalidDataException("capture runtime reference count is invalid");
        }
        var references = new CaptureRuntimeReference[checked((int)referenceCount)];
        var propertyNames = new Utf8StringPool();
        for (var index = 0; index < references.Length; index++)
        {
            if ((index & 0xfff) == 0)
            {
                cancellationToken.ThrowIfCancellationRequested();
            }
            var ownerIndex = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
            var targetHandle = BinaryPrimitives.ReadUInt64LittleEndian(Read(sizeof(ulong)));
            var propertyLength = BinaryPrimitives.ReadUInt16LittleEndian(Read(sizeof(ushort)));
            if (ownerIndex >= count || propertyLength == 0)
            {
                throw new InvalidDataException("capture runtime reference identity is invalid");
            }
            references[index] = new(
                checked((int)ownerIndex),
                propertyNames.Intern(Read(propertyLength)),
                checked((nuint)targetHandle));
        }
        Array.Sort(references, CaptureRuntimeReferenceComparer.Instance);
        for (var index = 1; index < references.Length; index++)
        {
            if (references[index].OwnerIndex == references[index - 1].OwnerIndex
                && string.Equals(
                    references[index].Property,
                    references[index - 1].Property,
                    StringComparison.Ordinal))
            {
                throw new InvalidDataException("capture runtime reference identity is invalid");
            }
        }
		var blockerCount = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
		if (blockerCount > 100_000_000)
		{
			throw new InvalidDataException("capture runtime Content.Object blocker count is invalid");
		}
		var contentObjects = new CaptureRuntimeContentObject[checked((int)blockerCount)];
		for (var index = 0; index < contentObjects.Length; index++)
		{
			if ((index & 0xfff) == 0)
			{
				cancellationToken.ThrowIfCancellationRequested();
			}
			var ownerIndex = BinaryPrimitives.ReadUInt32LittleEndian(Read(sizeof(uint)));
			var propertyLength = BinaryPrimitives.ReadUInt16LittleEndian(Read(sizeof(ushort)));
			if (ownerIndex >= count || propertyLength == 0)
			{
				throw new InvalidDataException("capture runtime Content.Object blocker identity is invalid");
			}
			contentObjects[index] = new(
				checked((int)ownerIndex),
				propertyNames.Intern(Read(propertyLength)));
		}
		Array.Sort(contentObjects, CaptureRuntimeContentObjectComparer.Instance);
		for (var index = 1; index < contentObjects.Length; index++)
		{
			if (contentObjects[index].OwnerIndex == contentObjects[index - 1].OwnerIndex
				&& string.Equals(
					contentObjects[index].Property,
					contentObjects[index - 1].Property,
					StringComparison.Ordinal))
			{
				throw new InvalidDataException("capture runtime Content.Object blocker identity is invalid");
			}
		}
        if (offset != payload.Length)
        {
            throw new InvalidDataException("capture runtime hierarchy payload has trailing bytes");
        }
        cancellationToken.ThrowIfCancellationRequested();
		return new(nodes, references, contentObjects);
    }

    private sealed class CaptureRuntimeReferenceComparer : IComparer<CaptureRuntimeReference>
    {
        internal static CaptureRuntimeReferenceComparer Instance { get; } = new();

        public int Compare(CaptureRuntimeReference left, CaptureRuntimeReference right)
        {
            var ownerComparison = left.OwnerIndex.CompareTo(right.OwnerIndex);
            return ownerComparison != 0
                ? ownerComparison
                : string.CompareOrdinal(left.Property, right.Property);
        }
    }

	private sealed class CaptureRuntimeContentObjectComparer : IComparer<CaptureRuntimeContentObject>
	{
		internal static CaptureRuntimeContentObjectComparer Instance { get; } = new();

		public int Compare(CaptureRuntimeContentObject left, CaptureRuntimeContentObject right)
		{
			var ownerComparison = left.OwnerIndex.CompareTo(right.OwnerIndex);
			return ownerComparison != 0
				? ownerComparison
				: string.CompareOrdinal(left.Property, right.Property);
		}
	}

    private sealed class Utf8StringPool
    {
        private readonly Dictionary<uint, List<Entry>> _entries = [];

        internal string Intern(ReadOnlySpan<byte> utf8)
        {
            var hash = 2166136261u;
            foreach (var item in utf8)
            {
                hash = unchecked((hash ^ item) * 16777619u);
            }
            if (_entries.TryGetValue(hash, out var bucket))
            {
                foreach (var entry in bucket)
                {
                    if (utf8.SequenceEqual(entry.Utf8))
                    {
                        return entry.Value;
                    }
                }
            }
            else
            {
                bucket = [];
                _entries.Add(hash, bucket);
            }
            var bytes = utf8.ToArray();
            var value = Encoding.UTF8.GetString(bytes);
            bucket.Add(new(bytes, value));
            return value;
        }

        private sealed record Entry(byte[] Utf8, string Value);
    }

    internal static IReadOnlyList<ManagedRuntimeNode> NormalizeRuntime(
        IReadOnlyList<ManagedRuntimeNode> runtime,
        Func<ManagedRuntimeNode, nuint>? readHeadWeldPart1 = null)
    {
        if (runtime.Count == 0)
        {
            return runtime;
        }

        var indexByHandle = new Dictionary<nuint, int>(runtime.Count);
        var hasAccessoryWeldChild = new bool[runtime.Count];
        var hasAccessoryRigidConstraintChild = new bool[runtime.Count];
        for (var index = 0; index < runtime.Count; index++)
        {
            var node = runtime[index];
            if (node.Handle != 0)
            {
                indexByHandle[node.Handle] = index;
            }
            if (node.ParentIndex < 0)
            {
                continue;
            }
            if (string.Equals(node.ClassName, "Weld", StringComparison.Ordinal)
                && string.Equals(node.Name, "AccessoryWeld", StringComparison.Ordinal))
            {
                hasAccessoryWeldChild[node.ParentIndex] = true;
            }
            else if (string.Equals(node.ClassName, "RigidConstraint", StringComparison.Ordinal)
                && string.Equals(node.Name, "AccessoryRigidConstraint", StringComparison.Ordinal))
            {
                hasAccessoryRigidConstraintChild[node.ParentIndex] = true;
            }
        }

        bool IsPartHandleUnderAccessory(int index)
        {
            var handle = runtime[index];
            if (!string.Equals(handle.ClassName, "Part", StringComparison.Ordinal)
                || !string.Equals(handle.Name, "Handle", StringComparison.Ordinal)
                || handle.ParentIndex < 0)
            {
                return false;
            }
            return string.Equals(
                runtime[handle.ParentIndex].ClassName,
                "Accessory",
                StringComparison.Ordinal);
        }

        bool IsExcludedRoot(int index)
        {
            var node = runtime[index];
            if (node.ParentIndex < 0)
            {
                return false;
            }
            var parent = runtime[node.ParentIndex];
            if (string.Equals(node.ClassName, "Status", StringComparison.Ordinal)
                && string.Equals(parent.ClassName, "Humanoid", StringComparison.Ordinal))
            {
                return true;
            }
            if (string.Equals(node.ClassName, "ConfigureServerService", StringComparison.Ordinal)
                && string.Equals(parent.ClassName, "DataModel", StringComparison.Ordinal))
            {
                return true;
            }
            if (string.Equals(node.ClassName, "Weld", StringComparison.Ordinal)
                && string.Equals(node.Name, "AccessoryWeld", StringComparison.Ordinal)
                && IsPartHandleUnderAccessory(node.ParentIndex)
                && hasAccessoryRigidConstraintChild[node.ParentIndex])
            {
                return true;
            }
            if (!string.Equals(node.ClassName, "Weld", StringComparison.Ordinal)
                || !string.Equals(node.Name, "HeadWeld", StringComparison.Ordinal)
                || !string.Equals(parent.ClassName, "Part", StringComparison.Ordinal)
                || !string.Equals(parent.Name, "Head", StringComparison.Ordinal)
                || readHeadWeldPart1 is null)
            {
                return false;
            }
            var part1Handle = readHeadWeldPart1(node);
            return part1Handle != 0
                && indexByHandle.TryGetValue(part1Handle, out var part1Index)
                && IsPartHandleUnderAccessory(part1Index)
                && hasAccessoryWeldChild[part1Index];
        }

        var excluded = new bool[runtime.Count];
        var oldToNew = new int[runtime.Count];
        Array.Fill(oldToNew, -1);
        var normalized = new List<ManagedRuntimeNode>(runtime.Count);
        for (var index = 0; index < runtime.Count; index++)
        {
            var node = runtime[index];
            excluded[index] = (node.ParentIndex >= 0 && excluded[node.ParentIndex])
                || IsExcludedRoot(index);
            if (excluded[index])
            {
                continue;
            }
            var parentIndex = node.ParentIndex < 0 ? -1 : oldToNew[node.ParentIndex];
            if (node.ParentIndex >= 0 && parentIndex < 0)
            {
                throw new InvalidDataException("managed runtime normalization orphaned a retained node");
            }
            oldToNew[index] = normalized.Count;
            normalized.Add(node with
            {
                ParentDebugId = parentIndex < 0 ? string.Empty : normalized[parentIndex].DebugId,
                ParentIndex = parentIndex,
            });
        }
        return normalized;
    }

    internal static RuntimeShapeIndex PrecomputeRuntimeShapes(
        IReadOnlyList<ManagedRuntimeNode> runtime)
    {
        var firstChild = new int[runtime.Count];
        var lastChild = new int[runtime.Count];
        var nextSibling = new int[runtime.Count];
        var shapeByIndex = new int[runtime.Count];
        Array.Fill(firstChild, -1);
        Array.Fill(lastChild, -1);
        Array.Fill(nextSibling, -1);
        Array.Fill(shapeByIndex, -1);
        for (var index = 1; index < runtime.Count; index++)
        {
            var parent = runtime[index].ParentIndex;
            if (parent < 0 || parent >= index)
            {
                throw new InvalidDataException("managed runtime shape index has an invalid parent");
            }
            if (firstChild[parent] < 0)
            {
                firstChild[parent] = index;
            }
            else
            {
                nextSibling[lastChild[parent]] = index;
            }
            lastChild[parent] = index;
        }

        static ulong Token(int shape)
        {
            var value = (ulong)(uint)shape + 0x9e3779b97f4a7c15UL;
            value ^= value >> 30;
            value *= 0xbf58476d1ce4e5b9UL;
            value ^= value >> 27;
            value *= 0x94d049bb133111ebUL;
            return value ^ (value >> 31);
        }

        static (int Class, int Name, int Children, ulong Sum, ulong SumSquares) ShapeHash(
            CanonicalShape shape)
        {
            ulong sum = 0;
            ulong sumSquares = 0;
            var children = 0;
            unchecked
            {
                foreach (var child in shape.Children)
                {
                    var token = Token(child.Shape);
                    sum += token * (uint)child.Count;
                    sumSquares += token * token * (uint)child.Count;
                    children += child.Count;
                }
            }
            return (
                StringComparer.Ordinal.GetHashCode(shape.ClassName),
                StringComparer.Ordinal.GetHashCode(shape.Name),
                children,
                sum,
                sumSquares);
        }

        var shapes = new List<CanonicalShape>();
        var shapeByKey = new Dictionary<CanonicalShape, int>();
        var leaves = new Dictionary<(string Class, string Name), int>();
        var candidatesByHash = new Dictionary<
            (int Class, int Name, int Children, ulong Sum, ulong SumSquares),
            List<int>>();

        bool NodeMatchesShape(int nodeIndex, CanonicalShape shape)
        {
            var node = runtime[nodeIndex];
            if (!string.Equals(node.ClassName, shape.ClassName, StringComparison.Ordinal)
                || !string.Equals(node.Name, shape.Name, StringComparison.Ordinal))
            {
                return false;
            }
            var childCount = 0;
            for (var child = firstChild[nodeIndex]; child >= 0; child = nextSibling[child])
            {
                childCount++;
            }
            if (childCount != shape.Children.Sum(child => child.Count))
            {
                return false;
            }
            foreach (var expected in shape.Children)
            {
                var actual = 0;
                for (var child = firstChild[nodeIndex]; child >= 0; child = nextSibling[child])
                {
                    if (shapeByIndex[child] == expected.Shape)
                    {
                        actual++;
                    }
                }
                if (actual != expected.Count)
                {
                    return false;
                }
            }
            return true;
        }

        for (var nodeIndex = runtime.Count - 1; nodeIndex >= 0; nodeIndex--)
        {
            var node = runtime[nodeIndex];
            var child = firstChild[nodeIndex];
            if (child < 0)
            {
                var identity = (node.ClassName, node.Name);
                if (!leaves.TryGetValue(identity, out var leafShape))
                {
                    leafShape = shapes.Count;
                    leaves.Add(identity, leafShape);
                    var shape = new CanonicalShape(node.ClassName, node.Name, []);
                    shapes.Add(shape);
                    var hash = ShapeHash(shape);
                    candidatesByHash[hash] = [leafShape];
                }
                shapeByIndex[nodeIndex] = leafShape;
                continue;
            }

            ulong sum = 0;
            ulong sumSquares = 0;
            var childCount = 0;
            unchecked
            {
                for (; child >= 0; child = nextSibling[child])
                {
                    var token = Token(shapeByIndex[child]);
                    sum += token;
                    sumSquares += token * token;
                    childCount++;
                }
            }
            var nodeHash = (
                StringComparer.Ordinal.GetHashCode(node.ClassName),
                StringComparer.Ordinal.GetHashCode(node.Name),
                childCount,
                sum,
                sumSquares);
            if (candidatesByHash.TryGetValue(nodeHash, out var candidates))
            {
                var matched = candidates.FirstOrDefault(
                    shape => NodeMatchesShape(nodeIndex, shapes[shape]),
                    -1);
                if (matched >= 0)
                {
                    shapeByIndex[nodeIndex] = matched;
                    continue;
                }
            }

            var childShapes = new Dictionary<int, int>();
            for (child = firstChild[nodeIndex]; child >= 0; child = nextSibling[child])
            {
                var childShape = shapeByIndex[child];
                childShapes[childShape] = childShapes.GetValueOrDefault(childShape) + 1;
            }
            var canonicalChildren = new CanonicalChild[childShapes.Count];
            var canonicalIndex = 0;
            foreach (var childShape in childShapes)
            {
                var canonicalChild = shapes[childShape.Key];
                canonicalChildren[canonicalIndex++] = new(
                    canonicalChild.ClassName,
                    canonicalChild.Name,
                    childShape.Key,
                    childShape.Value);
            }
            Array.Sort(canonicalChildren, static (left, right) => left.Shape.CompareTo(right.Shape));
            var key = new CanonicalShape(node.ClassName, node.Name, canonicalChildren);
            if (!shapeByKey.TryGetValue(key, out var newShape))
            {
                newShape = shapes.Count;
                shapeByKey.Add(key, newShape);
                shapes.Add(key);
            }
            shapeByIndex[nodeIndex] = newShape;
            if (!candidatesByHash.TryGetValue(nodeHash, out candidates))
            {
                candidates = [];
                candidatesByHash.Add(nodeHash, candidates);
            }
            if (!candidates.Contains(newShape))
            {
                candidates.Add(newShape);
            }
        }
        return new(shapeByIndex, shapes);
    }

    internal static IReadOnlyList<ManagedHierarchyMatch> Match(
        IReadOnlyList<ManagedSourceNode> source,
        IReadOnlyList<ManagedRuntimeNode> runtime,
        string runtimeRootDebugId,
        Action<string>? reportStrategy = null,
        RuntimeShapeIndex? runtimeShapeIndex = null)
    {
        if (source.Count == 0 || runtime.Count == 0)
        {
            throw new InvalidDataException("managed hierarchy cannot be empty");
        }
        var sourceRoot = source[0];
        if (sourceRoot.ParentSourceId.Length != 0)
        {
            throw new InvalidDataException("managed source root unexpectedly has a parent");
        }
        var matchedByParentOccurrence = TryMatchByParentOccurrence(
            source,
            runtime,
            runtimeRootDebugId,
            out var orderedBindings,
            out var indexedFailure,
            out var indexedTiming,
            runtimeShapeIndex);
        reportStrategy?.Invoke(matchedByParentOccurrence
            ? $"indexed parent occurrence ({indexedTiming})"
            : $"full structural fallback ({indexedFailure})");
        if (matchedByParentOccurrence)
        {
            return orderedBindings;
        }

        var sourceById = new Dictionary<string, ManagedSourceNode>(source.Count, StringComparer.Ordinal);
        var sourceChildren = new Dictionary<string, List<ManagedSourceNode>>(StringComparer.Ordinal);
        for (var index = 0; index < source.Count; index++)
        {
            var node = source[index];
            if (sourceById.ContainsKey(node.SourceId))
            {
                throw new InvalidDataException($"managed source duplicated identity {node.SourceId}");
            }
            if (index > 0 && !sourceById.ContainsKey(node.ParentSourceId))
            {
                throw new InvalidDataException($"managed source parent {node.ParentSourceId} precedes no mapped node");
            }
            sourceById.Add(node.SourceId, node);
            if (index == 0)
            {
                continue;
            }
            if (!sourceChildren.TryGetValue(node.ParentSourceId, out var children))
            {
                children = [];
                sourceChildren.Add(node.ParentSourceId, children);
            }
            children.Add(node);
        }

        var runtimeById = new Dictionary<string, ManagedRuntimeNode>(runtime.Count, StringComparer.Ordinal);
        var runtimeCandidates = new Dictionary<
            (string Parent, string Class, string Name),
            List<ManagedRuntimeNode>>();
        foreach (var node in runtime)
        {
            if (!runtimeById.TryAdd(node.DebugId, node))
            {
                throw new InvalidDataException($"managed runtime duplicated debug identity {node.DebugId}");
            }
            var key = (node.ParentDebugId, node.ClassName, node.Name);
            if (!runtimeCandidates.TryGetValue(key, out var candidates))
            {
                candidates = [];
                runtimeCandidates.Add(key, candidates);
            }
            candidates.Add(node);
        }
        if (!runtimeById.ContainsKey(runtimeRootDebugId))
        {
            throw new InvalidDataException("managed hierarchy runtime root is missing");
        }

        // Shape analysis is only needed for duplicate siblings with the same
        // class and name. Most managed hierarchies bind uniquely by their parent
        // path, so compute source subtree shapes lazily instead of paying the
        // global allocation and sorting cost for every instance.
        var shapeBySource = new Dictionary<string, int>(StringComparer.Ordinal);
        var shapes = new List<SourceShape>();
        var shapeByKey = new Dictionary<CanonicalShape, int>();
        var leafShapeByIdentity = new Dictionary<(string Class, string Name), int>();
        var emptyChildShapes = new Dictionary<
            (string Class, string Name),
            Dictionary<int, int>>();

        int InternLeafShape(string className, string name)
        {
            var identity = (className, name);
            if (leafShapeByIdentity.TryGetValue(identity, out var existing))
            {
                return existing;
            }
            var shapeId = shapes.Count;
            leafShapeByIdentity.Add(identity, shapeId);
            shapes.Add(new(className, name, emptyChildShapes));
            return shapeId;
        }

        int SourceShape(ManagedSourceNode requested)
        {
            if (shapeBySource.TryGetValue(requested.SourceId, out var known))
            {
                return known;
            }

            var pending = new Stack<(ManagedSourceNode Node, bool ChildrenReady)>();
            pending.Push((requested, false));
            while (pending.TryPop(out var entry))
            {
                var node = entry.Node;
                if (shapeBySource.ContainsKey(node.SourceId))
                {
                    continue;
                }
                if (!sourceChildren.TryGetValue(node.SourceId, out var children))
                {
                    shapeBySource.Add(node.SourceId, InternLeafShape(node.ClassName, node.Name));
                    continue;
                }
                if (!entry.ChildrenReady)
                {
                    pending.Push((node, true));
                    for (var index = children.Count - 1; index >= 0; index--)
                    {
                        var child = children[index];
                        if (!shapeBySource.ContainsKey(child.SourceId))
                        {
                            pending.Push((child, false));
                        }
                    }
                    continue;
                }

                var childShapes = new Dictionary<
                    (string Class, string Name),
                    Dictionary<int, int>>();
                foreach (var child in children)
                {
                    var identity = (child.ClassName, child.Name);
                    if (!childShapes.TryGetValue(identity, out var counts))
                    {
                        counts = [];
                        childShapes.Add(identity, counts);
                    }
                    var childShape = shapeBySource[child.SourceId];
                    counts[childShape] = counts.GetValueOrDefault(childShape) + 1;
                }
                var shapeKey = ShapeKey(node.ClassName, node.Name, childShapes);
                if (!shapeByKey.TryGetValue(shapeKey, out var shapeId))
                {
                    shapeId = shapes.Count;
                    shapeByKey.Add(shapeKey, shapeId);
                    shapes.Add(new(node.ClassName, node.Name, childShapes));
                }
                shapeBySource.Add(node.SourceId, shapeId);
            }
            return shapeBySource[requested.SourceId];
        }

        var compatibility = new Dictionary<(int Shape, string Runtime), bool>();

        bool TryAssignShapes(
            IReadOnlyDictionary<int, int> requiredShapes,
            IReadOnlyList<ManagedRuntimeNode> candidates,
            out Dictionary<int, List<ManagedRuntimeNode>> assignments)
        {
            assignments = requiredShapes.Keys.ToDictionary(
                shape => shape,
                _ => new List<ManagedRuntimeNode>());
            if (requiredShapes.Values.Sum() != candidates.Count)
            {
                return false;
            }
            if (requiredShapes.Count == 1)
            {
                var shape = requiredShapes.Keys.First();
                foreach (var candidate in candidates)
                {
                    if (!CanMatch(shape, candidate))
                    {
                        return false;
                    }
                    assignments[shape].Add(candidate);
                }
                return true;
            }

            var compatibleShapes = new List<int>[candidates.Count];
            for (var index = 0; index < candidates.Count; index++)
            {
                compatibleShapes[index] = [];
                foreach (var shape in requiredShapes.Keys)
                {
                    if (CanMatch(shape, candidates[index]))
                    {
                        compatibleShapes[index].Add(shape);
                    }
                }
                if (compatibleShapes[index].Count == 0)
                {
                    return false;
                }
            }

            var remaining = requiredShapes.ToDictionary(entry => entry.Key, entry => entry.Value);
            var assigned = new bool[candidates.Count];
            var unassigned = candidates.Count;
            while (unassigned > 0)
            {
                var progressed = false;
                for (var index = 0; index < candidates.Count; index++)
                {
                    if (assigned[index])
                    {
                        continue;
                    }
                    var available = compatibleShapes[index]
                        .Where(shape => remaining[shape] > 0)
                        .ToArray();
                    if (available.Length == 0)
                    {
                        return false;
                    }
                    if (available.Length != 1)
                    {
                        continue;
                    }
                    var shape = available[0];
                    assignments[shape].Add(candidates[index]);
                    remaining[shape]--;
                    assigned[index] = true;
                    unassigned--;
                    progressed = true;
                }

                foreach (var shape in requiredShapes.Keys)
                {
                    if (remaining[shape] == 0)
                    {
                        continue;
                    }
                    var eligible = Enumerable.Range(0, candidates.Count)
                        .Where(index => !assigned[index]
                            && compatibleShapes[index].Contains(shape))
                        .ToArray();
                    if (eligible.Length < remaining[shape])
                    {
                        return false;
                    }
                    if (eligible.Length != remaining[shape])
                    {
                        continue;
                    }
                    foreach (var index in eligible)
                    {
                        assignments[shape].Add(candidates[index]);
                        remaining[shape]--;
                        assigned[index] = true;
                        unassigned--;
                    }
                    progressed = true;
                }
                if (!progressed)
                {
                    // More than one complete assignment remains possible. With
                    // no persistent identity there is no safe binding choice.
                    return false;
                }
            }
            return remaining.Values.All(count => count == 0);
        }

        bool CanMatch(int shapeId, ManagedRuntimeNode candidate)
        {
            var memoKey = (shapeId, candidate.DebugId);
            if (compatibility.TryGetValue(memoKey, out var known))
            {
                return known;
            }
            compatibility[memoKey] = false;
            var shape = shapes[shapeId];
            if (!string.Equals(shape.ClassName, candidate.ClassName, StringComparison.Ordinal)
                || !string.Equals(shape.Name, candidate.Name, StringComparison.Ordinal))
            {
                return false;
            }
            foreach (var childGroup in shape.ChildShapes)
            {
                var runtimeKey = (candidate.DebugId, childGroup.Key.Class, childGroup.Key.Name);
                if (!runtimeCandidates.TryGetValue(runtimeKey, out var childCandidates)
                    || !TryAssignShapes(childGroup.Value, childCandidates, out _))
                {
                    return false;
                }
            }
            compatibility[memoKey] = true;
            return true;
        }

        var bindingBySource = new Dictionary<string, ManagedHierarchyMatch>(source.Count, StringComparer.Ordinal);
        var rootBinding = new ManagedHierarchyMatch(
            sourceRoot.SourceId,
            runtimeRootDebugId,
            sourceRoot.SourceId,
            runtimeRootDebugId);
        bindingBySource.Add(sourceRoot.SourceId, rootBinding);

        void BindChildren(ManagedSourceNode sourceParent, ManagedRuntimeNode runtimeParent, ManagedHierarchyMatch parent)
        {
            if (!sourceChildren.TryGetValue(sourceParent.SourceId, out var children))
            {
                return;
            }
            foreach (var group in children.GroupBy(child => (child.ClassName, child.Name)))
            {
                var first = group.First();
                var runtimeKey = (runtimeParent.DebugId, group.Key.ClassName, group.Key.Name);
                if (!runtimeCandidates.TryGetValue(runtimeKey, out var candidates))
                {
                    throw new InvalidDataException(
                        $"managed source identity {first.SourceId} ({first.ClassName} {first.Name}) has no runtime match");
                }
                var sourceGroup = group.ToList();
                if (sourceGroup.Count == 1)
                {
                    if (candidates.Count != 1)
                    {
                        throw new InvalidDataException(
                            $"managed source identity {first.SourceId} ({first.ClassName} {first.Name}) has an ambiguous runtime match");
                    }
                    BindNode(sourceGroup[0], candidates[0], parent);
                    continue;
                }
                var sourceByShape = sourceGroup
                    .GroupBy(SourceShape)
                    .ToDictionary(shapeGroup => shapeGroup.Key, shapeGroup => shapeGroup.ToList());
                if (sourceByShape.Count == 1)
                {
                    var sameShapeSource = sourceByShape.Values.Single();
                    if (sameShapeSource.Count != candidates.Count)
                    {
                        throw new InvalidDataException(
                            $"managed source identity {first.SourceId} ({first.ClassName} {first.Name}) has an ambiguous runtime match");
                    }
                    for (var index = 0; index < sameShapeSource.Count; index++)
                    {
                        BindNode(sameShapeSource[index], candidates[index], parent);
                    }
                    continue;
                }
                var requiredShapes = sourceByShape.ToDictionary(
                    entry => entry.Key,
                    entry => entry.Value.Count);
                if (!TryAssignShapes(requiredShapes, candidates, out var assignments))
                {
                    throw new InvalidDataException(
                        $"managed source identity {first.SourceId} ({first.ClassName} {first.Name}) has no unambiguous runtime match");
                }
                foreach (var sourceShape in sourceByShape)
                {
                    var matchedRuntime = assignments[sourceShape.Key];
                    for (var index = 0; index < sourceShape.Value.Count; index++)
                    {
                        BindNode(sourceShape.Value[index], matchedRuntime[index], parent);
                    }
                }
            }
        }

        void BindNode(
            ManagedSourceNode sourceNode,
            ManagedRuntimeNode runtimeNode,
            ManagedHierarchyMatch parent)
        {
            var rootSourceId = parent.SourceId == sourceRoot.SourceId
                ? sourceNode.SourceId
                : parent.RootSourceId;
            var rootDebugId = parent.DebugId == runtimeRootDebugId
                ? runtimeNode.DebugId
                : parent.RootDebugId;
            var binding = new ManagedHierarchyMatch(
                sourceNode.SourceId,
                runtimeNode.DebugId,
                rootSourceId,
                rootDebugId);
            bindingBySource.Add(sourceNode.SourceId, binding);
            BindChildren(sourceNode, runtimeNode, binding);
        }

        BindChildren(sourceRoot, runtimeById[runtimeRootDebugId], rootBinding);
        if (bindingBySource.Count != source.Count)
        {
            throw new InvalidDataException("managed hierarchy matching left source identities unbound");
        }
        return source.Select(node => bindingBySource[node.SourceId]).ToArray();
    }

    private static bool TryMatchByParentOccurrence(
        IReadOnlyList<ManagedSourceNode> source,
        IReadOnlyList<ManagedRuntimeNode> runtime,
        string runtimeRootDebugId,
        out IReadOnlyList<ManagedHierarchyMatch> bindings,
        out string failureReason,
        out string timing,
        RuntimeShapeIndex? precomputedRuntimeShapes)
    {
        bindings = [];
        failureReason = "unspecified mismatch";
        timing = string.Empty;
        var phaseTimer = System.Diagnostics.Stopwatch.StartNew();
        var sourceFirstChild = new int[source.Count];
        var sourceLastChild = new int[source.Count];
        var sourceNextSibling = new int[source.Count];
        Array.Fill(sourceFirstChild, -1);
        Array.Fill(sourceLastChild, -1);
        Array.Fill(sourceNextSibling, -1);

        void AddSourceChild(int parentIndex, int childIndex)
        {
            if (sourceFirstChild[parentIndex] < 0)
            {
                sourceFirstChild[parentIndex] = childIndex;
            }
            else
            {
                sourceNextSibling[sourceLastChild[parentIndex]] = childIndex;
            }
            sourceLastChild[parentIndex] = childIndex;
        }

        var sourceIsIndexed = source[0].ParentIndex == -1;
        for (var index = 1; sourceIsIndexed && index < source.Count; index++)
        {
            var node = source[index];
            sourceIsIndexed = node.ParentIndex >= 0
                && node.ParentIndex < index
                && string.Equals(
                    source[node.ParentIndex].SourceId,
                    node.ParentSourceId,
                    StringComparison.Ordinal);
        }
        if (sourceIsIndexed)
        {
            for (var index = 1; index < source.Count; index++)
            {
                AddSourceChild(source[index].ParentIndex, index);
            }
        }
        else
        {
            var sourceIndex = new Dictionary<string, int>(source.Count, StringComparer.Ordinal);
            for (var index = 0; index < source.Count; index++)
            {
                var node = source[index];
                if (!sourceIndex.TryAdd(node.SourceId, index))
                {
                    return false;
                }
                if (index == 0)
                {
                    if (node.ParentSourceId.Length != 0)
                    {
                        return false;
                    }
                    continue;
                }
                if (!sourceIndex.TryGetValue(node.ParentSourceId, out var parentIndex))
                {
                    return false;
                }
                AddSourceChild(parentIndex, index);
            }
        }
        var sourceIndexMilliseconds = phaseTimer.ElapsedMilliseconds;

        var runtimeFirstChild = new int[runtime.Count];
        var runtimeLastChild = new int[runtime.Count];
        var runtimeNextSibling = new int[runtime.Count];
        var runtimeNextMatch = new int[runtime.Count];
        Array.Fill(runtimeFirstChild, -1);
        Array.Fill(runtimeLastChild, -1);
        Array.Fill(runtimeNextSibling, -1);
        Array.Fill(runtimeNextMatch, -1);

        void AddRuntimeChild(int parentIndex, int childIndex)
        {
            if (runtimeFirstChild[parentIndex] < 0)
            {
                runtimeFirstChild[parentIndex] = childIndex;
            }
            else
            {
                runtimeNextSibling[runtimeLastChild[parentIndex]] = childIndex;
            }
            runtimeLastChild[parentIndex] = childIndex;
        }

        var runtimeIsIndexed = runtime[0].ParentIndex == -1;
        for (var index = 1; runtimeIsIndexed && index < runtime.Count; index++)
        {
            var node = runtime[index];
            runtimeIsIndexed = node.ParentIndex >= 0
                && node.ParentIndex < index
                && string.Equals(
                    runtime[node.ParentIndex].DebugId,
                    node.ParentDebugId,
                    StringComparison.Ordinal);
        }
        var runtimeRootIndex = -1;
        if (runtimeIsIndexed)
        {
            for (var index = 0; index < runtime.Count; index++)
            {
                var node = runtime[index];
                if (string.Equals(node.DebugId, runtimeRootDebugId, StringComparison.Ordinal))
                {
                    if (runtimeRootIndex >= 0)
                    {
                        return false;
                    }
                    runtimeRootIndex = index;
                }
                if (index > 0)
                {
                    AddRuntimeChild(node.ParentIndex, index);
                }
            }
        }
        else
        {
            var runtimeIndex = new Dictionary<string, int>(runtime.Count, StringComparer.Ordinal);
            for (var index = 0; index < runtime.Count; index++)
            {
                var node = runtime[index];
                if (!runtimeIndex.TryAdd(node.DebugId, index))
                {
                    return false;
                }
                if (node.ParentDebugId.Length == 0)
                {
                    continue;
                }
                if (!runtimeIndex.TryGetValue(node.ParentDebugId, out var parentIndex))
                {
                    return false;
                }
                AddRuntimeChild(parentIndex, index);
            }
            if (!runtimeIndex.TryGetValue(runtimeRootDebugId, out runtimeRootIndex))
            {
                runtimeRootIndex = -1;
            }
        }
        if (runtimeRootIndex < 0
            || runtime[runtimeRootIndex].ParentDebugId.Length != 0
            || !string.Equals(source[0].ClassName, runtime[runtimeRootIndex].ClassName, StringComparison.Ordinal))
        {
            return false;
        }
        var runtimeIndexMilliseconds = phaseTimer.ElapsedMilliseconds - sourceIndexMilliseconds;

        var sourceShapeByIndex = new int[source.Count];
        Array.Fill(sourceShapeByIndex, -1);
        var exactShapeByKey = new Dictionary<CanonicalShape, int>();
        var leafShapeByIdentity = new Dictionary<(string Class, string Name), int>();
        var sourceShapes = new List<CanonicalShape>();
        var sourceShapesBuilt = false;

        if (source.All(node => node.ShapeId >= 0))
        {
            var shapeCount = source.Max(node => node.ShapeId) + 1;
            var representativeByShape = new int[shapeCount];
            Array.Fill(representativeByShape, -1);
            for (var index = 0; index < source.Count; index++)
            {
                var shape = source[index].ShapeId;
                sourceShapeByIndex[index] = shape;
                if (representativeByShape[shape] < 0)
                {
                    representativeByShape[shape] = index;
                }
            }
            if (representativeByShape.All(index => index >= 0))
            {
                for (var shape = 0; shape < shapeCount; shape++)
                {
                    var representative = representativeByShape[shape];
                    var node = source[representative];
                    var childShapes = new Dictionary<
                        (string Class, string Name),
                        Dictionary<int, int>>();
                    for (var child = sourceFirstChild[representative];
                        child >= 0;
                        child = sourceNextSibling[child])
                    {
                        var childNode = source[child];
                        var identity = (childNode.ClassName, childNode.Name);
                        if (!childShapes.TryGetValue(identity, out var counts))
                        {
                            counts = [];
                            childShapes.Add(identity, counts);
                        }
                        counts[childNode.ShapeId] = counts.GetValueOrDefault(childNode.ShapeId) + 1;
                    }
                    sourceShapes.Add(ShapeKey(node.ClassName, node.Name, childShapes));
                }
                sourceShapesBuilt = true;
            }
        }

        int InternCanonicalLeaf(
            string className,
            string name,
            Dictionary<(string Class, string Name), int> leaves,
            List<CanonicalShape> shapes)
        {
            var identity = (className, name);
            if (!leaves.TryGetValue(identity, out var shape))
            {
                shape = shapes.Count;
                leaves.Add(identity, shape);
                shapes.Add(new(className, name, []));
            }
            return shape;
        }

        static ulong ChildShapeToken(string className, string name, int shape)
        {
            var upper = (uint)HashCode.Combine(
                StringComparer.Ordinal.GetHashCode(className),
                StringComparer.Ordinal.GetHashCode(name),
                shape);
            var lower = (uint)HashCode.Combine(
                shape,
                StringComparer.Ordinal.GetHashCode(name),
                StringComparer.Ordinal.GetHashCode(className),
                0x61c88647);
            var value = ((ulong)upper << 32) | lower;
            value ^= value >> 30;
            value *= 0xbf58476d1ce4e5b9UL;
            value ^= value >> 27;
            value *= 0x94d049bb133111ebUL;
            return value ^ (value >> 31);
        }

        static (int Class, int Name, int Children, ulong Sum, ulong SumSquares) CanonicalShapeHash(
            CanonicalShape shape)
        {
            ulong sum = 0;
            ulong sumSquares = 0;
            var childCount = 0;
            unchecked
            {
                foreach (var child in shape.Children)
                {
                    var token = ChildShapeToken(child.ClassName, child.Name, child.Shape);
                    sum += token * (uint)child.Count;
                    sumSquares += token * token * (uint)child.Count;
                    childCount += child.Count;
                }
            }
            return (
                StringComparer.Ordinal.GetHashCode(shape.ClassName),
                StringComparer.Ordinal.GetHashCode(shape.Name),
                childCount,
                sum,
                sumSquares);
        }

        void BuildHierarchyShapes(
            int count,
            int[] firstChildByIndex,
            int[] nextSiblingByIndex,
            Func<int, string> classNameAt,
            Func<int, string> nameAt,
            int[] shapeByIndex,
            Dictionary<CanonicalShape, int> shapeByKey,
            Dictionary<(string Class, string Name), int> leafByIdentity,
            List<CanonicalShape> shapes)
        {
            var candidatesByHash = new Dictionary<
                (int Class, int Name, int Children, ulong Sum, ulong SumSquares),
                List<int>>();
            for (var shape = 0; shape < shapes.Count; shape++)
            {
                var hash = CanonicalShapeHash(shapes[shape]);
                if (!candidatesByHash.TryGetValue(hash, out var candidates))
                {
                    candidates = [];
                    candidatesByHash.Add(hash, candidates);
                }
                candidates.Add(shape);
            }

            bool NodeMatchesShape(int nodeIndex, CanonicalShape shape)
            {
                if (!string.Equals(classNameAt(nodeIndex), shape.ClassName, StringComparison.Ordinal)
                    || !string.Equals(nameAt(nodeIndex), shape.Name, StringComparison.Ordinal))
                {
                    return false;
                }
                var childCount = 0;
                for (var child = firstChildByIndex[nodeIndex];
                    child >= 0;
                    child = nextSiblingByIndex[child])
                {
                    childCount++;
                }
                if (childCount != shape.Children.Sum(child => child.Count))
                {
                    return false;
                }
                foreach (var expected in shape.Children)
                {
                    var actualCount = 0;
                    for (var child = firstChildByIndex[nodeIndex];
                        child >= 0;
                        child = nextSiblingByIndex[child])
                    {
                        if (shapeByIndex[child] == expected.Shape
                            && string.Equals(classNameAt(child), expected.ClassName, StringComparison.Ordinal)
                            && string.Equals(nameAt(child), expected.Name, StringComparison.Ordinal))
                        {
                            actualCount++;
                        }
                    }
                    if (actualCount != expected.Count)
                    {
                        return false;
                    }
                }
                return true;
            }

            for (var nodeIndex = count - 1; nodeIndex >= 0; nodeIndex--)
            {
                if (shapeByIndex[nodeIndex] >= 0)
                {
                    continue;
                }
                var firstChild = firstChildByIndex[nodeIndex];
                if (firstChild < 0)
                {
                    shapeByIndex[nodeIndex] = InternCanonicalLeaf(
                        classNameAt(nodeIndex),
                        nameAt(nodeIndex),
                        leafByIdentity,
                        shapes);
                    continue;
                }

                ulong sum = 0;
                ulong sumSquares = 0;
                var childCount = 0;
                unchecked
                {
                    for (var child = firstChild;
                        child >= 0;
                        child = nextSiblingByIndex[child])
                    {
                        var token = ChildShapeToken(
                            classNameAt(child),
                            nameAt(child),
                            shapeByIndex[child]);
                        sum += token;
                        sumSquares += token * token;
                        childCount++;
                    }
                }
                var hash = (
                    StringComparer.Ordinal.GetHashCode(classNameAt(nodeIndex)),
                    StringComparer.Ordinal.GetHashCode(nameAt(nodeIndex)),
                    childCount,
                    sum,
                    sumSquares);
                if (candidatesByHash.TryGetValue(hash, out var candidates))
                {
                    var matchedShape = candidates.FirstOrDefault(
                        shape => NodeMatchesShape(nodeIndex, shapes[shape]),
                        -1);
                    if (matchedShape >= 0)
                    {
                        shapeByIndex[nodeIndex] = matchedShape;
                        continue;
                    }
                }

                var childShapes = new Dictionary<
                    (string Class, string Name),
                    Dictionary<int, int>>();
                for (var child = firstChild;
                    child >= 0;
                    child = nextSiblingByIndex[child])
                {
                    var identity = (classNameAt(child), nameAt(child));
                    if (!childShapes.TryGetValue(identity, out var counts))
                    {
                        counts = [];
                        childShapes.Add(identity, counts);
                    }
                    var childShape = shapeByIndex[child];
                    counts[childShape] = counts.GetValueOrDefault(childShape) + 1;
                }
                var key = ShapeKey(classNameAt(nodeIndex), nameAt(nodeIndex), childShapes);
                if (!shapeByKey.TryGetValue(key, out var newShape))
                {
                    newShape = shapes.Count;
                    shapeByKey.Add(key, newShape);
                    shapes.Add(key);
                }
                shapeByIndex[nodeIndex] = newShape;
                if (!candidatesByHash.TryGetValue(hash, out candidates))
                {
                    candidates = [];
                    candidatesByHash.Add(hash, candidates);
                }
                if (!candidates.Contains(newShape))
                {
                    candidates.Add(newShape);
                }
            }
        }

        int ExactSourceShape(int requestedIndex)
        {
            if (sourceShapeByIndex[requestedIndex] >= 0)
            {
                return sourceShapeByIndex[requestedIndex];
            }
            var node = source[requestedIndex];
            if (sourceFirstChild[requestedIndex] < 0)
            {
                return sourceShapeByIndex[requestedIndex] = InternCanonicalLeaf(
                    node.ClassName,
                    node.Name,
                    leafShapeByIdentity,
                    sourceShapes);
            }
            if (!sourceShapesBuilt)
            {
                BuildHierarchyShapes(
                    source.Count,
                    sourceFirstChild,
                    sourceNextSibling,
                    index => source[index].ClassName,
                    index => source[index].Name,
                    sourceShapeByIndex,
                    exactShapeByKey,
                    leafShapeByIdentity,
                    sourceShapes);
                sourceShapesBuilt = true;
            }
            return sourceShapeByIndex[requestedIndex];
        }

        var indexedCompatibility = new Dictionary<(int Shape, int Runtime), bool>();
        var indexedCompatibilityFailure = new Dictionary<(int Shape, int Runtime), string>();

        bool TryAssignShapesWith(
            IReadOnlyDictionary<int, int> requiredShapes,
            IReadOnlyList<int> candidates,
            Func<int, int, bool> canMatch,
            out Dictionary<int, List<int>> assignments)
        {
            assignments = requiredShapes.Keys.ToDictionary(
                shape => shape,
                _ => new List<int>());
            if (requiredShapes.Values.Sum() != candidates.Count)
            {
                return false;
            }
            if (requiredShapes.Count == 1)
            {
                var shape = requiredShapes.Keys.First();
                foreach (var candidate in candidates)
                {
                    if (!canMatch(shape, candidate))
                    {
                        return false;
                    }
                    assignments[shape].Add(candidate);
                }
                return true;
            }

            var compatibleShapes = new List<int>[candidates.Count];
            for (var index = 0; index < candidates.Count; index++)
            {
                compatibleShapes[index] = [];
                foreach (var shape in requiredShapes.Keys)
                {
                    if (canMatch(shape, candidates[index]))
                    {
                        compatibleShapes[index].Add(shape);
                    }
                }
                if (compatibleShapes[index].Count == 0)
                {
                    return false;
                }
            }

            var remaining = requiredShapes.ToDictionary(entry => entry.Key, entry => entry.Value);
            var assigned = new bool[candidates.Count];
            var unassigned = candidates.Count;
            while (unassigned > 0)
            {
                var progressed = false;
                for (var index = 0; index < candidates.Count; index++)
                {
                    if (assigned[index])
                    {
                        continue;
                    }
                    var available = compatibleShapes[index]
                        .Where(shape => remaining[shape] > 0)
                        .ToArray();
                    if (available.Length == 0)
                    {
                        return false;
                    }
                    if (available.Length != 1)
                    {
                        continue;
                    }
                    var shape = available[0];
                    assignments[shape].Add(candidates[index]);
                    remaining[shape]--;
                    assigned[index] = true;
                    unassigned--;
                    progressed = true;
                }

                foreach (var shape in requiredShapes.Keys)
                {
                    if (remaining[shape] == 0)
                    {
                        continue;
                    }
                    var eligible = Enumerable.Range(0, candidates.Count)
                        .Where(index => !assigned[index]
                            && compatibleShapes[index].Contains(shape))
                        .ToArray();
                    if (eligible.Length < remaining[shape])
                    {
                        return false;
                    }
                    if (eligible.Length != remaining[shape])
                    {
                        continue;
                    }
                    foreach (var index in eligible)
                    {
                        assignments[shape].Add(candidates[index]);
                        remaining[shape]--;
                        assigned[index] = true;
                        unassigned--;
                    }
                    progressed = true;
                }
                if (!progressed)
                {
                    return false;
                }
            }
            return remaining.Values.All(count => count == 0);
        }

        bool TryAssignIndexedShapes(
            IReadOnlyDictionary<int, int> requiredShapes,
            IReadOnlyList<int> candidates,
            out Dictionary<int, List<int>> assignments) => TryAssignShapesWith(
                requiredShapes,
                candidates,
                CanMatchIndexedShape,
                out assignments);

        bool CanMatchIndexedShape(int shapeId, int runtimeIndex)
        {
            var memoKey = (shapeId, runtimeIndex);
            if (indexedCompatibility.TryGetValue(memoKey, out var known))
            {
                return known;
            }
            indexedCompatibility[memoKey] = false;
            var shape = sourceShapes[shapeId];
            var candidate = runtime[runtimeIndex];
            if (!string.Equals(shape.ClassName, candidate.ClassName, StringComparison.Ordinal)
                || !string.Equals(shape.Name, candidate.Name, StringComparison.Ordinal))
            {
                indexedCompatibilityFailure[memoKey] =
                    $"expected {shape.ClassName} {shape.Name}, found {candidate.ClassName} {candidate.Name}";
                return false;
            }
            for (var childOffset = 0; childOffset < shape.Children.Length;)
            {
                var first = shape.Children[childOffset];
                var required = new Dictionary<int, int>();
                while (childOffset < shape.Children.Length
                    && string.Equals(shape.Children[childOffset].ClassName, first.ClassName, StringComparison.Ordinal)
                    && string.Equals(shape.Children[childOffset].Name, first.Name, StringComparison.Ordinal))
                {
                    var child = shape.Children[childOffset++];
                    required.Add(child.Shape, child.Count);
                }
                var runtimeChildren = new List<int>();
                for (var runtimeChild = runtimeFirstChild[runtimeIndex];
                    runtimeChild >= 0;
                    runtimeChild = runtimeNextSibling[runtimeChild])
                {
                    var node = runtime[runtimeChild];
                    if (string.Equals(node.ClassName, first.ClassName, StringComparison.Ordinal)
                        && string.Equals(node.Name, first.Name, StringComparison.Ordinal))
                    {
                        runtimeChildren.Add(runtimeChild);
                    }
                }
                if (!TryAssignIndexedShapes(required, runtimeChildren, out _))
                {
                    var nestedFailure = required.Keys
                        .SelectMany(requiredShape => runtimeChildren.Select(runtimeChild =>
                            indexedCompatibilityFailure.GetValueOrDefault(
                                (requiredShape, runtimeChild))))
                        .FirstOrDefault(reason => reason is not null);
                    indexedCompatibilityFailure[memoKey] = nestedFailure is null
                        ? $"ambiguous {first.ClassName} {first.Name} child assignment"
                        : $"under {candidate.ClassName} {candidate.Name}: {nestedFailure}";
                    return false;
                }
            }
            indexedCompatibility[memoKey] = true;
            return true;
        }

        var requirementsByShape = new Dictionary<
            int,
            Dictionary<(string Class, string Name), Dictionary<int, int>>>();
        var discriminantChecks = 0;

        Dictionary<(string Class, string Name), Dictionary<int, int>> ShapeRequirements(int shapeId)
        {
            if (requirementsByShape.TryGetValue(shapeId, out var known))
            {
                return known;
            }
            var requirements = new Dictionary<
                (string Class, string Name),
                Dictionary<int, int>>();
            foreach (var child in sourceShapes[shapeId].Children)
            {
                var identity = (child.ClassName, child.Name);
                if (!requirements.TryGetValue(identity, out var shapes))
                {
                    shapes = [];
                    requirements.Add(identity, shapes);
                }
                shapes.Add(child.Shape, child.Count);
            }
            requirementsByShape.Add(shapeId, requirements);
            return requirements;
        }

        static bool SameShapeCounts(
            IReadOnlyDictionary<int, int>? left,
            IReadOnlyDictionary<int, int>? right)
        {
            if (ReferenceEquals(left, right))
            {
                return true;
            }
            if (left is null || right is null || left.Count != right.Count)
            {
                return false;
            }
            foreach (var entry in left)
            {
                if (!right.TryGetValue(entry.Key, out var count) || count != entry.Value)
                {
                    return false;
                }
            }
            return true;
        }

        bool CanMatchDiscriminant(
            int shapeId,
            int runtimeIndex,
            IReadOnlyList<int> alternatives)
        {
            discriminantChecks++;
            var shape = sourceShapes[shapeId];
            var candidate = runtime[runtimeIndex];
            if (!string.Equals(shape.ClassName, candidate.ClassName, StringComparison.Ordinal)
                || !string.Equals(shape.Name, candidate.Name, StringComparison.Ordinal))
            {
                return false;
            }
            if (alternatives.Count <= 1)
            {
                return true;
            }

            var alternativeRequirements = alternatives
                .Select(ShapeRequirements)
                .ToArray();
            var identities = alternativeRequirements
                .SelectMany(requirements => requirements.Keys)
                .Distinct()
                .ToArray();
            var currentRequirements = ShapeRequirements(shapeId);
            foreach (var identity in identities)
            {
                alternativeRequirements[0].TryGetValue(identity, out var firstCounts);
                var differs = false;
                for (var index = 1; index < alternativeRequirements.Length; index++)
                {
                    alternativeRequirements[index].TryGetValue(identity, out var otherCounts);
                    if (!SameShapeCounts(firstCounts, otherCounts))
                    {
                        differs = true;
                        break;
                    }
                }
                if (!differs
                    || !currentRequirements.TryGetValue(identity, out var required))
                {
                    // A source shape without this identity remains compatible
                    // with an unrelated runtime-only identity, matching the
                    // established full structural verifier.
                    continue;
                }

                var runtimeChildren = new List<int>();
                for (var runtimeChild = runtimeFirstChild[runtimeIndex];
                    runtimeChild >= 0;
                    runtimeChild = runtimeNextSibling[runtimeChild])
                {
                    var node = runtime[runtimeChild];
                    if (string.Equals(node.ClassName, identity.Class, StringComparison.Ordinal)
                        && string.Equals(node.Name, identity.Name, StringComparison.Ordinal))
                    {
                        runtimeChildren.Add(runtimeChild);
                    }
                }
                if (required.Values.Sum() != runtimeChildren.Count)
                {
                    return false;
                }
                var childAlternatives = alternativeRequirements
                    .SelectMany(requirements => requirements.TryGetValue(identity, out var counts)
                        ? counts.Keys
                        : Enumerable.Empty<int>())
                    .Distinct()
                    .ToArray();
                if (childAlternatives.Length <= 1)
                {
                    continue;
                }
                if (!TryAssignShapesWith(
                    required,
                    runtimeChildren,
                    (childShape, runtimeChild) => CanMatchDiscriminant(
                        childShape,
                        runtimeChild,
                        childAlternatives),
                    out _))
                {
                    return false;
                }
            }
            return true;
        }

        if (precomputedRuntimeShapes is not null
            && precomputedRuntimeShapes.ShapeByIndex.Length != runtime.Count)
        {
            failureReason = "precomputed runtime shape count differs";
            return false;
        }
        var runtimeShapeByIndex = precomputedRuntimeShapes?.ShapeByIndex
            ?? new int[runtime.Count];
        if (precomputedRuntimeShapes is null)
        {
            Array.Fill(runtimeShapeByIndex, -1);
        }
        var runtimeShapeByKey = new Dictionary<CanonicalShape, int>();
        var runtimeLeafShapeByIdentity = new Dictionary<(string Class, string Name), int>();
        var runtimeShapes = precomputedRuntimeShapes?.Shapes
            ?? new List<CanonicalShape>();

        var runtimeShapesBuilt = precomputedRuntimeShapes is not null;
        int ExactRuntimeShape(int requestedIndex)
        {
            if (runtimeShapeByIndex[requestedIndex] >= 0)
            {
                return runtimeShapeByIndex[requestedIndex];
            }
            var node = runtime[requestedIndex];
            if (runtimeFirstChild[requestedIndex] < 0)
            {
                return runtimeShapeByIndex[requestedIndex] = InternCanonicalLeaf(
                    node.ClassName,
                    node.Name,
                    runtimeLeafShapeByIdentity,
                    runtimeShapes);
            }
            if (!runtimeShapesBuilt)
            {
                BuildHierarchyShapes(
                    runtime.Count,
                    runtimeFirstChild,
                    runtimeNextSibling,
                    index => runtime[index].ClassName,
                    index => runtime[index].Name,
                    runtimeShapeByIndex,
                    runtimeShapeByKey,
                    runtimeLeafShapeByIdentity,
                    runtimeShapes);
                runtimeShapesBuilt = true;
            }
            return runtimeShapeByIndex[requestedIndex];
        }

        var runtimeRequirementsByShape = new Dictionary<
            int,
            Dictionary<(string Class, string Name), Dictionary<int, int>>>();

        Dictionary<(string Class, string Name), Dictionary<int, int>> RuntimeShapeRequirements(
            int shapeId)
        {
            if (runtimeRequirementsByShape.TryGetValue(shapeId, out var known))
            {
                return known;
            }
            var requirements = new Dictionary<
                (string Class, string Name),
                Dictionary<int, int>>();
            foreach (var child in runtimeShapes[shapeId].Children)
            {
                var identity = (child.ClassName, child.Name);
                if (!requirements.TryGetValue(identity, out var shapes))
                {
                    shapes = [];
                    requirements.Add(identity, shapes);
                }
                shapes.Add(child.Shape, child.Count);
            }
            runtimeRequirementsByShape.Add(shapeId, requirements);
            return requirements;
        }

        var dagCompatibility = new Dictionary<(int Source, int Runtime), bool>();

        bool TryAssignShapeCounts(
            IReadOnlyDictionary<int, int> required,
            IReadOnlyDictionary<int, int> candidates)
        {
            if (required.Values.Sum() != candidates.Values.Sum())
            {
                return false;
            }
            if (required.Count == 1)
            {
                var sourceShape = required.Keys.First();
                return candidates.Keys.All(runtimeShape =>
                    CanMatchShapeDag(sourceShape, runtimeShape));
            }

            var compatibleSources = candidates.Keys.ToDictionary(
                runtimeShape => runtimeShape,
                runtimeShape => required.Keys
                    .Where(sourceShape => CanMatchShapeDag(sourceShape, runtimeShape))
                    .ToArray());
            if (compatibleSources.Values.Any(shapes => shapes.Length == 0))
            {
                return false;
            }
            var remainingSource = required.ToDictionary(entry => entry.Key, entry => entry.Value);
            var remainingRuntime = candidates.ToDictionary(entry => entry.Key, entry => entry.Value);
            while (remainingRuntime.Values.Any(count => count > 0))
            {
                var progressed = false;
                foreach (var runtimeShape in candidates.Keys)
                {
                    var runtimeCount = remainingRuntime[runtimeShape];
                    if (runtimeCount == 0)
                    {
                        continue;
                    }
                    var available = compatibleSources[runtimeShape]
                        .Where(sourceShape => remainingSource[sourceShape] > 0)
                        .ToArray();
                    if (available.Length == 0)
                    {
                        return false;
                    }
                    if (available.Length != 1)
                    {
                        continue;
                    }
                    var sourceShape = available[0];
                    if (runtimeCount > remainingSource[sourceShape])
                    {
                        return false;
                    }
                    remainingSource[sourceShape] -= runtimeCount;
                    remainingRuntime[runtimeShape] = 0;
                    progressed = true;
                }

                foreach (var sourceShape in required.Keys)
                {
                    var sourceCount = remainingSource[sourceShape];
                    if (sourceCount == 0)
                    {
                        continue;
                    }
                    var eligible = candidates.Keys
                        .Where(runtimeShape => remainingRuntime[runtimeShape] > 0
                            && compatibleSources[runtimeShape].Contains(sourceShape))
                        .ToArray();
                    var eligibleCount = eligible.Sum(runtimeShape => remainingRuntime[runtimeShape]);
                    if (eligibleCount < sourceCount)
                    {
                        return false;
                    }
                    if (eligibleCount != sourceCount)
                    {
                        continue;
                    }
                    foreach (var runtimeShape in eligible)
                    {
                        remainingRuntime[runtimeShape] = 0;
                    }
                    remainingSource[sourceShape] = 0;
                    progressed = true;
                }
                if (!progressed)
                {
                    return false;
                }
            }
            return remainingSource.Values.All(count => count == 0);
        }

        bool CanMatchShapeDag(int sourceShapeId, int runtimeShapeId)
        {
            var memoKey = (sourceShapeId, runtimeShapeId);
            if (dagCompatibility.TryGetValue(memoKey, out var known))
            {
                return known;
            }
            dagCompatibility[memoKey] = false;
            var sourceShape = sourceShapes[sourceShapeId];
            var runtimeShape = runtimeShapes[runtimeShapeId];
            if (!string.Equals(sourceShape.ClassName, runtimeShape.ClassName, StringComparison.Ordinal)
                || !string.Equals(sourceShape.Name, runtimeShape.Name, StringComparison.Ordinal))
            {
                return false;
            }
            var runtimeRequirements = RuntimeShapeRequirements(runtimeShapeId);
            foreach (var sourceRequirement in ShapeRequirements(sourceShapeId))
            {
                if (!runtimeRequirements.TryGetValue(sourceRequirement.Key, out var runtimeChildren)
                    || !TryAssignShapeCounts(sourceRequirement.Value, runtimeChildren))
                {
                    return false;
                }
            }
            dagCompatibility[memoKey] = true;
            return true;
        }

        var sourceToRuntime = new int[source.Count];
        Array.Fill(sourceToRuntime, -1);
        sourceToRuntime[0] = runtimeRootIndex;
        var matched = new ManagedHierarchyMatch[source.Count];
        matched[0] = new(
            source[0].SourceId,
            runtimeRootDebugId,
            source[0].SourceId,
            runtimeRootDebugId);
        var parentGroups = 0;
        var differingDuplicateGroups = 0;

        for (var parentIndex = 0; parentIndex < source.Count; parentIndex++)
        {
            var firstChild = sourceFirstChild[parentIndex];
            if (firstChild < 0)
            {
                continue;
            }
            var runtimeParentIndex = sourceToRuntime[parentIndex];
            if (runtimeParentIndex < 0)
            {
                return false;
            }
            if (sourceNextSibling[firstChild] < 0)
            {
                var sourceChild = source[firstChild];
                var runtimeChildIndex = -1;
                for (var candidateIndex = runtimeFirstChild[runtimeParentIndex];
                    candidateIndex >= 0;
                    candidateIndex = runtimeNextSibling[candidateIndex])
                {
                    var candidate = runtime[candidateIndex];
                    if (!string.Equals(sourceChild.ClassName, candidate.ClassName, StringComparison.Ordinal)
                        || !string.Equals(sourceChild.Name, candidate.Name, StringComparison.Ordinal))
                    {
                        continue;
                    }
                    if (runtimeChildIndex >= 0)
                    {
                        return false;
                    }
                    runtimeChildIndex = candidateIndex;
                }
                if (runtimeChildIndex < 0)
                {
                    return false;
                }
                var runtimeChild = runtime[runtimeChildIndex];
                sourceToRuntime[firstChild] = runtimeChildIndex;
                var parentBinding = matched[parentIndex];
                matched[firstChild] = new(
                    sourceChild.SourceId,
                    runtimeChild.DebugId,
                    parentIndex == 0 ? sourceChild.SourceId : parentBinding.RootSourceId,
                    runtimeParentIndex == runtimeRootIndex
                        ? runtimeChild.DebugId
                        : parentBinding.RootDebugId);
                continue;
            }

            if (source[parentIndex].ChildShapeMode == 0)
            {
                var alignedSource = firstChild;
                var alignedRuntime = runtimeFirstChild[runtimeParentIndex];
                while (alignedSource >= 0 && alignedRuntime >= 0)
                {
                    var sourceChild = source[alignedSource];
                    var runtimeChild = runtime[alignedRuntime];
                    if (!string.Equals(
                            sourceChild.ClassName,
                            runtimeChild.ClassName,
                            StringComparison.Ordinal)
                        || !string.Equals(
                            sourceChild.Name,
                            runtimeChild.Name,
                            StringComparison.Ordinal))
                    {
                        break;
                    }
                    alignedSource = sourceNextSibling[alignedSource];
                    alignedRuntime = runtimeNextSibling[alignedRuntime];
                }
                if (alignedSource < 0 && alignedRuntime < 0)
                {
                    var sourceChildIndex = firstChild;
                    var runtimeChildIndex = runtimeFirstChild[runtimeParentIndex];
                    while (sourceChildIndex >= 0)
                    {
                        var sourceChild = source[sourceChildIndex];
                        var runtimeChild = runtime[runtimeChildIndex];
                        sourceToRuntime[sourceChildIndex] = runtimeChildIndex;
                        var parentBinding = matched[parentIndex];
                        matched[sourceChildIndex] = new(
                            sourceChild.SourceId,
                            runtimeChild.DebugId,
                            parentIndex == 0 ? sourceChild.SourceId : parentBinding.RootSourceId,
                            runtimeParentIndex == runtimeRootIndex
                                ? runtimeChild.DebugId
                                : parentBinding.RootDebugId);
                        sourceChildIndex = sourceNextSibling[sourceChildIndex];
                        runtimeChildIndex = runtimeNextSibling[runtimeChildIndex];
                    }
                    continue;
                }
            }

            parentGroups++;
            var groups = new Dictionary<(string Class, string Name), OccurrenceGroup>();
            var hasDifferentDuplicateShapes = false;
            for (var childIndex = firstChild;
                childIndex >= 0;
                childIndex = sourceNextSibling[childIndex])
            {
                var child = source[childIndex];
                var identity = (child.ClassName, child.Name);
                if (groups.TryGetValue(identity, out var group))
                {
                    group.SourceCount++;
                    groups[identity] = group;
                }
                else
                {
                    groups.Add(identity, new()
                    {
                        SourceCount = 1,
                        RuntimeFirst = -1,
                        RuntimeLast = -1,
                        RuntimeNext = -1,
                        SourceShape = -1,
                    });
                }
            }
            for (var childIndex = firstChild;
                childIndex >= 0;
                childIndex = sourceNextSibling[childIndex])
            {
                var child = source[childIndex];
                var identity = (child.ClassName, child.Name);
                var group = groups[identity];
                if (group.SourceCount <= 1)
                {
                    continue;
                }
                var shape = ExactSourceShape(childIndex);
                if (group.SourceShape == -2)
                {
                    continue;
                }
                if (group.SourceShape >= 0)
                {
                    if (shape != group.SourceShape)
                    {
                        group.SourceShape = -2;
                        groups[identity] = group;
                        hasDifferentDuplicateShapes = true;
                    }
                }
                else
                {
                    group.SourceShape = shape;
                    groups[identity] = group;
                }
            }

            if (hasDifferentDuplicateShapes)
            {
                differingDuplicateGroups += groups.Count(entry => entry.Value.SourceShape == -2);
                for (var candidateIndex = runtimeFirstChild[runtimeParentIndex];
                    candidateIndex >= 0;
                    candidateIndex = runtimeNextSibling[candidateIndex])
                {
                    var candidate = runtime[candidateIndex];
                    var identity = (candidate.ClassName, candidate.Name);
                    if (!groups.TryGetValue(identity, out var group))
                    {
                        continue;
                    }
                    if (group.RuntimeFirst < 0)
                    {
                        group.RuntimeFirst = candidateIndex;
                    }
                    else
                    {
                        runtimeNextMatch[group.RuntimeLast] = candidateIndex;
                    }
                    group.RuntimeLast = candidateIndex;
                    group.RuntimeCount++;
                    groups[identity] = group;
                }
                foreach (var group in groups.Values)
                {
                    if (group.RuntimeCount != group.SourceCount)
                    {
                        failureReason = "runtime sibling identity count differs";
                        return false;
                    }
                }

                var assignedRuntime = new Dictionary<int, int>();
                foreach (var groupEntry in groups.Where(entry => entry.Value.SourceShape == -2))
                {
                    var sourceByShape = new Dictionary<int, List<int>>();
                    for (var childIndex = firstChild;
                        childIndex >= 0;
                        childIndex = sourceNextSibling[childIndex])
                    {
                        var child = source[childIndex];
                        if (!string.Equals(child.ClassName, groupEntry.Key.Class, StringComparison.Ordinal)
                            || !string.Equals(child.Name, groupEntry.Key.Name, StringComparison.Ordinal))
                        {
                            continue;
                        }
                        var shape = ExactSourceShape(childIndex);
                        if (!sourceByShape.TryGetValue(shape, out var sameShape))
                        {
                            sameShape = [];
                            sourceByShape.Add(shape, sameShape);
                        }
                        sameShape.Add(childIndex);
                    }
                    var runtimeCandidates = new List<int>();
                    for (var candidateIndex = groupEntry.Value.RuntimeFirst;
                        candidateIndex >= 0;
                        candidateIndex = runtimeNextMatch[candidateIndex])
                    {
                        runtimeCandidates.Add(candidateIndex);
                    }
                    var requiredShapes = sourceByShape.ToDictionary(
                        entry => entry.Key,
                        entry => entry.Value.Count);
                    if (!TryAssignShapesWith(
                        requiredShapes,
                        runtimeCandidates,
                        (shape, runtimeCandidate) => CanMatchShapeDag(
                            shape,
                            ExactRuntimeShape(runtimeCandidate)),
                        out var assignments))
                    {
                        var details = requiredShapes.Keys
                            .SelectMany(shape => runtimeCandidates.Select(runtimeCandidate =>
                                indexedCompatibilityFailure.GetValueOrDefault(
                                    (shape, runtimeCandidate))))
                            .Where(reason => reason is not null)
                            .Distinct(StringComparer.Ordinal)
                            .Take(3);
                        failureReason =
                            $"runtime duplicate shape differs for {groupEntry.Key.Class} {groupEntry.Key.Name}: " +
                            string.Join("; ", details);
                        return false;
                    }
                    foreach (var sourceShape in sourceByShape)
                    {
                        var matchedRuntime = assignments[sourceShape.Key];
                        for (var index = 0; index < sourceShape.Value.Count; index++)
                        {
                            assignedRuntime.Add(sourceShape.Value[index], matchedRuntime[index]);
                        }
                    }
                }

                for (var childIndex = firstChild;
                    childIndex >= 0;
                    childIndex = sourceNextSibling[childIndex])
                {
                    var sourceChild = source[childIndex];
                    var identity = (sourceChild.ClassName, sourceChild.Name);
                    var group = groups[identity];
                    int runtimeChildIndex;
                    if (group.SourceShape == -2)
                    {
                        runtimeChildIndex = assignedRuntime[childIndex];
                    }
                    else
                    {
                        runtimeChildIndex = group.RuntimeNext < 0
                            ? group.RuntimeFirst
                            : group.RuntimeNext;
                        group.RuntimeNext = runtimeNextMatch[runtimeChildIndex];
                        groups[identity] = group;
                    }
                    var runtimeChild = runtime[runtimeChildIndex];
                    sourceToRuntime[childIndex] = runtimeChildIndex;
                    var parentBinding = matched[parentIndex];
                    matched[childIndex] = new(
                        sourceChild.SourceId,
                        runtimeChild.DebugId,
                        parentIndex == 0 ? sourceChild.SourceId : parentBinding.RootSourceId,
                        runtimeParentIndex == runtimeRootIndex
                            ? runtimeChild.DebugId
                            : parentBinding.RootDebugId);
                }
                continue;
            }

            for (var candidateIndex = runtimeFirstChild[runtimeParentIndex];
                candidateIndex >= 0;
                candidateIndex = runtimeNextSibling[candidateIndex])
            {
                var candidate = runtime[candidateIndex];
                var identity = (candidate.ClassName, candidate.Name);
                if (!groups.TryGetValue(identity, out var group))
                {
                    continue;
                }
                if (group.RuntimeFirst < 0)
                {
                    group.RuntimeFirst = candidateIndex;
                }
                else
                {
                    runtimeNextMatch[group.RuntimeLast] = candidateIndex;
                }
                group.RuntimeLast = candidateIndex;
                group.RuntimeCount++;
                groups[identity] = group;
            }
            foreach (var group in groups.Values)
            {
                if (group.RuntimeCount != group.SourceCount)
                {
                    failureReason = "runtime sibling identity count differs";
                    return false;
                }
            }

            for (var childIndex = firstChild;
                childIndex >= 0;
                childIndex = sourceNextSibling[childIndex])
            {
                var sourceChild = source[childIndex];
                var identity = (sourceChild.ClassName, sourceChild.Name);
                var group = groups[identity];
                var runtimeChildIndex = group.RuntimeNext < 0
                    ? group.RuntimeFirst
                    : group.RuntimeNext;
                group.RuntimeNext = runtimeNextMatch[runtimeChildIndex];
                groups[identity] = group;
                var runtimeChild = runtime[runtimeChildIndex];
                sourceToRuntime[childIndex] = runtimeChildIndex;
                var parentBinding = matched[parentIndex];
                matched[childIndex] = new(
                    sourceChild.SourceId,
                    runtimeChild.DebugId,
                    parentIndex == 0 ? sourceChild.SourceId : parentBinding.RootSourceId,
                    runtimeParentIndex == runtimeRootIndex
                        ? runtimeChild.DebugId
                        : parentBinding.RootDebugId);
            }
        }
        if (sourceToRuntime.Any(index => index < 0))
        {
            return false;
        }
        bindings = matched;
        var bindMilliseconds = phaseTimer.ElapsedMilliseconds
            - sourceIndexMilliseconds
            - runtimeIndexMilliseconds;
        timing = $"source-index {sourceIndexMilliseconds} ms, " +
            $"runtime-index {runtimeIndexMilliseconds} ms, bind {bindMilliseconds} ms, " +
            $"grouped-parents {parentGroups}, differing-groups {differingDuplicateGroups}, " +
            $"source-shapes {sourceShapes.Count}, runtime-shapes {runtimeShapes.Count}, " +
            $"dag-pairs {dagCompatibility.Count}, discriminant-checks {discriminantChecks}";
        failureReason = string.Empty;
        return true;
    }

    private static CanonicalShape ShapeKey(
        string className,
        string name,
        IReadOnlyDictionary<(string Class, string Name), Dictionary<int, int>> childShapes)
    {
        var children = new CanonicalChild[childShapes.Sum(group => group.Value.Count)];
        var index = 0;
        foreach (var group in childShapes)
        {
            foreach (var count in group.Value)
            {
                children[index++] = new(
                    group.Key.Class,
                    group.Key.Name,
                    count.Key,
                    count.Value);
            }
        }
        Array.Sort(children, static (left, right) =>
        {
            var order = string.Compare(left.ClassName, right.ClassName, StringComparison.Ordinal);
            if (order != 0)
            {
                return order;
            }
            order = string.Compare(left.Name, right.Name, StringComparison.Ordinal);
            return order != 0 ? order : left.Shape.CompareTo(right.Shape);
        });
        return new(className, name, children);
    }

    internal readonly record struct CanonicalChild(
        string ClassName,
        string Name,
        int Shape,
        int Count);

    internal sealed class CanonicalShape : IEquatable<CanonicalShape>
    {
        private readonly int _hashCode;

        internal CanonicalShape(string className, string name, CanonicalChild[] children)
        {
            ClassName = className;
            Name = name;
            Children = children;
            var hash = new HashCode();
            hash.Add(className, StringComparer.Ordinal);
            hash.Add(name, StringComparer.Ordinal);
            foreach (var child in children)
            {
                hash.Add(child.ClassName, StringComparer.Ordinal);
                hash.Add(child.Name, StringComparer.Ordinal);
                hash.Add(child.Shape);
                hash.Add(child.Count);
            }
            _hashCode = hash.ToHashCode();
        }

        internal string ClassName { get; }
        internal string Name { get; }
        internal CanonicalChild[] Children { get; }

        public bool Equals(CanonicalShape? other) => other is not null
            && string.Equals(ClassName, other.ClassName, StringComparison.Ordinal)
            && string.Equals(Name, other.Name, StringComparison.Ordinal)
            && Children.AsSpan().SequenceEqual(other.Children);

        public override bool Equals(object? value) =>
            value is CanonicalShape other && Equals(other);

        public override int GetHashCode() => _hashCode;
    }

    private struct OccurrenceGroup
    {
        internal int SourceCount;
        internal int RuntimeCount;
        internal int RuntimeFirst;
        internal int RuntimeLast;
        internal int RuntimeNext;
        internal int SourceShape;
    }

    private sealed record SourceShape(
        string ClassName,
        string Name,
        IReadOnlyDictionary<(string Class, string Name), Dictionary<int, int>> ChildShapes);
}
