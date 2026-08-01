using System.Buffers.Binary;
using System.Security.Cryptography;

namespace Carbon.RmlBridge;

internal readonly record struct ManifestIdentity(ulong High, ulong Low) : IComparable<ManifestIdentity>
{
    public static ManifestIdentity Parse(string value)
    {
        if (value.Length != 32 || value.Any(character => !Uri.IsHexDigit(character)))
        {
            throw new InvalidDataException("manifest identity must be a nonzero 128-bit hexadecimal value");
        }
        return FromBytes(Convert.FromHexString(value));
    }

    public static ManifestIdentity FromBytes(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length != 16)
        {
            throw new InvalidDataException("manifest identity must contain 16 bytes");
        }
        var identity = new ManifestIdentity(
            BinaryPrimitives.ReadUInt64BigEndian(bytes),
            BinaryPrimitives.ReadUInt64BigEndian(bytes[8..]));
        if (identity.High == 0 && identity.Low == 0)
        {
            throw new InvalidDataException("manifest identity cannot be zero");
        }
        return identity;
    }

    public void Write(Span<byte> output)
    {
        if (output.Length < 16)
        {
            throw new ArgumentException("manifest identity output is too short", nameof(output));
        }
        BinaryPrimitives.WriteUInt64BigEndian(output, High);
        BinaryPrimitives.WriteUInt64BigEndian(output[8..], Low);
    }

    public int CompareTo(ManifestIdentity other)
    {
        var high = High.CompareTo(other.High);
        return high != 0 ? high : Low.CompareTo(other.Low);
    }

    public override string ToString() => $"{High:x16}{Low:x16}";
}

/// <summary>
/// Allocates identities from never-resumed 112-bit random blocks. The final
/// 16 bits are a big-endian counter, so a complete block contains 65,536
/// identities while remaining compact in Carbon's artifact sideband.
/// </summary>
internal sealed class ManifestIdentityAllocator
{
    internal const int PrefixLength = 14;
    private const int IdentitiesPerBlock = 1 << 16;

    private readonly Func<byte[]> _newPrefix;
    private readonly HashSet<string> _issuedPrefixes = new(StringComparer.Ordinal);
    private byte[] _prefix = [];
    private int _nextCounter = IdentitiesPerBlock;

    public ManifestIdentityAllocator(Func<byte[]>? newPrefix = null)
    {
        _newPrefix = newPrefix ?? NewRandomPrefix;
    }

    public ManifestIdentity Next()
    {
        if (_nextCounter == IdentitiesPerBlock)
        {
            RotateBlock();
        }

        Span<byte> bytes = stackalloc byte[16];
        _prefix.CopyTo(bytes);
        BinaryPrimitives.WriteUInt16BigEndian(bytes[PrefixLength..], (ushort)_nextCounter);
        _nextCounter += 1;
        return ManifestIdentity.FromBytes(bytes);
    }

    public void AbandonBlock()
    {
        _nextCounter = IdentitiesPerBlock;
    }

    private void RotateBlock()
    {
        while (true)
        {
            var prefix = _newPrefix();
            if (prefix.Length != PrefixLength)
            {
                throw new InvalidDataException($"manifest identity prefix must contain {PrefixLength} bytes");
            }
            if (prefix.All(value => value == 0))
            {
                continue;
            }

            var key = Convert.ToHexString(prefix);
            if (_issuedPrefixes.Add(key))
            {
                _prefix = prefix.ToArray();
                _nextCounter = 0;
                return;
            }
        }
    }

    private static byte[] NewRandomPrefix()
    {
        var prefix = new byte[PrefixLength];
        RandomNumberGenerator.Fill(prefix);
        return prefix;
    }
}

internal readonly record struct ManifestIdentityBinding(nuint Handle, string SourceId);

internal sealed record ManifestIdentityServiceAnchor(
    string SourceId,
    string ClassName,
    string Name);

