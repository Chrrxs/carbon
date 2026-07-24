using System.Security.Cryptography;

using IOFile = System.IO.File;

namespace Carbon.RmlBridge;

internal sealed record CaptureLeaseRequest(
    string CaptureId,
    string StudioSessionId,
    string InstanceId,
    long EngineGeneration,
    string SourceGeneration,
    string ManagedContractId,
    string ReflectionSchemaHash,
    bool ManifestIdentitiesAuthoritative,
    bool AllowPageReuse,
    string[] MappedRootSourceIds,
    CaptureShellClassRequest[] ShellClasses);

internal sealed record CaptureShellClassRequest(
    string ClassName,
    string[] Properties);

internal delegate Task<CaptureEnvelopeData> CaptureLeaseOperation(
    CaptureLeaseRequest request,
    Action<CaptureLeasePhase> reportPhase,
    CaptureModelArtifactWriter modelWriter,
    CancellationToken cancellationToken);

internal enum CaptureLeasePhase
{
    Preparing,
    Serializing,
    Spooling,
    Ready,
    Cancelling,
    Cancelled,
    Failed,
}

internal sealed record CaptureLeaseStatus(
    string LeaseId,
    string CaptureId,
    string State,
    bool CancelRequested,
    bool SerializerSettled,
    long? ModelBytes,
    long? EnvelopeBytes,
    int? TotalChunks,
    int CompletedChunks,
    long SerializedBytes,
    long CommittedModelBytes,
    string DigestAlgorithm,
    string? ModelDigest,
    DateTimeOffset CreatedAt,
    DateTimeOffset UpdatedAt,
    DateTimeOffset ExpiresAt,
    string? Error);

internal sealed record CaptureLeaseDeleteResult(
    CaptureLeaseStatus Status,
    bool Released);

internal sealed class CaptureLeaseConflictException(string message) : InvalidOperationException(message);

internal sealed class CaptureLeaseManager : IAsyncDisposable
{
    private static readonly TimeSpan DefaultRetention = TimeSpan.FromMinutes(10);
    private readonly object _lock = new();
    private readonly string _spoolDirectory;
    private readonly CaptureLeaseOperation _capture;
    private readonly TimeProvider _timeProvider;
    private readonly TimeSpan _retention;
    private Lease? _lease;
    private bool _disposed;

    public CaptureLeaseManager(
        string spoolDirectory,
        CaptureLeaseOperation capture,
        TimeProvider? timeProvider = null,
        TimeSpan? retention = null)
    {
        _spoolDirectory = spoolDirectory;
        _capture = capture;
        _timeProvider = timeProvider ?? TimeProvider.System;
        _retention = retention ?? DefaultRetention;
        Directory.CreateDirectory(_spoolDirectory);
    }

    public CaptureLeaseStatus Start(CaptureLeaseRequest request)
    {
        ValidateRequest(request);
        Lease lease;
        lock (_lock)
        {
            ThrowIfDisposed();
            SweepExpiredLocked();
            if (_lease is not null)
            {
                throw new CaptureLeaseConflictException(
                    $"capture lease '{_lease.Id}' is still exclusive ({StateName(_lease.Phase)})");
            }
            var now = _timeProvider.GetUtcNow();
            lease = new Lease(
                request.CaptureId.ToLowerInvariant(),
                request,
                IOPath(request.CaptureId, ".rbxm"),
                IOPath(request.CaptureId, ".envelope"),
                now,
                now + _retention);
            _lease = lease;
            lease.Worker = Task.Run(() => RunAsync(lease));
            return StatusLocked(lease);
        }
    }

    internal static void EnsureShellSchemaCoverage(
        IEnumerable<string> capturedShellClasses,
        IEnumerable<string> schemaClasses)
    {
        var captured = capturedShellClasses.ToHashSet(StringComparer.Ordinal);
        var schema = schemaClasses.ToHashSet(StringComparer.Ordinal);
        var missing = captured.Except(schema, StringComparer.Ordinal)
            .Order(StringComparer.Ordinal)
            .ToArray();
        if (missing.Length == 0)
        {
            return;
        }

        var extra = schema.Except(captured, StringComparer.Ordinal)
            .Order(StringComparer.Ordinal)
            .ToArray();
        throw new InvalidOperationException(
            "capture shell property schema does not cover every persistent service shell: "
            + $"missing=[{string.Join(", ", missing)}]; extra=[{string.Join(", ", extra)}]");
    }

    public CaptureLeaseStatus Get(string leaseId)
    {
        lock (_lock)
        {
            SweepExpiredLocked();
            return StatusLocked(FindLocked(leaseId));
        }
    }

