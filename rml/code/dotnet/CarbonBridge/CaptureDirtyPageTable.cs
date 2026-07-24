using System.Buffers.Binary;
using System.Security.Cryptography;

namespace Carbon.RmlBridge;

internal sealed record CapturePageTableKey(
    long EngineGeneration,
    string StudioSessionId,
    string InstanceId,
    string ManagedContractId,
    string ReflectionSchemaHash,
    string MappingFingerprint,
    bool ManifestIdentitiesAuthoritative);

internal sealed record CapturePageDefinition(
    string PageId,
    nuint[] MemberHandles);

internal enum CapturePageDisposition
{
    Serialize,
    Reuse,
}

internal sealed record CapturePageDecision(
    CapturePageDefinition Definition,
    CapturePageDisposition Disposition,
    CaptureCachedPage? ReusedPayload);

internal sealed record CaptureCachedPage(
    string Path,
    long Length,
    byte[] Digest)
{
    internal FileStream OpenRead() => new(
        Path,
        FileMode.Open,
        FileAccess.Read,
        FileShare.Read,
        bufferSize: 1024 * 1024,
        FileOptions.SequentialScan);

    internal byte[] ReadAllBytes()
    {
        var bytes = File.ReadAllBytes(Path);
        if (bytes.LongLength != Length
            || !SHA256.HashData(bytes).AsSpan().SequenceEqual(Digest))
        {
            throw new InvalidDataException(
                "capture cached page failed its length or digest check");
        }
        return bytes;
    }
}

internal sealed record CaptureDirtyPagePlan(
    string CaptureId,
    CapturePageTableKey Key,
    long HierarchySequence,
    long ChangeSequence,
    CapturePageDecision[] Pages,
    CapturePageRoutes Routes,
    bool ReusedAnyPage);

internal sealed record CapturePageRoutes(
    nuint[] Handles,
    int[] PageIndexes);

/// <summary>
/// Coalesced invalidation state for bounded native capture pages.
///
/// This deliberately is not a mutation journal. It stores no mutation order,
/// property names, old values, new values, or replayable operation. An observed
/// change only marks the owning page dirty. A fresh hierarchy/reference
/// snapshot and the global capture epoch still attest every capture.
/// </summary>
internal sealed class CaptureDirtyPageTable : IDisposable
{
    private readonly object _lock = new();
    private readonly string _storageDirectory;
    private Baseline? _baseline;
    private ActivePlan? _activePlan;
    private Pending? _pending;
    private readonly HashSet<int> _dirtyPages = [];
    private bool _poisoned;
    private bool _disposed;

