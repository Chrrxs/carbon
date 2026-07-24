using System.Buffers;
using System.Security.Cryptography;
using System.Text;

namespace Carbon.RmlBridge;

internal readonly record struct CaptureHierarchyNode(
    uint ParentOrdinal,
    string ClassName,
    string Name,
    uint Flags = 0);

internal readonly record struct CaptureServiceRoot(
    uint HierarchyOrdinal,
    string ClassName,
    string Name,
    uint FirstSerializedRoot,
    uint SerializedRootCount);

internal readonly record struct CaptureMappedBinding(
    string SourceId,
    uint HierarchyOrdinal,
    uint ParentOrdinal);

internal readonly record struct CaptureExternalReference(
    uint OwnerOrdinal,
    string Property,
    uint TargetOrdinal,
    string? MappedSourceId = null);

internal readonly record struct CaptureShellProperty(
    uint OwnerOrdinal,
    string Property,
    string TypeName,
    byte[] Value);

internal readonly record struct CaptureShellCarrier(
    uint OwnerOrdinal,
    string Property,
    string TypeName,
    string CarrierClass,
    uint SerializedRootIndex);

internal sealed record CaptureEnvelopeData(
    string CaptureId,
    long EngineGeneration,
    string SourceGeneration,
    long HierarchySequenceBefore,
    long HierarchySequenceAfter,
    long ChangeSequenceBefore,
    long ChangeSequenceAfter,
    string StudioSessionId,
    string InstanceId,
    string ManagedContractId,
    string ReflectionSchemaHash,
    IReadOnlyList<CaptureHierarchyNode> Nodes,
    IReadOnlyList<CaptureServiceRoot> Roots,
    IReadOnlyList<CaptureMappedBinding> MappedBindings,
    IReadOnlyList<CaptureExternalReference> ExternalReferences,
    IReadOnlyList<CaptureShellProperty> ShellProperties,
    IReadOnlyList<CaptureShellCarrier> ShellCarriers,
    IReadOnlyList<uint> SerializedRootOrdinals,
    bool ManifestIdentitiesAuthoritative,
    IReadOnlyList<ManifestIdentity> ManifestIdentities);

internal static class CaptureEnvelope
{
    internal static ReadOnlySpan<byte> Magic => "CARBONCP4"u8;
    internal const ushort Version = 10;
    internal const ushort AuthoritativeIdentitiesFlag = 1;
    internal const uint NoParent = uint.MaxValue;
    internal const uint NullReference = uint.MaxValue;
    internal const uint MappedReference = uint.MaxValue - 1;
    internal const uint SyntheticNode = uint.MaxValue;
    internal const string DigestAlgorithm = "sha256";