    public void EnsureReadyCapture(string captureId)
    {
        lock (_lock)
        {
            SweepExpiredLocked();
            if (_lease is not { } lease
                || lease.Phase is not CaptureLeasePhase.Ready
                || !string.Equals(lease.Request.CaptureId, captureId, StringComparison.OrdinalIgnoreCase))
            {
                throw new InvalidOperationException(
                    "manifest identity finalization does not belong to the active ready capture lease");
            }
        }
    }

    public string EnsureReadyLease(string leaseId)
    {
        lock (_lock)
        {
            SweepExpiredLocked();
            var lease = FindLocked(leaseId);
            if (lease.Phase is not CaptureLeasePhase.Ready)
            {
                throw new InvalidOperationException(
                    "capture page-table acknowledgement requires a ready lease");
            }
            return lease.Request.CaptureId;
        }
    }

    public CaptureLeaseDeleteResult Delete(string leaseId)
    {
        lock (_lock)
        {
            var lease = FindLocked(leaseId);
            if (!IsTerminal(lease.Phase))
            {
                lease.CancelRequested = true;
                lease.ReleaseWhenSettled = true;
                lease.Cancellation.Cancel();
                if (lease.Phase is not CaptureLeasePhase.Cancelling)
                {
                    lease.Phase = CaptureLeasePhase.Cancelling;
                }
                TouchLocked(lease);
                return new(StatusLocked(lease), false);
            }
            var status = StatusLocked(lease);
            ReleaseLocked(lease);
            return new(status, true);
        }
    }

    public void CancelActive()
    {
        lock (_lock)
        {
            if (_lease is not { } lease || IsTerminal(lease.Phase))
            {
                return;
            }
            lease.CancelRequested = true;
            lease.Cancellation.Cancel();
            lease.Phase = CaptureLeasePhase.Cancelling;
            TouchLocked(lease);
        }
    }

    public CaptureLeaseFile OpenFile(string leaseId, bool envelope, string? rangeHeader)
    {
        lock (_lock)
        {
            var lease = FindLocked(leaseId);
            if (envelope && lease.Phase is not CaptureLeasePhase.Ready)
            {
                throw new InvalidOperationException("capture lease envelope is not ready");
            }
            if (!envelope
                && (lease.Phase is not (
                    CaptureLeasePhase.Serializing
                    or CaptureLeasePhase.Spooling
                    or CaptureLeasePhase.Ready)
                    || lease.CommittedModelBytes == 0))
            {
                throw new InvalidOperationException("capture lease has no committed payload frames");
            }
            var path = envelope
                ? lease.EnvelopePath
                : lease.Phase is CaptureLeasePhase.Ready
                    ? lease.ModelPath
                    : lease.ModelPath + ".tmp";
            var length = envelope
                ? lease.EnvelopeBytes!.Value
                : lease.Phase is CaptureLeasePhase.Ready
                    ? lease.ModelBytes!.Value
                    : lease.CommittedModelBytes;
            var range = ParseRange(rangeHeader, length);
            return new CaptureLeaseFile(path, range.Offset, range.Length, length, range.IsPartial);
        }
    }

    public static CaptureByteRange ParseRange(string? rangeHeader, long length)
    {
        if (length < 0)
        {
            throw new ArgumentOutOfRangeException(nameof(length));
        }
        if (string.IsNullOrWhiteSpace(rangeHeader))
        {
            return new(0, length, false);
        }
        if (!rangeHeader.StartsWith("bytes=", StringComparison.OrdinalIgnoreCase)
            || rangeHeader.AsSpan("bytes=".Length).Contains(','))
        {
            throw new InvalidDataException("capture payload supports one byte range");
        }
        var value = rangeHeader["bytes=".Length..];
        var separator = value.IndexOf('-');
        if (separator < 0)
        {
            throw new InvalidDataException("capture payload byte range is malformed");
        }
        var startText = value[..separator];
        var endText = value[(separator + 1)..];
        long start;
        long end;
        if (startText.Length == 0)
        {
            if (!long.TryParse(endText, out var suffix) || suffix <= 0 || length == 0)
            {
                throw new InvalidDataException("capture payload suffix range is invalid");
            }
            start = Math.Max(0, length - suffix);
            end = length - 1;
        }
        else
        {
            if (!long.TryParse(startText, out start) || start < 0 || start >= length)
            {
                throw new InvalidDataException("capture payload range start is invalid");
            }
            if (endText.Length == 0)
            {
                end = length - 1;
            }
            else if (!long.TryParse(endText, out end) || end < start)
            {
                throw new InvalidDataException("capture payload range end is invalid");
            }
            end = Math.Min(end, length - 1);
        }
        return new(start, checked(end - start + 1), true);
    }