internal sealed record ManifestIdentityRebinding(
    string SourceId,
    string ParentSourceId,
    string ClassName,
    string Name,
    string Kind,
    string? RelatedSourceId);

internal static class ManifestIdentityBootstrapResolver
{
    public static IReadOnlyList<ManifestIdentityBinding> Resolve(
        CaptureRuntimeHierarchyPayload runtime,
        IEnumerable<ManifestIdentityBinding> markerBindings,
        IReadOnlyList<ManifestIdentityRebinding> rebindings,
        IReadOnlyList<ManifestIdentityServiceAnchor>? serviceAnchors = null)
    {
        if (runtime.Nodes.Length == 0)
        {
            throw new InvalidDataException("manifest identity bootstrap runtime hierarchy is empty");
        }

        var indexByHandle = runtime.Nodes
            .Select((node, index) => (node.Handle, Index: index))
            .ToDictionary(entry => entry.Handle, entry => entry.Index);
        var children = new Dictionary<(int Parent, string ClassName, string Name), List<int>>();
        var childCounts = new int[runtime.Nodes.Length];
        for (var index = 1; index < runtime.Nodes.Length; index++)
        {
            var node = runtime.Nodes[index];
            childCounts[node.ParentIndex] += 1;
            var key = (node.ParentIndex, node.ClassName, node.Name);
            if (!children.TryGetValue(key, out var matches))
            {
                matches = [];
                children.Add(key, matches);
            }
            matches.Add(index);
        }
        var referenceTargets = runtime.References.ToDictionary(
            reference => (reference.OwnerIndex, reference.Property),
            reference => reference.TargetHandle);

        var result = new List<ManifestIdentityBinding>();
        var handleBySourceId = new Dictionary<string, nuint>(StringComparer.Ordinal);
        var boundHandles = new HashSet<nuint>();
        foreach (var binding in markerBindings)
        {
            if (!indexByHandle.ContainsKey(binding.Handle)
                || !handleBySourceId.TryAdd(binding.SourceId, binding.Handle)
                || !boundHandles.Add(binding.Handle))
            {
                throw new InvalidDataException("manifest identity bootstrap marker bindings are inconsistent");
            }
            result.Add(binding);
        }

        foreach (var anchor in serviceAnchors ?? [])
        {
            if (handleBySourceId.ContainsKey(anchor.SourceId))
            {
                continue;
            }
            var candidates = runtime.Nodes
                .Skip(1)
                .Where(node =>
                    node.ParentIndex == 0
                    && string.Equals(node.ClassName, anchor.ClassName, StringComparison.Ordinal)
                    && string.Equals(node.Name, anchor.Name, StringComparison.Ordinal)
                    && !boundHandles.Contains(node.Handle))
                .ToArray();
            if (candidates.Length != 1)
            {
                throw new InvalidDataException(
                    $"manifest identity bootstrap service {anchor.ClassName} '{anchor.Name}' " +
                    $"matched {candidates.Length} unbound instances");
            }
            var handle = candidates[0].Handle;
            handleBySourceId.Add(anchor.SourceId, handle);
            boundHandles.Add(handle);
            result.Add(new(handle, anchor.SourceId));
        }

        foreach (var rebinding in rebindings)
        {
            if (handleBySourceId.ContainsKey(rebinding.SourceId))
            {
                throw new InvalidDataException(
                    $"manifest identity bootstrap repeats rehydrated source identity {rebinding.SourceId}");
            }
            if (!handleBySourceId.TryGetValue(rebinding.ParentSourceId, out var parentHandle)
                || !indexByHandle.TryGetValue(parentHandle, out var parentIndex))
            {
                throw new InvalidDataException(
                    $"manifest identity bootstrap rehydrated parent {rebinding.ParentSourceId} is not authoritative");
            }

            var key = (parentIndex, rebinding.ClassName, rebinding.Name);
            var candidates = children.TryGetValue(key, out var exactChildren)
                ? exactChildren.Where(index =>
                    !boundHandles.Contains(runtime.Nodes[index].Handle)
                    && MatchesKind(
                        runtime,
                        index,
                        rebinding,
                        childCounts,
                        handleBySourceId,
                        referenceTargets)).ToArray()
                : [];
            if (candidates.Length != 1)
            {
                throw new InvalidDataException(
                    $"manifest identity bootstrap rehydrated {rebinding.Kind} {rebinding.ClassName} " +
                    $"'{rebinding.Name}' beneath {rebinding.ParentSourceId} matched {candidates.Length} instances");
            }

            var handle = runtime.Nodes[candidates[0]].Handle;
            handleBySourceId.Add(rebinding.SourceId, handle);
            boundHandles.Add(handle);
            result.Add(new(handle, rebinding.SourceId));
        }
        return result;
    }