    internal CaptureDirtyPageTable(string? storageDirectory = null)
    {
        _storageDirectory = storageDirectory
            ?? System.IO.Path.Combine(
                System.IO.Path.GetTempPath(),
                "carbon-capture-pages",
                Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(_storageDirectory);
    }

    internal int DirtyPageCount
    {
        get
        {
            lock (_lock)
            {
                return _dirtyPages.Count;
            }
        }
    }

    internal bool IsPoisoned
    {
        get
        {
            lock (_lock)
            {
                return _poisoned;
            }
        }
    }

    internal CaptureDirtyPagePlan Plan(
        string captureId,
        CapturePageTableKey key,
        long hierarchySequence,
        long changeSequence,
        IReadOnlyList<CapturePageDefinition> pages,
        bool allowReuse)
    {
        ArgumentException.ThrowIfNullOrEmpty(captureId);
        ArgumentNullException.ThrowIfNull(key);
        ArgumentNullException.ThrowIfNull(pages);
        if (hierarchySequence < 0 || changeSequence < 0)
        {
            throw new ArgumentOutOfRangeException(
                hierarchySequence < 0 ? nameof(hierarchySequence) : nameof(changeSequence));
        }

        var definitions = ValidateDefinitions(pages);
        Baseline? routesBaseline = null;
        CapturePageRoutes? routes = null;
        lock (_lock)
        {
            ThrowIfDisposed();
            if (_baseline is { } baseline
                && baseline.HierarchySequence == hierarchySequence
                && SamePageOrder(baseline.Pages, definitions))
            {
                routesBaseline = baseline;
                routes = baseline.Routes;
            }
        }
        routes ??= BuildRoutes(definitions);
        lock (_lock)
        {
            ThrowIfDisposed();
            // Only an explicit acknowledgement may install pending capture
            // bytes. Beginning another plan abandons an unacknowledged result.
            DeletePending();
            DeleteActivePlan();
            var baseline = _baseline;
            if (routesBaseline is not null
                && !ReferenceEquals(routesBaseline, baseline))
            {
                throw new InvalidOperationException(
                    "capture page-table baseline changed during planning");
            }
            var reusable = allowReuse
                && !_poisoned
                && baseline is not null
                && baseline.Key == key
                && baseline.HierarchySequence == hierarchySequence
                && SamePageOrder(baseline.Pages, definitions);
            var decisions = new CapturePageDecision[definitions.Length];
            var reusedAny = false;
            for (var index = 0; index < definitions.Length; index++)
            {
                var definition = definitions[index];
                if (reusable
                    && !_dirtyPages.Contains(index)
                    && baseline!.Pages[index].Payload is { Length: > 0 } payload)
                {
                    decisions[index] = new(
                        definition,
                        CapturePageDisposition.Reuse,
                        payload);
                    reusedAny = true;
                }
                else
                {
                    decisions[index] = new(
                        definition,
                        CapturePageDisposition.Serialize,
                        null);
                }
            }
            var plan = new CaptureDirtyPagePlan(
                captureId,
                key,
                hierarchySequence,
                changeSequence,
                decisions,
                routes,
                reusedAny);
            _activePlan = new(
                captureId,
                hierarchySequence,
                changeSequence,
                new CaptureCachedPage?[decisions.Length]);
            return plan;
        }
    }

    internal void StoreSerializedPage(
        CaptureDirtyPagePlan plan,
        int pageIndex,
        byte[] payload)
    {
        ArgumentNullException.ThrowIfNull(plan);
        ArgumentNullException.ThrowIfNull(payload);
        lock (_lock)
        {
            ThrowIfDisposed();
            var active = RequireActivePlan(plan);
            ValidateSerializedPageIndex(plan, pageIndex);
            if (payload.Length == 0)
            {
                throw new InvalidDataException(
                    "serialized capture page-table payload is empty");
            }
            if (active.SerializedPayloads[pageIndex] is not null)
            {
                throw new InvalidDataException(
                    "capture page-table serialized page was stored more than once");
            }
        }

        var prepared = PreparePayload(payload);
        try
        {
            lock (_lock)
            {
                ThrowIfDisposed();
                var active = RequireActivePlan(plan);
                ValidateSerializedPageIndex(plan, pageIndex);
                if (active.SerializedPayloads[pageIndex] is not null)
                {
                    throw new InvalidDataException(
                        "capture page-table serialized page was stored more than once");
                }
                File.Move(prepared.TemporaryPath, prepared.Path, overwrite: true);
                active.SerializedPayloads[pageIndex] = new(
                    prepared.Path,
                    prepared.Length,
                    prepared.Digest);
            }
        }
        finally
        {
            try
            {
                File.Delete(prepared.TemporaryPath);
            }
            catch (FileNotFoundException)
            {
            }
        }
    }

    internal void Stage(
        CaptureDirtyPagePlan plan,
        long hierarchySequence,
        long changeSequence)
    {
        ArgumentNullException.ThrowIfNull(plan);
        if (hierarchySequence != plan.HierarchySequence
            || changeSequence != plan.ChangeSequence)
        {
            throw new InvalidOperationException(
                "capture page-table epochs changed before staging");
        }

        lock (_lock)
        {
            ThrowIfDisposed();
            DeletePending();
            if (_activePlan is not { } active
                || !string.Equals(active.CaptureId, plan.CaptureId, StringComparison.Ordinal)
                || active.HierarchySequence != hierarchySequence
                || active.ChangeSequence != changeSequence)
            {
                throw new InvalidOperationException(
                    "capture page-table plan is no longer active");
            }
            var pages = new StoredPage[plan.Pages.Length];
            try
            {
                for (var index = 0; index < pages.Length; index++)
                {
                    var decision = plan.Pages[index];
                    CaptureCachedPage cached;
                    if (decision.Disposition is CapturePageDisposition.Serialize)
                    {
                        cached = active.SerializedPayloads[index]
                            ?? throw new InvalidDataException(
                                "serialized capture page-table payload is missing");
                    }
                    else
                    {
                        if (active.SerializedPayloads[index] is not null)
                        {
                            throw new InvalidDataException(
                                "reused capture page unexpectedly supplied serialized bytes");
                        }
                        cached = decision.ReusedPayload
                            ?? throw new InvalidDataException(
                                "reused capture page has no acknowledged payload");
                    }
                    pages[index] = new(
                        decision.Definition.PageId,
                        cached);
                }
                _pending = new(
                    plan.CaptureId,
                    plan.Key,
                    hierarchySequence,
                    changeSequence,
                    pages,
                    plan.Routes);
                _activePlan = null;
            }
            catch
            {
                PrunePayloads();
                throw;
            }
        }
    }

    internal void Acknowledge(string captureId)
    {
        ArgumentException.ThrowIfNullOrEmpty(captureId);
        lock (_lock)
        {
            ThrowIfDisposed();
            if (_pending is not { } pending
                || !string.Equals(pending.CaptureId, captureId, StringComparison.Ordinal))
            {
                throw new InvalidOperationException(
                    "capture page-table acknowledgement has no exact pending capture");
            }

            _baseline = new(
                pending.Key,
                pending.HierarchySequence,
                pending.ChangeSequence,
                pending.Pages,
                pending.Routes);
            _pending = null;
            _dirtyPages.Clear();
            _poisoned = false;
            PrunePayloads();
        }
    }

    internal void Discard(string captureId)
    {
        ArgumentException.ThrowIfNullOrEmpty(captureId);
        lock (_lock)
        {
            ThrowIfDisposed();
            if (_pending is { } pending
                && string.Equals(pending.CaptureId, captureId, StringComparison.Ordinal))
            {
                DeletePending();
            }
            if (_activePlan is { } active
                && string.Equals(active.CaptureId, captureId, StringComparison.Ordinal))
            {
                DeleteActivePlan();
            }
        }
    }

    internal void MarkDirty(nuint handle, long changeSequence)
    {
        if (handle == 0)
        {
            Poison();
            return;
        }
        lock (_lock)
        {
            ThrowIfDisposed();
            if (_pending is { } pending && changeSequence > pending.ChangeSequence)
            {
                DeletePending();
            }
            if (_activePlan is { } active && changeSequence > active.ChangeSequence)
            {
                DeleteActivePlan();
            }
            if (_baseline is not { } baseline
                || changeSequence <= baseline.ChangeSequence)
            {
                return;
            }
            var routeIndex = Array.BinarySearch(baseline.Routes.Handles, handle);
            if (routeIndex < 0)
            {
                _poisoned = true;
                return;
            }
            _dirtyPages.Add(baseline.Routes.PageIndexes[routeIndex]);
        }
    }

    internal void InvalidateStructure()
    {
        lock (_lock)
        {
            ThrowIfDisposed();
            _poisoned = true;
            DeletePending();
            DeleteActivePlan();
        }
    }

    internal void Poison()
    {
        lock (_lock)
        {
            ThrowIfDisposed();
            _poisoned = true;
            DeletePending();
            DeleteActivePlan();
        }
    }

    internal void Reset()
    {
        lock (_lock)
        {
            ThrowIfDisposed();
            DeletePending();
            DeleteActivePlan();
            _baseline = null;
            DeleteDirectory(_storageDirectory);
            Directory.CreateDirectory(_storageDirectory);
            _dirtyPages.Clear();
            _poisoned = false;
        }
    }

    public void Dispose()
    {
        lock (_lock)
        {
            if (_disposed)
            {
                return;
            }
            _disposed = true;
            _activePlan = null;
            _pending = null;
            _baseline = null;
            _dirtyPages.Clear();
            DeleteDirectory(_storageDirectory);
        }
    }

    internal static string ComputePageId(
        IReadOnlyList<nuint> rootHandles,
        IReadOnlyList<nuint> frontierHandles,
        IReadOnlyList<nuint> memberHandles,
        IReadOnlyList<nuint> dependencyRootHandles,
        IReadOnlyList<nuint> maskedDependencyChildHandles)
    {
        ArgumentNullException.ThrowIfNull(rootHandles);
        ArgumentNullException.ThrowIfNull(frontierHandles);
        ArgumentNullException.ThrowIfNull(memberHandles);
        ArgumentNullException.ThrowIfNull(dependencyRootHandles);
        ArgumentNullException.ThrowIfNull(maskedDependencyChildHandles);
        using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        hash.AppendData("carbon-capture-page-v2\0"u8);
        AppendHandles(hash, rootHandles);
        AppendHandles(hash, frontierHandles);
        AppendHandles(hash, memberHandles);
        AppendHandles(hash, dependencyRootHandles);
        AppendHandles(hash, maskedDependencyChildHandles);
        return Convert.ToHexStringLower(hash.GetHashAndReset());
    }

    internal static string ComputeMappingFingerprint(
        IEnumerable<string> mappedRootSourceIds)
    {
        ArgumentNullException.ThrowIfNull(mappedRootSourceIds);
        using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        hash.AppendData("carbon-capture-mapped-roots-v1\0"u8);
        foreach (var sourceId in mappedRootSourceIds.Order(StringComparer.Ordinal))
        {
            hash.AppendData(System.Text.Encoding.ASCII.GetBytes(sourceId));
            hash.AppendData([0]);
        }
        return Convert.ToHexStringLower(hash.GetHashAndReset());
    }

    private static void AppendHandles(
        IncrementalHash hash,
        IReadOnlyList<nuint> handles)
    {
        Span<byte> bytes = stackalloc byte[sizeof(ulong)];
        BinaryPrimitives.WriteUInt64LittleEndian(bytes, checked((ulong)handles.Count));
        hash.AppendData(bytes);
        foreach (var handle in handles)
        {
            BinaryPrimitives.WriteUInt64LittleEndian(bytes, checked((ulong)handle));
            hash.AppendData(bytes);
        }
    }

    private static CapturePageDefinition[] ValidateDefinitions(
        IReadOnlyList<CapturePageDefinition> pages)
    {
        var definitions = pages.ToArray();
        var pageIds = new HashSet<string>(StringComparer.Ordinal);
        foreach (var page in definitions)
        {
            if (string.IsNullOrEmpty(page.PageId)
                || page.MemberHandles is null
                || page.MemberHandles.Length == 0)
            {
                throw new InvalidDataException(
                    "capture page definition requires an identity and members");
            }
            if (!pageIds.Add(page.PageId))
            {
                throw new InvalidDataException(
                    "capture page definition repeats a page identity");
            }
            foreach (var handle in page.MemberHandles)
            {
                if (handle == 0)
                {
                    throw new InvalidDataException(
                        "capture page definition contains a null runtime handle");
                }
            }
        }
        return definitions;
    }

    private static CapturePageRoutes BuildRoutes(
        IReadOnlyList<CapturePageDefinition> definitions)
    {
        var memberCount = 0;
        foreach (var page in definitions)
        {
            memberCount = checked(memberCount + page.MemberHandles.Length);
        }
        var handles = new nuint[memberCount];
        var pageIndexes = new int[memberCount];
        var routeIndex = 0;
        for (var pageIndex = 0; pageIndex < definitions.Count; pageIndex++)
        {
            foreach (var handle in definitions[pageIndex].MemberHandles)
            {
                handles[routeIndex] = handle;
                pageIndexes[routeIndex] = pageIndex;
                routeIndex++;
            }
        }
        Array.Sort(handles, pageIndexes);
        for (var index = 1; index < handles.Length; index++)
        {
            if (handles[index] == handles[index - 1])
            {
                throw new InvalidDataException(
                    "capture page definition repeats a runtime handle");
            }
        }
        return new(handles, pageIndexes);
    }

    private static bool SamePageOrder(
        IReadOnlyList<StoredPage> baseline,
        IReadOnlyList<CapturePageDefinition> current)
    {
        if (baseline.Count != current.Count)
        {
            return false;
        }
        for (var index = 0; index < baseline.Count; index++)
        {
            if (!string.Equals(
                    baseline[index].PageId,
                    current[index].PageId,
                    StringComparison.Ordinal))
            {
                return false;
            }
        }
        return true;
    }

    private sealed record StoredPage(
        string PageId,
        CaptureCachedPage Payload);

    private sealed record Baseline(
        CapturePageTableKey Key,
        long HierarchySequence,
        long ChangeSequence,
        StoredPage[] Pages,
        CapturePageRoutes Routes);

    private sealed record Pending(
        string CaptureId,
        CapturePageTableKey Key,
        long HierarchySequence,
        long ChangeSequence,
        StoredPage[] Pages,
        CapturePageRoutes Routes);

    private sealed record ActivePlan(
        string CaptureId,
        long HierarchySequence,
        long ChangeSequence,
        CaptureCachedPage?[] SerializedPayloads);

    private sealed record PreparedPayload(
        string TemporaryPath,
        string Path,
        long Length,
        byte[] Digest);

    private void DeletePending()
    {
        if (_pending is not null)
        {
            _pending = null;
            PrunePayloads();
        }
    }

    private void DeleteActivePlan()
    {
        if (_activePlan is not null)
        {
            _activePlan = null;
            PrunePayloads();
        }
    }

    private PreparedPayload PreparePayload(byte[] payload)
    {
        var digest = SHA256.HashData(payload);
        var path = System.IO.Path.Combine(
            _storageDirectory,
            $"{Convert.ToHexStringLower(digest)}.rbxm");
        var temporaryPath = $"{path}.{Guid.NewGuid():N}.tmp";
        try
        {
            File.WriteAllBytes(temporaryPath, payload);
            return new(
                temporaryPath,
                path,
                payload.LongLength,
                digest);
        }
        catch
        {
            try
            {
                File.Delete(temporaryPath);
            }
            catch (FileNotFoundException)
            {
            }
            throw;
        }
    }

    private ActivePlan RequireActivePlan(CaptureDirtyPagePlan plan)
    {
        if (_activePlan is not { } active
            || !string.Equals(active.CaptureId, plan.CaptureId, StringComparison.Ordinal)
            || active.HierarchySequence != plan.HierarchySequence
            || active.ChangeSequence != plan.ChangeSequence)
        {
            throw new InvalidOperationException(
                "capture page-table plan is no longer active");
        }
        return active;
    }

    private static void ValidateSerializedPageIndex(
        CaptureDirtyPagePlan plan,
        int pageIndex)
    {
        if (pageIndex < 0
            || pageIndex >= plan.Pages.Length
            || plan.Pages[pageIndex].Disposition is not CapturePageDisposition.Serialize)
        {
            throw new InvalidDataException(
                "capture page-table serialized page index is invalid");
        }
    }

    private void PrunePayloads()
    {
        var retained = new HashSet<string>(StringComparer.Ordinal);
        if (_baseline is { } baseline)
        {
            foreach (var page in baseline.Pages)
            {
                retained.Add(page.Payload.Path);
            }
        }
        if (_pending is { } pending)
        {
            foreach (var page in pending.Pages)
            {
                retained.Add(page.Payload.Path);
            }
        }
        if (_activePlan is { } active)
        {
            foreach (var payload in active.SerializedPayloads)
            {
                if (payload is not null)
                {
                    retained.Add(payload.Path);
                }
            }
        }
        foreach (var path in Directory.EnumerateFiles(_storageDirectory, "*.rbxm"))
        {
            if (!retained.Contains(path))
            {
                File.Delete(path);
            }
        }
    }

    private static void DeleteDirectory(string directory)
    {
        try
        {
            Directory.Delete(directory, recursive: true);
        }
        catch (DirectoryNotFoundException)
        {
        }
    }

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(_disposed, this);
}