    public async ValueTask DisposeAsync()
    {
        Task? worker;
        lock (_lock)
        {
            if (_disposed)
            {
                return;
            }
            _disposed = true;
            worker = _lease?.Worker;
            if (_lease is { } lease && !IsTerminal(lease.Phase))
            {
                lease.CancelRequested = true;
                lease.ReleaseWhenSettled = true;
                lease.Cancellation.Cancel();
                lease.Phase = CaptureLeasePhase.Cancelling;
            }
        }
        if (worker is not null)
        {
            await worker.ConfigureAwait(false);
        }
        lock (_lock)
        {
            if (_lease is { } lease)
            {
                ReleaseLocked(lease);
            }
        }
    }

    private async Task RunAsync(Lease lease)
    {
        var modelTemporary = lease.ModelPath + ".tmp";
        var envelopeTemporary = lease.EnvelopePath + ".tmp";
        try
        {
            byte[] modelDigest;
            long modelLength;
            long envelopeLength;
            CaptureEnvelopeData envelope;
            using (var hash = SHA256.Create())
            await using (var modelOutput = new FileStream(
                modelTemporary,
                FileMode.Create,
                FileAccess.Write,
                FileShare.Read,
                bufferSize: 1024 * 1024,
                FileOptions.SequentialScan))
            using (var hashingOutput = new CryptoStream(
                modelOutput,
                hash,
                CryptoStreamMode.Write,
                leaveOpen: true))
            {
                var modelWriter = new CaptureModelArtifactWriter(
                    hashingOutput,
                    (total, completed, bytes, committedBytes) =>
                    {
                        modelOutput.Flush();
                        SetProgress(
                            lease,
                            total,
                            completed,
                            bytes,
                            committedBytes);
                    });
                envelope = await _capture(
                    lease.Request,
                    phase => SetPhase(lease, phase),
                    modelWriter,
                    lease.Cancellation.Token).ConfigureAwait(false);
                lock (_lock)
                {
                    lease.SerializerSettled = true;
                    TouchLocked(lease);
                }
                lease.Cancellation.Token.ThrowIfCancellationRequested();
                SetPhase(lease, CaptureLeasePhase.Spooling);
                modelWriter.Complete();
                hashingOutput.FlushFinalBlock();
                await modelOutput.FlushAsync(lease.Cancellation.Token).ConfigureAwait(false);
                modelLength = modelOutput.Length;
                modelDigest = hash.Hash
                    ?? throw new InvalidOperationException("capture model SHA-256 digest is unavailable");
            }
            await using (var envelopeOutput = new FileStream(
                envelopeTemporary,
                FileMode.Create,
                FileAccess.Write,
                FileShare.None,
                bufferSize: 1024 * 1024,
                FileOptions.SequentialScan))
            {
                CaptureEnvelope.Write(
                    envelopeOutput,
                    envelope,
                    modelLength,
                    modelDigest);
                await envelopeOutput.FlushAsync(lease.Cancellation.Token).ConfigureAwait(false);
                envelopeLength = envelopeOutput.Length;
            }
            IOFile.Move(modelTemporary, lease.ModelPath, true);
            IOFile.Move(envelopeTemporary, lease.EnvelopePath, true);
            lease.Cancellation.Token.ThrowIfCancellationRequested();
            lock (_lock)
            {
                lease.SerializerSettled = true;
                lease.ModelBytes = modelLength;
                lease.CommittedModelBytes = modelLength;
                lease.EnvelopeBytes = envelopeLength;
                lease.ModelDigest = Convert.ToHexStringLower(modelDigest);
                lease.Phase = CaptureLeasePhase.Ready;
                TouchLocked(lease);
            }
        }
        catch (OperationCanceledException) when (lease.Cancellation.IsCancellationRequested)
        {
            lock (_lock)
            {
                lease.SerializerSettled = true;
                lease.Phase = CaptureLeasePhase.Cancelled;
                TouchLocked(lease);
            }
        }
        catch (Exception ex)
        {
            lock (_lock)
            {
                lease.SerializerSettled = true;
                lease.Phase = CaptureLeasePhase.Failed;
                lease.Error = ex.Message;
                TouchLocked(lease);
            }
        }
        finally
        {
            DeleteIfExists(modelTemporary);
            DeleteIfExists(envelopeTemporary);
            lock (_lock)
            {
                if (lease.Phase is not CaptureLeasePhase.Ready)
                {
                    DeleteLeaseFiles(lease);
                }
                if (lease.ReleaseWhenSettled)
                {
                    ReleaseLocked(lease);
                }
            }
        }
    }