    public static void Write(
        Stream output,
        CaptureEnvelopeData envelope,
        long modelLength,
        ReadOnlySpan<byte> modelDigest)
    {
        ArgumentNullException.ThrowIfNull(output);
        ArgumentNullException.ThrowIfNull(envelope);
        if (!output.CanWrite)
        {
            throw new ArgumentException("capture envelope output is not writable", nameof(output));
        }
        if (modelLength < 0)
        {
            throw new ArgumentOutOfRangeException(nameof(modelLength));
        }
        if (modelDigest.Length != SHA256.HashSizeInBytes)
        {
            throw new ArgumentException("capture model digest must be SHA-256", nameof(modelDigest));
        }
        Validate(envelope);

        var strings = new List<string>();
        var stringIndexes = new Dictionary<string, uint>(StringComparer.Ordinal);
        uint Intern(string value)
        {
            if (stringIndexes.TryGetValue(value, out var existing))
            {
                return existing;
            }
            var index = checked((uint)strings.Count);
            strings.Add(value);
            stringIndexes.Add(value, index);
            return index;
        }

        var studioSessionIndex = Intern(envelope.StudioSessionId);
        var instanceIdIndex = Intern(envelope.InstanceId);
        var managedContractIndex = Intern(envelope.ManagedContractId);
        var reflectionSchemaIndex = Intern(envelope.ReflectionSchemaHash);
        var sourceGenerationIndex = Intern(envelope.SourceGeneration);
        var digestAlgorithmIndex = Intern(DigestAlgorithm);
        foreach (var node in envelope.Nodes)
        {
            _ = Intern(node.ClassName);
        }
        foreach (var root in envelope.Roots)
        {
            _ = Intern(root.ClassName);
            _ = Intern(root.Name);
        }
        foreach (var reference in envelope.ExternalReferences)
        {
            _ = Intern(reference.Property);
        }
        foreach (var property in envelope.ShellProperties)
        {
            _ = Intern(property.Property);
            _ = Intern(property.TypeName);
        }
        foreach (var carrier in envelope.ShellCarriers)
        {
            _ = Intern(carrier.Property);
            _ = Intern(carrier.TypeName);
            _ = Intern(carrier.CarrierClass);
        }

        using var writer = new BinaryWriter(output, Encoding.UTF8, leaveOpen: true);
        writer.Write(Magic);
        writer.Write(Version);
        writer.Write(envelope.ManifestIdentitiesAuthoritative
            ? AuthoritativeIdentitiesFlag
            : (ushort)0);
        writer.Write(ParseIdentity(envelope.CaptureId, "capture"));
        writer.Write(envelope.EngineGeneration);
        writer.Write(envelope.HierarchySequenceBefore);
        writer.Write(envelope.HierarchySequenceAfter);
        writer.Write(envelope.ChangeSequenceBefore);
        writer.Write(envelope.ChangeSequenceAfter);
        writer.Write(checked((ulong)modelLength));
        writer.Write(modelDigest);
        writer.Write(checked((uint)strings.Count));
        writer.Write(checked((uint)envelope.Nodes.Count));
        writer.Write(checked((uint)envelope.Roots.Count));
        writer.Write(checked((uint)envelope.MappedBindings.Count));
        writer.Write(checked((uint)envelope.ExternalReferences.Count));
        writer.Write(checked((uint)envelope.ShellProperties.Count));
        writer.Write(checked((uint)envelope.ShellCarriers.Count));
        writer.Write(checked((uint)envelope.SerializedRootOrdinals.Count));
        writer.Write(studioSessionIndex);
        writer.Write(instanceIdIndex);
        writer.Write(managedContractIndex);
        writer.Write(reflectionSchemaIndex);
        writer.Write(sourceGenerationIndex);
        writer.Write(digestAlgorithmIndex);

        foreach (var value in strings)
        {
            WriteUtf8(writer, value);
        }
        foreach (var node in envelope.Nodes)
        {
            writer.Write(node.ParentOrdinal);
            writer.Write(stringIndexes[node.ClassName]);
            WriteUtf8(writer, node.Name);
            writer.Write(node.Flags);
        }
        Span<byte> manifestIdentityBytes = stackalloc byte[16];
        foreach (var sourceId in envelope.ManifestIdentities)
        {
            sourceId.Write(manifestIdentityBytes);
            writer.Write(manifestIdentityBytes);
        }
        foreach (var root in envelope.Roots)
        {
            writer.Write(root.HierarchyOrdinal);
            writer.Write(stringIndexes[root.ClassName]);
            writer.Write(stringIndexes[root.Name]);
            writer.Write(root.FirstSerializedRoot);
            writer.Write(root.SerializedRootCount);
        }
        foreach (var binding in envelope.MappedBindings)
        {
            writer.Write(ParseSourceId(binding.SourceId));
            writer.Write(binding.HierarchyOrdinal);
            writer.Write(binding.ParentOrdinal);
        }
        foreach (var reference in envelope.ExternalReferences)
        {
            writer.Write(reference.OwnerOrdinal);
            writer.Write(stringIndexes[reference.Property]);
            writer.Write(reference.TargetOrdinal);
            if (reference.TargetOrdinal == MappedReference)
            {
                writer.Write(ParseSourceId(reference.MappedSourceId!));
            }
        }
        foreach (var property in envelope.ShellProperties)
        {
            writer.Write(property.OwnerOrdinal);
            writer.Write(stringIndexes[property.Property]);
            writer.Write(stringIndexes[property.TypeName]);
            writer.Write(checked((uint)property.Value.Length));
            writer.Write(property.Value);
        }
        foreach (var carrier in envelope.ShellCarriers)
        {
            writer.Write(carrier.OwnerOrdinal);
            writer.Write(stringIndexes[carrier.Property]);
            writer.Write(stringIndexes[carrier.TypeName]);
            writer.Write(stringIndexes[carrier.CarrierClass]);
            writer.Write(carrier.SerializedRootIndex);
        }
        foreach (var ordinal in envelope.SerializedRootOrdinals)
        {
            writer.Write(ordinal);
        }
        writer.Flush();
    }

