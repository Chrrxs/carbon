using System.Buffers.Binary;
using System.Text;

namespace Carbon.RmlBridge;

/// <summary>
/// Reads and compares Carbon/MCP markers in Roblox's persisted
/// AttributesSerialize wire value. This keeps bootstrap and transport filtering
/// on the same exact-serialization seam used by capture instead of relying on
/// the generic reflection Variant ABI.
/// </summary>
internal static class ManifestIdentityAttributeCodec
{
    internal const string AttributeName = "__StudioWorktree_CarbonManifestId";
    internal const string SerializedPropertyName = "AttributesSerialize";

    private static readonly byte[] AttributeNameUtf8 = Encoding.UTF8.GetBytes(AttributeName);
    private static readonly UTF8Encoding StrictUtf8 = new(false, true);

    public static string? Decode(
        ReadOnlySpan<byte> serialized,
        string className,
        string instanceName)
    {
        try
        {
            return Decode(serialized);
        }
        catch (InvalidDataException error)
        {
            throw new InvalidDataException(
                $"{SerializedPropertyName} is invalid on {className} {instanceName}: {error.Message}",
                error);
        }
    }

    internal static bool MatchesIgnoringTransportMcpPlaceId(
        ReadOnlySpan<byte> baseline,
        ReadOnlySpan<byte> live)
    {
        try
        {
            var baselineAttributes = ParseWireAttributes(baseline);
            var liveAttributes = ParseWireAttributes(live);
            if (!liveAttributes.Remove("__MCPPlaceId", out var transport)
                || transport.TypeId != 0x02
                || !StringValueIsUuid(transport.Value))
            {
                return false;
            }
            baselineAttributes.Remove("__MCPPlaceId");
            if (baselineAttributes.Count != liveAttributes.Count)
            {
                return false;
            }
            foreach (var (name, expected) in baselineAttributes)
            {
                if (!liveAttributes.TryGetValue(name, out var current)
                    || expected.TypeId != current.TypeId
                    || !expected.Value.AsSpan().SequenceEqual(current.Value))
                {
                    return false;
                }
            }
            return true;
        }
        catch (InvalidDataException)
        {
            return false;
        }
    }

    private static Dictionary<string, AttributeWireValue> ParseWireAttributes(
        ReadOnlySpan<byte> serialized)
    {
        var attributes = new Dictionary<string, AttributeWireValue>(StringComparer.Ordinal);
        if (serialized.IsEmpty)
        {
            return attributes;
        }

        var offset = 0;
        var count = ReadUInt32(serialized, ref offset, "attribute count");
        if (count > (uint)((serialized.Length - offset) / 6))
        {
            throw new InvalidDataException("attribute count exceeds the serialized value");
        }
        for (uint index = 0; index < count; index++)
        {
            var name = DecodeAttributeName(ReadString(serialized, ref offset, "attribute name"));
            var typeId = ReadByte(serialized, ref offset, "attribute type");
            var valueStart = offset;
            SkipValue(serialized, ref offset, typeId);
            if (!attributes.TryAdd(
                name,
                new(typeId, serialized[valueStart..offset].ToArray())))
            {
                throw new InvalidDataException($"attribute {name} is repeated");
            }
        }
        if (offset != serialized.Length)
        {
            throw new InvalidDataException("serialized attributes contain trailing bytes");
        }
        return attributes;
    }

    private static string DecodeAttributeName(ReadOnlySpan<byte> encoded)
    {
        try
        {
            return StrictUtf8.GetString(encoded);
        }
        catch (DecoderFallbackException error)
        {
            throw new InvalidDataException("attribute name is not valid UTF-8", error);
        }
    }

    private static bool StringValueIsUuid(ReadOnlySpan<byte> encoded)
    {
        var offset = 0;
        var value = ReadString(encoded, ref offset, "string attribute");
        return offset == encoded.Length
            && Guid.TryParseExact(Encoding.UTF8.GetString(value), "D", out _);
    }

    private static string? Decode(ReadOnlySpan<byte> serialized)
    {
        if (serialized.IsEmpty)
        {
            return null;
        }

        var offset = 0;
        var count = ReadUInt32(serialized, ref offset, "attribute count");
        if (count > (uint)((serialized.Length - offset) / 6))
        {
            throw new InvalidDataException("attribute count exceeds the serialized value");
        }

        string? manifestIdentity = null;
        for (uint index = 0; index < count; index++)
        {
            var name = ReadString(serialized, ref offset, "attribute name");
            var typeId = ReadByte(serialized, ref offset, "attribute type");
            if (name.SequenceEqual(AttributeNameUtf8))
            {
                if (manifestIdentity is not null)
                {
                    throw new InvalidDataException($"{AttributeName} is repeated");
                }
                if (typeId != 0x02)
                {
                    throw new InvalidDataException($"{AttributeName} must be a string attribute");
                }

                var encoded = ReadString(serialized, ref offset, AttributeName);
                if (!IsEncodedManifestIdentity(encoded))
                {
                    throw new InvalidDataException(
                        $"{AttributeName} must be a nonzero 128-bit hexadecimal value");
                }
                manifestIdentity = ManifestIdentity
                    .Parse(Encoding.ASCII.GetString(encoded))
                    .ToString();
            }
            else
            {
                SkipValue(serialized, ref offset, typeId);
            }
        }

        if (offset != serialized.Length)
        {
            throw new InvalidDataException("serialized attributes contain trailing bytes");
        }
        return manifestIdentity;
    }