    private void SetPhase(Lease lease, CaptureLeasePhase phase)
    {
        lock (_lock)
        {
            if (!ReferenceEquals(_lease, lease) || IsTerminal(lease.Phase))
            {
                return;
            }
            lease.Phase = lease.CancelRequested ? CaptureLeasePhase.Cancelling : phase;
            TouchLocked(lease);
        }
    }

    private void SetProgress(
        Lease lease,
        int totalChunks,
        int completedChunks,
        long serializedBytes,
        long committedModelBytes)
    {
        lock (_lock)
        {
            if (!ReferenceEquals(_lease, lease) || IsTerminal(lease.Phase))
            {
                return;
            }
            lease.TotalChunks = totalChunks;
            lease.CompletedChunks = completedChunks;
            lease.SerializedBytes = serializedBytes;
            lease.CommittedModelBytes = committedModelBytes;
            TouchLocked(lease);
        }
    }

    private void SweepExpiredLocked()
    {
        if (_lease is { } lease
            && IsTerminal(lease.Phase)
            && _timeProvider.GetUtcNow() >= lease.ExpiresAt)
        {
            ReleaseLocked(lease);
        }
    }

    private Lease FindLocked(string leaseId)
    {
        if (_lease is not { } lease || !string.Equals(lease.Id, leaseId, StringComparison.Ordinal))
        {
            throw new KeyNotFoundException($"capture lease '{leaseId}' is unavailable");
        }
        return lease;
    }

    private CaptureLeaseStatus StatusLocked(Lease lease) => new(
        lease.Id,
        lease.Request.CaptureId,
        StateName(lease.Phase),
        lease.CancelRequested,
        lease.SerializerSettled,
        lease.ModelBytes,
        lease.EnvelopeBytes,
        lease.TotalChunks,
        lease.CompletedChunks,
        lease.SerializedBytes,
        lease.CommittedModelBytes,
        CaptureEnvelope.DigestAlgorithm,
        lease.ModelDigest,
        lease.CreatedAt,
        lease.UpdatedAt,
        lease.ExpiresAt,
        lease.Error);

    private void ReleaseLocked(Lease lease)
    {
        DeleteLeaseFiles(lease);
        lease.Cancellation.Dispose();
        if (ReferenceEquals(_lease, lease))
        {
            _lease = null;
        }
    }

    private static void DeleteLeaseFiles(Lease lease)
    {
        DeleteIfExists(lease.ModelPath);
        DeleteIfExists(lease.EnvelopePath);
        DeleteIfExists(lease.ModelPath + ".tmp");
        DeleteIfExists(lease.EnvelopePath + ".tmp");
    }

    private static void DeleteIfExists(string path)
    {
        try
        {
            IOFile.Delete(path);
        }
        catch (DirectoryNotFoundException)
        {
        }
    }

    private string IOPath(string captureId, string extension) =>
        Path.Combine(_spoolDirectory, captureId.ToLowerInvariant() + extension);

    private void TouchLocked(Lease lease)
    {
        lease.UpdatedAt = _timeProvider.GetUtcNow();
        lease.ExpiresAt = lease.UpdatedAt + _retention;
    }

    private static bool IsTerminal(CaptureLeasePhase phase) =>
        phase is CaptureLeasePhase.Ready or CaptureLeasePhase.Cancelled or CaptureLeasePhase.Failed;

    private static string StateName(CaptureLeasePhase phase) => phase switch
    {
        CaptureLeasePhase.Preparing => "preparing",
        CaptureLeasePhase.Serializing => "serializing",
        CaptureLeasePhase.Spooling => "spooling",
        CaptureLeasePhase.Ready => "ready",
        CaptureLeasePhase.Cancelling => "cancelling",
        CaptureLeasePhase.Cancelled => "cancelled",
        CaptureLeasePhase.Failed => "failed",
        _ => throw new ArgumentOutOfRangeException(nameof(phase)),
    };

