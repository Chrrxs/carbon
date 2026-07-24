using System.Text;
using System.Security.Cryptography;

namespace Carbon.RmlBridge;

internal readonly record struct CaptureModelChunk(
    uint[] RootOrdinals,
    byte[] Payload);

internal static class CaptureModelArtifact
{
    internal static ReadOnlySpan<byte> Magic => "CARBONCM2"u8;
    // Persistent ordinals are below the protocol's 20M-node ceiling. The high
    // bit tags an isolated serializer-only root while preserving its target.
    internal const uint ReferenceDependencyFlag = 1U << 31;

    internal static uint EncodeReferenceDependency(uint targetOrdinal)
    {
        if (targetOrdinal == 0 || targetOrdinal >= ReferenceDependencyFlag)
        {
            throw new ArgumentOutOfRangeException(nameof(targetOrdinal));
        }
        return ReferenceDependencyFlag | targetOrdinal;
    }

    internal static byte[] Encode(IReadOnlyList<CaptureModelChunk> chunks)
    {
        using var output = new MemoryStream(CheckedLength(chunks));
        Write(output, chunks);
        return output.ToArray();
    }

    internal static void Write(Stream output, IReadOnlyList<CaptureModelChunk> chunks)
    {
        ArgumentNullException.ThrowIfNull(output);
        if (!output.CanWrite)
        {
            throw new ArgumentException("capture model output is not writable", nameof(output));
        }
        _ = CheckedLength(chunks);
        var writer = new CaptureModelArtifactWriter(output);
        writer.Begin(chunks.Count);
        foreach (var chunk in chunks)
        {
            writer.WriteChunk(chunk.RootOrdinals, chunk.Payload);
        }
        writer.Complete();
    }

    private static int CheckedLength(IReadOnlyList<CaptureModelChunk> chunks)
    {
        ArgumentNullException.ThrowIfNull(chunks);
        var length = Magic.Length + sizeof(uint);
        foreach (var chunk in chunks)
        {
            if (chunk.RootOrdinals is null || chunk.Payload is null
                || chunk.RootOrdinals.Length == 0 || chunk.Payload.Length == 0)
            {
                throw new InvalidDataException(
                    "capture model chunks require roots and a serializer payload");
            }
            length = checked(
                length
                + sizeof(uint)
                + chunk.RootOrdinals.Length * sizeof(uint)
                + sizeof(ulong)
                + chunk.Payload.Length);
        }
        return length;
    }
}

internal sealed class CaptureModelArtifactWriter
{
    private readonly BinaryWriter _writer;
    private readonly Action<int, int, long, long>? _reportProgress;
    private int _expectedChunks = -1;
    private int _writtenChunks;
    private long _writtenPayloadBytes;
    private long _writtenArtifactBytes;
    private bool _complete;

    internal CaptureModelArtifactWriter(
        Stream output,
        Action<int, int, long, long>? reportProgress = null)
    {
        ArgumentNullException.ThrowIfNull(output);
        if (!output.CanWrite)
        {
            throw new ArgumentException("capture model output is not writable", nameof(output));
        }
        _writer = new BinaryWriter(output, Encoding.UTF8, leaveOpen: true);
        _reportProgress = reportProgress;
    }

    internal int ExpectedChunks => _expectedChunks;

    internal int WrittenChunks => _writtenChunks;

    internal long WrittenPayloadBytes => _writtenPayloadBytes;

    internal long WrittenArtifactBytes => _writtenArtifactBytes;

    internal void Begin(int chunkCount)
    {
        if (_expectedChunks >= 0)
        {
            throw new InvalidOperationException("capture model artifact was already started");
        }
        if (chunkCount < 0)
        {
            throw new ArgumentOutOfRangeException(nameof(chunkCount));
        }
        _writer.Write(CaptureModelArtifact.Magic);
        _writer.Write(checked((uint)chunkCount));
        _expectedChunks = chunkCount;
        _writtenArtifactBytes = CaptureModelArtifact.Magic.Length + sizeof(uint);
        _writer.Flush();
        _reportProgress?.Invoke(
            _expectedChunks,
            _writtenChunks,
            _writtenPayloadBytes,
            _writtenArtifactBytes);
    }