    private static bool MatchesKind(
        CaptureRuntimeHierarchyPayload runtime,
        int index,
        ManifestIdentityRebinding rebinding,
        IReadOnlyList<int> childCounts,
        IReadOnlyDictionary<string, nuint> handleBySourceId,
        IReadOnlyDictionary<(int Owner, string Property), nuint> referenceTargets)
    {
        var node = runtime.Nodes[index];
        var parent = runtime.Nodes[node.ParentIndex];
        return rebinding.Kind switch
        {
            "humanoidStatus" => string.Equals(node.ClassName, "Status", StringComparison.Ordinal)
                && string.Equals(parent.ClassName, "Humanoid", StringComparison.Ordinal)
                && rebinding.RelatedSourceId is null,
            "configureServerService" => string.Equals(node.ClassName, "ConfigureServerService", StringComparison.Ordinal)
                && string.Equals(parent.ClassName, "DataModel", StringComparison.Ordinal)
                && rebinding.RelatedSourceId is null,
            "filteredSelection" => string.Equals(node.ClassName, "Instance", StringComparison.Ordinal)
                && string.Equals(node.Name, "FilteredSelection", StringComparison.Ordinal)
                && string.Equals(parent.ClassName, "DataModel", StringComparison.Ordinal)
                && childCounts[index] == 0
                && rebinding.RelatedSourceId is null,
            "accessoryWeld" => IsAccessoryWeld(runtime.Nodes, node, parent)
                && rebinding.RelatedSourceId is null,
            "headWeld" => IsHeadWeld(
                node,
                parent,
                index,
                rebinding.RelatedSourceId,
                handleBySourceId,
                referenceTargets),
            "descendant" => rebinding.RelatedSourceId is null,
            _ => throw new InvalidDataException(
                $"manifest identity bootstrap rehydration kind '{rebinding.Kind}' is unsupported"),
        };
    }

    private static bool IsAccessoryWeld(
        IReadOnlyList<CaptureRuntimeNode> nodes,
        CaptureRuntimeNode node,
        CaptureRuntimeNode parent) =>
        string.Equals(node.ClassName, "Weld", StringComparison.Ordinal)
        && string.Equals(node.Name, "AccessoryWeld", StringComparison.Ordinal)
        && string.Equals(parent.ClassName, "Part", StringComparison.Ordinal)
        && string.Equals(parent.Name, "Handle", StringComparison.Ordinal)
        && parent.ParentIndex >= 0
        && string.Equals(nodes[parent.ParentIndex].ClassName, "Accessory", StringComparison.Ordinal);

    private static bool IsHeadWeld(
        CaptureRuntimeNode node,
        CaptureRuntimeNode parent,
        int index,
        string? relatedSourceId,
        IReadOnlyDictionary<string, nuint> handleBySourceId,
        IReadOnlyDictionary<(int Owner, string Property), nuint> referenceTargets) =>
        string.Equals(node.ClassName, "Weld", StringComparison.Ordinal)
        && string.Equals(node.Name, "HeadWeld", StringComparison.Ordinal)
        && string.Equals(parent.ClassName, "Part", StringComparison.Ordinal)
        && string.Equals(parent.Name, "Head", StringComparison.Ordinal)
        && relatedSourceId is not null
        && handleBySourceId.TryGetValue(relatedSourceId, out var relatedHandle)
        && referenceTargets.TryGetValue((index, "Part1"), out var targetHandle)
        && targetHandle == relatedHandle;
}