    private static void ValidateRequest(CaptureLeaseRequest request)
    {
        if (request.CaptureId.Length != 32
            || request.CaptureId.Any(character => !Uri.IsHexDigit(character)))
        {
            throw new InvalidDataException("capture identity must be 128-bit hexadecimal");
        }
        if (request.StudioSessionId.Length == 0 || request.InstanceId.Length == 0)
        {
            throw new InvalidDataException("capture Studio route is incomplete");
        }
        if (request.SourceGeneration.Length == 0 || request.SourceGeneration.Length > 256)
        {
            throw new InvalidDataException("capture source generation is invalid");
        }
        if (request.ManagedContractId.Length is not (0 or 32)
            || request.ManagedContractId.Any(character => !Uri.IsHexDigit(character)))
        {
            throw new InvalidDataException("capture managed contract identity is invalid");
        }
        if (request.ReflectionSchemaHash.Length > 128)
        {
            throw new InvalidDataException("capture reflection schema identity is too long");
        }
        if (request.MappedRootSourceIds is null || request.MappedRootSourceIds.Length > 4096)
        {
            throw new InvalidDataException("capture mapped root identity list is missing or too large");
        }
        if (request.MappedRootSourceIds.Any(sourceId =>
            sourceId.Length != 32 || sourceId.Any(character => !Uri.IsHexDigit(character))))
        {
            throw new InvalidDataException("capture mapped root identity is not 128-bit hexadecimal");
        }
        if (request.MappedRootSourceIds.Distinct(StringComparer.Ordinal).Count()
            != request.MappedRootSourceIds.Length)
        {
            throw new InvalidDataException("capture mapped root identity list contains a duplicate");
        }
        if (request.ShellClasses is null
            || request.ShellClasses.Length == 0
            || request.ShellClasses.Length > 4096)
        {
            throw new InvalidDataException("capture shell property schema is missing or too large");
        }
        var classes = new HashSet<string>(StringComparer.Ordinal);
        foreach (var shell in request.ShellClasses)
        {
            if (string.IsNullOrEmpty(shell.ClassName))
            {
                throw new InvalidDataException("capture shell property schema contains an empty class name");
            }
            if (!classes.Add(shell.ClassName))
            {
                throw new InvalidDataException(
                    $"capture shell property schema repeats class '{shell.ClassName}'");
            }
            if (shell.Properties is null)
            {
                throw new InvalidDataException(
                    $"capture shell property schema has no property list for '{shell.ClassName}'");
            }
            if (shell.Properties.Length > 4096)
            {
                throw new InvalidDataException(
                    $"capture shell property schema for '{shell.ClassName}' exceeds 4096 properties");
            }
            if (shell.Properties.Any(string.IsNullOrEmpty))
            {
                throw new InvalidDataException(
                    $"capture shell property schema for '{shell.ClassName}' contains an empty property name");
            }
            var duplicate = shell.Properties
                .GroupBy(property => property, StringComparer.Ordinal)
                .FirstOrDefault(group => group.Count() > 1)?.Key;
            if (duplicate is not null)
            {
                throw new InvalidDataException(
                    $"capture shell property schema for '{shell.ClassName}' repeats property '{duplicate}'");
            }
        }
    }

    private void ThrowIfDisposed() => ObjectDisposedException.ThrowIf(_disposed, this);

    private sealed class Lease(
        string id,
        CaptureLeaseRequest request,
        string modelPath,
        string envelopePath,
        DateTimeOffset createdAt,
        DateTimeOffset expiresAt)
    {
        public string Id { get; } = id;
        public CaptureLeaseRequest Request { get; } = request;
        public string ModelPath { get; } = modelPath;
        public string EnvelopePath { get; } = envelopePath;
        public DateTimeOffset CreatedAt { get; } = createdAt;
        public DateTimeOffset UpdatedAt { get; set; } = createdAt;
        public DateTimeOffset ExpiresAt { get; set; } = expiresAt;
        public CancellationTokenSource Cancellation { get; } = new();
        public CaptureLeasePhase Phase { get; set; } = CaptureLeasePhase.Preparing;
        public bool CancelRequested { get; set; }
        public bool SerializerSettled { get; set; }
        public bool ReleaseWhenSettled { get; set; }
        public long? ModelBytes { get; set; }
        public long? EnvelopeBytes { get; set; }
        public int? TotalChunks { get; set; }
        public int CompletedChunks { get; set; }
        public long SerializedBytes { get; set; }
        public long CommittedModelBytes { get; set; }
        public string? ModelDigest { get; set; }
        public string? Error { get; set; }
        public Task? Worker { get; set; }
    }
}

internal readonly record struct CaptureByteRange(long Offset, long Length, bool IsPartial);

internal sealed record CaptureLeaseFile(
    string Path,
    long Offset,
    long Length,
    long TotalLength,
    bool IsPartial);