    internal void WriteChunk(uint[] rootOrdinals, byte[] payload)
    {
        ArgumentNullException.ThrowIfNull(rootOrdinals);
        ArgumentNullException.ThrowIfNull(payload);
        if (_expectedChunks < 0 || _complete)
        {
            throw new InvalidOperationException("capture model artifact is not writable");
        }
        if (_writtenChunks >= _expectedChunks)
        {
            throw new InvalidDataException("capture model artifact received too many chunks");
        }
        if (rootOrdinals.Length == 0 || payload.Length == 0)
        {
            throw new InvalidDataException(
                "capture model chunks require roots and a serializer payload");
        }
        _writer.Write(checked((uint)rootOrdinals.Length));
        foreach (var ordinal in rootOrdinals)
        {
            _writer.Write(ordinal);
        }
        _writer.Write(checked((ulong)payload.LongLength));
        _writer.Write(payload);
        _writtenChunks++;
        _writtenPayloadBytes = checked(_writtenPayloadBytes + payload.LongLength);
        _writtenArtifactBytes = checked(
            _writtenArtifactBytes
            + sizeof(uint)
            + rootOrdinals.LongLength * sizeof(uint)
            + sizeof(ulong)
            + payload.LongLength);
        // Publish only complete frames. Flushes move managed buffering into the
        // OS cache; durability remains the final seal's responsibility.
        _writer.Flush();
        _reportProgress?.Invoke(
            _expectedChunks,
            _writtenChunks,
            _writtenPayloadBytes,
            _writtenArtifactBytes);
    }

    internal void WriteChunk(
        uint[] rootOrdinals,
        Stream payload,
        long payloadLength,
        byte[] expectedDigest)
    {
        ArgumentNullException.ThrowIfNull(rootOrdinals);
        ArgumentNullException.ThrowIfNull(payload);
        ArgumentNullException.ThrowIfNull(expectedDigest);
        if (_expectedChunks < 0 || _complete)
        {
            throw new InvalidOperationException("capture model artifact is not writable");
        }
        if (_writtenChunks >= _expectedChunks)
        {
            throw new InvalidDataException("capture model artifact received too many chunks");
        }
        if (!payload.CanRead
            || rootOrdinals.Length == 0
            || payloadLength <= 0
            || expectedDigest.Length != SHA256.HashSizeInBytes)
        {
            throw new InvalidDataException(
                "cached capture model chunks require roots, length, digest, and a readable payload");
        }

        _writer.Write(checked((uint)rootOrdinals.Length));
        foreach (var ordinal in rootOrdinals)
        {
            _writer.Write(ordinal);
        }
        _writer.Write(checked((ulong)payloadLength));
        using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
        var buffer = new byte[1024 * 1024];
        var remaining = payloadLength;
        while (remaining != 0)
        {
            var read = payload.Read(
                buffer,
                0,
                checked((int)Math.Min(buffer.LongLength, remaining)));
            if (read == 0)
            {
                throw new EndOfStreamException(
                    "cached capture page ended before its attested length");
            }
            hash.AppendData(buffer.AsSpan(0, read));
            _writer.Write(buffer, 0, read);
            remaining -= read;
        }
        if (payload.ReadByte() != -1)
        {
            throw new InvalidDataException(
                "cached capture page exceeds its attested length");
        }
        if (!CryptographicOperations.FixedTimeEquals(
                hash.GetHashAndReset(),
                expectedDigest))
        {
            throw new InvalidDataException(
                "cached capture page failed its SHA-256 integrity check");
        }

        _writtenChunks++;
        _writtenPayloadBytes = checked(_writtenPayloadBytes + payloadLength);
        _writtenArtifactBytes = checked(
            _writtenArtifactBytes
            + sizeof(uint)
            + rootOrdinals.LongLength * sizeof(uint)
            + sizeof(ulong)
            + payloadLength);
        _writer.Flush();
        _reportProgress?.Invoke(
            _expectedChunks,
            _writtenChunks,
            _writtenPayloadBytes,
            _writtenArtifactBytes);
    }

    internal void Complete()
    {
        if (_expectedChunks < 0 || _complete)
        {
            throw new InvalidOperationException("capture model artifact is not completable");
        }
        if (_writtenChunks != _expectedChunks)
        {
            throw new InvalidDataException(
                $"capture model artifact received {_writtenChunks} of {_expectedChunks} chunks");
        }
        _writer.Flush();
        _complete = true;
    }
}