/// <summary>
/// Owns Carbon manifest identity at the native-instance seam. Structure is
/// deliberately absent: a native handle keeps its identity across rename and
/// reparent, while a new lifetime receives a fresh Carbon identity.
/// </summary>
internal sealed class ManifestIdentityLedger
{
    private readonly Dictionary<nuint, ManifestIdentity> _byHandle = [];
    private readonly HashSet<ManifestIdentity> _issued = [];
    private readonly ManifestIdentityAllocator _allocator = new();
    private readonly Func<ManifestIdentity> _newIdentity;
    private (ManifestIdentity Root, int Count, string Digest)? _bootstrapContract;
    private (ManifestIdentity CaptureId, string Digest)? _completedRemap;

    public ManifestIdentityLedger(Func<string>? newIdentity = null)
    {
        _newIdentity = newIdentity is null
            ? _allocator.Next
            : () => ManifestIdentity.Parse(newIdentity());
    }

    public bool IsAuthoritative { get; private set; }

    public int Count => _byHandle.Count;

    public bool Contains(nuint handle) => _byHandle.ContainsKey(handle);

    public bool MatchesRetainedAttachment(
        IEnumerable<nuint> currentHandles,
        nuint previousRootHandle,
        nuint currentRootHandle)
    {
        if (!IsAuthoritative
            || previousRootHandle == 0
            || currentRootHandle == 0
            || !_byHandle.ContainsKey(previousRootHandle))
        {
            return false;
        }

        var current = currentHandles.ToHashSet();
        if (!current.Contains(currentRootHandle))
        {
            return false;
        }

        var retainedDescendants = 0;
        foreach (var handle in _byHandle.Keys)
        {
            if (handle == previousRootHandle)
            {
                continue;
            }
            retainedDescendants += 1;
            if (!current.Contains(handle))
            {
                return false;
            }
        }
        return retainedDescendants > 0 || previousRootHandle == currentRootHandle;
    }

    public void Reset()
    {
        _byHandle.Clear();
        _issued.Clear();
        _bootstrapContract = null;
        _completedRemap = null;
        IsAuthoritative = false;
        _allocator.AbandonBlock();
    }

    public void Bootstrap(
        IEnumerable<ManifestIdentityBinding> bindings,
        string expectedRootSourceId,
        int expectedCount,
        string expectedDigest)
    {
        Bootstrap(bindings, expectedRootSourceId, expectedCount, expectedDigest, replaceAuthoritative: false);
    }

    public void ReplaceBootstrap(
        IEnumerable<ManifestIdentityBinding> bindings,
        string expectedRootSourceId,
        int expectedCount,
        string expectedDigest)
    {
        Bootstrap(bindings, expectedRootSourceId, expectedCount, expectedDigest, replaceAuthoritative: true);
    }

    public bool TryAdoptActiveContract(
        IReadOnlyList<string> expectedSourceIds,
        string expectedRootSourceId,
        int expectedCount,
        string expectedDigest)
    {
        var root = ManifestIdentity.Parse(expectedRootSourceId);
        var digest = NormalizeDigest(expectedDigest);
        var expected = new HashSet<ManifestIdentity>();
        foreach (var sourceId in expectedSourceIds)
        {
            if (!expected.Add(ManifestIdentity.Parse(sourceId)))
            {
                throw new InvalidDataException(
                    "manifest identity reload contract repeats a source identity");
            }
        }
        if (expected.Count != expectedCount
            || !expected.Contains(root)
            || !string.Equals(Digest(expected), digest, StringComparison.Ordinal))
        {
            throw new InvalidDataException(
                "manifest identity reload contract identity inventory is inconsistent");
        }
        if (!IsAuthoritative)
        {
            return false;
        }

        var retained = _byHandle
            .Where(pair => expected.Contains(pair.Value))
            .ToDictionary(pair => pair.Key, pair => pair.Value);
        if (retained.Count != expected.Count
            || !retained.Values.ToHashSet().SetEquals(expected))
        {
            return false;
        }

        _byHandle.Clear();
        foreach (var pair in retained)
        {
            _byHandle.Add(pair.Key, pair.Value);
        }
        _bootstrapContract = (root, expectedCount, digest);
        _completedRemap = null;
        return true;
    }