    private static void SkipValue(ReadOnlySpan<byte> serialized, ref int offset, byte typeId)
    {
        switch (typeId)
        {
            case 0x02: // String / BinaryString
                _ = ReadString(serialized, ref offset, "string attribute");
                return;
            case 0x03: // Bool
                Skip(serialized, ref offset, 1, "bool attribute");
                return;
            case 0x04: // Int32
            case 0x05: // Float32
            case 0x0E: // BrickColor
                Skip(serialized, ref offset, 4, "four-byte attribute");
                return;
            case 0x06: // Float64
            case 0x09: // UDim
            case 0x10: // Vector2
            case 0x1B: // NumberRange
                Skip(serialized, ref offset, 8, "eight-byte attribute");
                return;
            case 0x0F: // Color3
            case 0x11: // Vector3
                Skip(serialized, ref offset, 12, "twelve-byte attribute");
                return;
            case 0x0A: // UDim2
            case 0x1C: // Rect
                Skip(serialized, ref offset, 16, "sixteen-byte attribute");
                return;
            case 0x14: // CFrame
                Skip(serialized, ref offset, 12, "CFrame position");
                if (ReadByte(serialized, ref offset, "CFrame rotation") == 0)
                {
                    Skip(serialized, ref offset, 36, "CFrame rotation matrix");
                }
                return;
            case 0x15: // EnumItem
                _ = ReadString(serialized, ref offset, "enum type");
                Skip(serialized, ref offset, 4, "enum value");
                return;
            case 0x17: // NumberSequence
                SkipElements(serialized, ref offset, 12, "NumberSequence");
                return;
            case 0x19: // ColorSequence
                SkipElements(serialized, ref offset, 20, "ColorSequence");
                return;
            case 0x21: // Font
                Skip(serialized, ref offset, 3, "font weight and style");
                _ = ReadString(serialized, ref offset, "font family");
                _ = ReadString(serialized, ref offset, "font cached face");
                return;
            default:
                throw new InvalidDataException($"attribute type 0x{typeId:x2} is unsupported");
        }
    }

    private static void SkipElements(
        ReadOnlySpan<byte> serialized,
        ref int offset,
        int stride,
        string description)
    {
        var count = ReadUInt32(serialized, ref offset, $"{description} length");
        if (count > int.MaxValue / stride)
        {
            throw new InvalidDataException($"{description} length is too large");
        }
        Skip(serialized, ref offset, checked((int)count * stride), description);
    }

    private static ReadOnlySpan<byte> ReadString(
        ReadOnlySpan<byte> serialized,
        ref int offset,
        string description)
    {
        var length = ReadUInt32(serialized, ref offset, $"{description} length");
        if (length > int.MaxValue)
        {
            throw new InvalidDataException($"{description} length is too large");
        }
        return Take(serialized, ref offset, (int)length, description);
    }

    private static byte ReadByte(
        ReadOnlySpan<byte> serialized,
        ref int offset,
        string description) => Take(serialized, ref offset, 1, description)[0];

    private static uint ReadUInt32(
        ReadOnlySpan<byte> serialized,
        ref int offset,
        string description) => BinaryPrimitives.ReadUInt32LittleEndian(
            Take(serialized, ref offset, sizeof(uint), description));

    private static void Skip(
        ReadOnlySpan<byte> serialized,
        ref int offset,
        int length,
        string description) => _ = Take(serialized, ref offset, length, description);

    private static ReadOnlySpan<byte> Take(
        ReadOnlySpan<byte> serialized,
        ref int offset,
        int length,
        string description)
    {
        if (length < 0 || offset > serialized.Length - length)
        {
            throw new InvalidDataException($"{description} is truncated");
        }
        var value = serialized.Slice(offset, length);
        offset += length;
        return value;
    }

    private static bool IsAsciiHex(byte value) =>
        value is >= (byte)'0' and <= (byte)'9'
        or >= (byte)'a' and <= (byte)'f'
        or >= (byte)'A' and <= (byte)'F';

    private static bool IsEncodedManifestIdentity(ReadOnlySpan<byte> encoded)
    {
        if (encoded.Length != 32)
        {
            return false;
        }
        foreach (var value in encoded)
        {
            if (!IsAsciiHex(value))
            {
                return false;
            }
        }
        return true;
    }

    private readonly record struct AttributeWireValue(byte TypeId, byte[] Value);
}