    private static void WriteUtf8(BinaryWriter writer, string value)
    {
        var length = Encoding.UTF8.GetByteCount(value);
        writer.Write(checked((uint)length));
        if (length == 0)
        {
            return;
        }
        if (length <= 256)
        {
            Span<byte> local = stackalloc byte[length];
            var written = Encoding.UTF8.GetBytes(value, local);
            writer.Write(local[..written]);
            return;
        }
        var buffer = ArrayPool<byte>.Shared.Rent(length);
        try
        {
            var written = Encoding.UTF8.GetBytes(value, buffer);
            writer.Write(buffer.AsSpan(0, written));
        }
        finally
        {
            ArrayPool<byte>.Shared.Return(buffer);
        }
    }

    private static void Validate(CaptureEnvelopeData envelope)
    {
        if (envelope.Nodes.Count == 0 || envelope.Nodes.Count > 20_000_000)
        {
            throw new InvalidDataException("capture hierarchy count is invalid");
        }
        if (envelope.Nodes[0].ParentOrdinal != NoParent)
        {
            throw new InvalidDataException("capture hierarchy root unexpectedly has a parent");
        }
        if (envelope.ManifestIdentities.Count != envelope.Nodes.Count)
        {
            throw new InvalidDataException("capture manifest identity count disagrees with the hierarchy");
        }
        var manifestIdentities = new HashSet<ManifestIdentity>();
        foreach (var sourceId in envelope.ManifestIdentities)
        {
            if (!manifestIdentities.Add(sourceId))
            {
                throw new InvalidDataException("capture manifest identity is duplicated");
            }
        }
        for (var index = 1; index < envelope.Nodes.Count; index++)
        {
            if (envelope.Nodes[index].ParentOrdinal >= index)
            {
                throw new InvalidDataException("capture hierarchy parent does not precede its child");
            }
        }
        var persistentDirectRootTotal = envelope.Roots.Aggregate(
            0UL,
            (total, root) => total + root.SerializedRootCount);
        foreach (var root in envelope.Roots)
        {
            if (root.HierarchyOrdinal == 0 || root.HierarchyOrdinal >= envelope.Nodes.Count)
            {
                throw new InvalidDataException("capture service root ordinal is invalid");
            }
            var node = envelope.Nodes[checked((int)root.HierarchyOrdinal)];
            if (!string.Equals(node.ClassName, root.ClassName, StringComparison.Ordinal)
                || !string.Equals(node.Name, root.Name, StringComparison.Ordinal))
            {
                throw new InvalidDataException("capture service shell identity disagrees with its hierarchy node");
            }
            if ((ulong)root.FirstSerializedRoot + root.SerializedRootCount
                > persistentDirectRootTotal)
            {
                throw new InvalidDataException("capture serialized root range is invalid");
            }
        }
        foreach (var binding in envelope.MappedBindings)
        {
            _ = ParseSourceId(binding.SourceId);
            if (binding.HierarchyOrdinal != SyntheticNode
                || binding.ParentOrdinal >= envelope.Nodes.Count)
            {
                throw new InvalidDataException("capture mapped binding graft anchor is invalid");
            }
        }
        foreach (var reference in envelope.ExternalReferences)
        {
            if (reference.OwnerOrdinal >= envelope.Nodes.Count
                || (reference.TargetOrdinal != NullReference
                    && reference.TargetOrdinal != MappedReference
                    && reference.TargetOrdinal >= envelope.Nodes.Count)
                || (reference.TargetOrdinal == MappedReference) != (reference.MappedSourceId is not null)
                || reference.Property.Length == 0)
            {
                throw new InvalidDataException("capture external reference is invalid");
            }
            if (reference.MappedSourceId is not null)
            {
                _ = ParseSourceId(reference.MappedSourceId);
            }
        }
        foreach (var property in envelope.ShellProperties)
        {
            if (property.OwnerOrdinal >= envelope.Nodes.Count
                || property.Property.Length == 0
                || property.TypeName.Length == 0)
            {
                throw new InvalidDataException("capture shell property is invalid");
            }
        }
        var carrierRootIndexes = envelope.ShellCarriers
            .Select(carrier => carrier.SerializedRootIndex)
            .Distinct()
            .Order()
            .ToArray();
        var persistentComponentRootCount = envelope.SerializedRootOrdinals
            .TakeWhile(ordinal => ordinal != SyntheticNode)
            .Count();
        if ((ulong)persistentComponentRootCount < persistentDirectRootTotal
            || envelope.SerializedRootOrdinals
                .Skip(persistentComponentRootCount)
                .Any(ordinal => ordinal != SyntheticNode))
        {
            throw new InvalidDataException(
                "capture persistent component roots and carriers are not dense ranges");
        }
        for (var index = 0; index < carrierRootIndexes.Length; index++)
        {
            if (carrierRootIndexes[index]
                != checked((uint)persistentComponentRootCount) + (uint)index)
            {
                throw new InvalidDataException("capture shell carrier roots are not a dense suffix");
            }
        }
        foreach (var carrier in envelope.ShellCarriers)
        {
            if (carrier.OwnerOrdinal >= envelope.Nodes.Count
                || carrier.Property.Length == 0
                || carrier.TypeName.Length == 0
                || carrier.CarrierClass.Length == 0)
            {
                throw new InvalidDataException("capture shell carrier is invalid");
            }
            if (carrier.SerializedRootIndex >= envelope.SerializedRootOrdinals.Count
                || envelope.SerializedRootOrdinals[checked((int)carrier.SerializedRootIndex)] != SyntheticNode)
            {
                throw new InvalidDataException("capture shell carrier does not identify a synthetic serializer root");
            }
        }
        if (envelope.SerializedRootOrdinals.Count
            != persistentComponentRootCount + carrierRootIndexes.Length)
        {
            throw new InvalidDataException("capture serialized root ordinal count is invalid");
        }
        var persistentOrdinals = new HashSet<uint>();
        for (var index = 0; index < envelope.SerializedRootOrdinals.Count; index++)
        {
            var ordinal = envelope.SerializedRootOrdinals[index];
            var synthetic = index >= persistentComponentRootCount;
            if ((synthetic && ordinal != SyntheticNode)
                || (!synthetic && (ordinal == 0 || ordinal >= envelope.Nodes.Count
                    || !persistentOrdinals.Add(ordinal))))
            {
                throw new InvalidDataException("capture serialized ordinal is invalid");
            }
        }
        foreach (var root in envelope.Roots)
        {
            var start = checked((int)root.FirstSerializedRoot);
            var end = checked(start + (int)root.SerializedRootCount);
            for (var index = start; index < end; index++)
            {
                var ordinal = envelope.SerializedRootOrdinals[index];
                if (envelope.Nodes[checked((int)ordinal)].ParentOrdinal != root.HierarchyOrdinal)
                {
                    throw new InvalidDataException(
                        "capture service range contains a non-direct component root");
                }
            }
        }
    }

    private static byte[] ParseSourceId(string sourceId)
        => ParseIdentity(sourceId, "mapped source");

    private static byte[] ParseIdentity(string value, string kind)
    {
        if (value.Length != 32)
        {
            throw new InvalidDataException($"capture {kind} identity is not 128-bit hexadecimal");
        }
        try
        {
            return Convert.FromHexString(value);
        }
        catch (FormatException ex)
        {
            throw new InvalidDataException($"capture {kind} identity is not hexadecimal", ex);
        }
    }
}