    public IReadOnlyList<ManifestIdentityBinding> SnapshotExpectedBindings(
        IReadOnlyList<string> expectedSourceIds)
    {
        var expected = expectedSourceIds
            .Select(ManifestIdentity.Parse)
            .ToHashSet();
        return _byHandle
            .Where(pair => expected.Contains(pair.Value))
            .Select(pair => new ManifestIdentityBinding(pair.Key, pair.Value.ToString()))
            .ToArray();
    }

    private void Bootstrap(
        IEnumerable<ManifestIdentityBinding> bindings,
        string expectedRootSourceId,
        int expectedCount,
        string expectedDigest,
        bool replaceAuthoritative)
    {
        var root = ManifestIdentity.Parse(expectedRootSourceId);
        var digest = NormalizeDigest(expectedDigest);
        var contract = (root, expectedCount, digest);
        if (IsAuthoritative && !replaceAuthoritative)
        {
            if (_bootstrapContract == contract)
            {
                return;
            }
            throw new InvalidOperationException("manifest identity ledger is already authoritative for another contract");
        }

        var nextByHandle = new Dictionary<nuint, ManifestIdentity>();
        var nextIds = new HashSet<ManifestIdentity>();
        foreach (var binding in bindings)
        {
            var sourceId = ManifestIdentity.Parse(binding.SourceId);
            if (binding.Handle == 0 || !nextByHandle.TryAdd(binding.Handle, sourceId))
            {
                throw new InvalidDataException("manifest identity bootstrap repeats a native handle");
            }
            if (!nextIds.Add(sourceId))
            {
                throw new InvalidDataException("manifest identity bootstrap repeats a source identity");
            }
        }
        var actualDigest = Digest(nextIds);
        var containsRoot = nextIds.Contains(root);
        if (nextByHandle.Count != expectedCount
            || !containsRoot
            || !string.Equals(actualDigest, digest, StringComparison.Ordinal))
        {
            throw new InvalidDataException(
                "manifest identity bootstrap does not match the disposable build contract " +
                $"(expected count {expectedCount}, actual {nextByHandle.Count}; " +
                $"root present {containsRoot}; expected digest {digest}, actual {actualDigest}); " +
                "reopen a fresh Carbon build after an RML or DataModel restart");
        }

        _byHandle.Clear();
        foreach (var (handle, sourceId) in nextByHandle)
        {
            _byHandle.Add(handle, sourceId);
        }
        _issued.UnionWith(nextIds);
        _bootstrapContract = contract;
        _completedRemap = null;
        IsAuthoritative = true;
    }

    public ManifestIdentity GetOrCreateIdentity(nuint handle)
    {
        if (handle == 0)
        {
            throw new InvalidDataException("manifest identity cannot bind the null native handle");
        }
        if (_byHandle.TryGetValue(handle, out var existing))
        {
            return existing;
        }
        ManifestIdentity sourceId;
        do
        {
            sourceId = _newIdentity();
        }
        while (!_issued.Add(sourceId));
        _byHandle.Add(handle, sourceId);
        return sourceId;
    }

    public string GetOrCreate(nuint handle) => GetOrCreateIdentity(handle).ToString();

    public IReadOnlyList<ManifestIdentity> Snapshot(IEnumerable<nuint> handles)
    {
        var current = handles.ToArray();
        if (!IsAuthoritative)
        {
            var retained = current.ToHashSet();
            foreach (var stale in _byHandle.Keys.Where(handle => !retained.Contains(handle)).ToArray())
            {
                _byHandle.Remove(stale);
            }
        }
        return current.Select(GetOrCreateIdentity).ToArray();
    }

    public void RebindHandle(nuint previousHandle, nuint currentHandle)
    {
        if (previousHandle == 0 || currentHandle == 0)
        {
            throw new InvalidDataException("manifest identity cannot rebind a null native handle");
        }
        if (previousHandle == currentHandle)
        {
            return;
        }
        if (!_byHandle.Remove(previousHandle, out var sourceId))
        {
            throw new InvalidOperationException("manifest identity rebind lost the previous native handle");
        }
        if (_byHandle.ContainsKey(currentHandle))
        {
            _byHandle.Add(previousHandle, sourceId);
            throw new InvalidOperationException("manifest identity rebind aliases an existing native handle");
        }
        _byHandle.Add(currentHandle, sourceId);
    }

    public void Release(nuint handle) => _byHandle.Remove(handle);

    public void ApplyRemap(
        ManifestIdentity captureId,
        IReadOnlyDictionary<ManifestIdentity, ManifestIdentity> remap)
    {
        var remapDigest = RemapDigest(remap);
        if (IsAuthoritative)
        {
            if (_completedRemap == (captureId, remapDigest))
            {
                return;
            }
            throw new InvalidOperationException("manifest identity ledger is already authoritative for another capture");
        }
        if (!remap.Keys.ToHashSet().SetEquals(_byHandle.Values))
        {
            throw new InvalidDataException("manifest identity remap does not cover the captured native ledger");
        }
        var rewritten = new Dictionary<nuint, ManifestIdentity>(_byHandle.Count);
        var active = new HashSet<ManifestIdentity>();
        foreach (var (handle, sourceId) in _byHandle)
        {
            var final = remap.GetValueOrDefault(sourceId, sourceId);
            if (!active.Add(final))
            {
                throw new InvalidDataException("manifest identity remap aliases two live native instances");
            }
            rewritten.Add(handle, final);
        }
        _byHandle.Clear();
        foreach (var pair in rewritten)
        {
            _byHandle.Add(pair.Key, pair.Value);
        }
        _issued.UnionWith(active);
        _completedRemap = (captureId, remapDigest);
        IsAuthoritative = true;
    }

    public string ActiveDigest() => Digest(_byHandle.Values);

    public static string Digest(IEnumerable<string> sourceIds) =>
        Digest(sourceIds.Select(ManifestIdentity.Parse));

    public static string Digest(IEnumerable<ManifestIdentity> sourceIds)
    {
        using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        Span<byte> bytes = stackalloc byte[16];
        foreach (var sourceId in sourceIds.Order())
        {
            sourceId.Write(bytes);
            hash.AppendData(bytes);
        }
        return Convert.ToHexString(hash.GetHashAndReset()).ToLowerInvariant();
    }

    private static string RemapDigest(IReadOnlyDictionary<ManifestIdentity, ManifestIdentity> remap)
    {
        using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        Span<byte> bytes = stackalloc byte[32];
        foreach (var pair in remap.OrderBy(pair => pair.Key))
        {
            pair.Key.Write(bytes);
            pair.Value.Write(bytes[16..]);
            hash.AppendData(bytes);
        }
        return Convert.ToHexString(hash.GetHashAndReset()).ToLowerInvariant();
    }

    private static string NormalizeDigest(string digest)
    {
        if (digest.Length != 64 || digest.Any(character => !Uri.IsHexDigit(character)))
        {
            throw new InvalidDataException("manifest identity digest must be SHA-256 hexadecimal");
        }
        return digest.ToLowerInvariant();
    }
}
