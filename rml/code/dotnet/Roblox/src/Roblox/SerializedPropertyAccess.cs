using System.Buffers.Binary;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;

using RML.Interop;

namespace Roblox;

[Flags]
public enum SerializedPropertyAttributes : uint
{
    None = 0,
    XmlRead = 1u << 0,
    XmlWrite = 1u << 1,
    Scriptable = 1u << 2,
    ReadOnly = 1u << 3,
    WriteOnly = 1u << 4,
    Binary = 1u << 5,
    Excluded = 1u << 6,
    Accessible = 1u << 7,
    Reference = 1u << 8,
}

public readonly record struct SerializedPropertyDescriptor(
    string Name,
    string TypeName,
    SerializedPropertyAttributes Attributes)
{
    public bool IsAccessible => Attributes.HasFlag(SerializedPropertyAttributes.Accessible);
    public bool IsExcluded => Attributes.HasFlag(SerializedPropertyAttributes.Excluded);
    public bool IsReference => Attributes.HasFlag(SerializedPropertyAttributes.Reference);
}

public readonly record struct SerializedPropertySnapshot(
    SerializedPropertyDescriptor Descriptor,
    byte[] Value)
{
    public nuint ReferenceHandle
    {
        get
        {
            if (!Descriptor.IsReference || Value.Length != sizeof(ulong))
            {
                throw new InvalidOperationException(
                    $"Serialized property '{Descriptor.Name}' is not a valid reference snapshot.");
            }
            return checked((nuint)BinaryPrimitives.ReadUInt64LittleEndian(Value));
        }
    }
}

/// <summary>
/// Carbon's deliberately narrow elevated reflection seam. It transports only
/// canonical, binary, non-scriptable values that the engine persists in
/// place/model files; native code independently enforces the same policy.
/// </summary>
public static unsafe class SerializedPropertyAccess
{
    private static ReadOnlySpan<byte> SnapshotMagic => "RMLPROP1"u8;

    public static SerializedPropertyDescriptor? Describe(Object instance, string propertyName)
    {
        ArgumentNullException.ThrowIfNull(instance);
        ArgumentNullException.ThrowIfNull(propertyName);

        return Interop.Reflection.TryDescribeSerializedProperty(
            (void*)instance.Handle, propertyName, out var flags, out var typeName)
            ? new SerializedPropertyDescriptor(propertyName, typeName, (SerializedPropertyAttributes)flags)
            : null;
    }

    public static byte[] Read(Object instance, string propertyName)
    {
        ArgumentNullException.ThrowIfNull(instance);
        ArgumentNullException.ThrowIfNull(propertyName);
        return Interop.Reflection.ReadSerializedProperty((void*)instance.Handle, propertyName);
    }

    public static IReadOnlyDictionary<string, SerializedPropertySnapshot> Snapshot(Object instance)
    {
        ArgumentNullException.ThrowIfNull(instance);
        return ParseSnapshot(Interop.Reflection.SnapshotSerializedProperties((void*)instance.Handle));
    }

    internal static IReadOnlyDictionary<string, SerializedPropertySnapshot> ParseSnapshot(
        ReadOnlySpan<byte> payload)
    {
        if (payload.Length < SnapshotMagic.Length + sizeof(uint) ||
            !payload[..SnapshotMagic.Length].SequenceEqual(SnapshotMagic))
        {
            throw new InvalidDataException("Serialized property snapshot magic is invalid.");
        }

        var offset = SnapshotMagic.Length;
        var count = ReadUInt32(payload, ref offset, "property count");
        if (count > 100_000)
        {
            throw new InvalidDataException("Serialized property snapshot exceeds the property limit.");
        }

        var result = new Dictionary<string, SerializedPropertySnapshot>(
            checked((int)count),
            StringComparer.Ordinal);
        for (uint index = 0; index < count; index++)
        {
            var nameLength = ReadUInt16(payload, ref offset, "property name length");
            var typeLength = ReadUInt16(payload, ref offset, "property type length");
            var attributes = (SerializedPropertyAttributes)ReadUInt32(
                payload,
                ref offset,
                "property attributes");
            var valueLength = ReadUInt64(payload, ref offset, "property value length");
            if (nameLength == 0 || typeLength == 0 || valueLength > int.MaxValue)
            {
                throw new InvalidDataException("Serialized property snapshot entry is invalid.");
            }
            var name = ReadUtf8(payload, ref offset, nameLength, "property name");
            var typeName = ReadUtf8(payload, ref offset, typeLength, "property type");
            var value = ReadBytes(payload, ref offset, checked((int)valueLength), "property value");
            var descriptor = new SerializedPropertyDescriptor(name, typeName, attributes);
            if (descriptor.IsReference && value.Length != sizeof(ulong))
            {
                throw new InvalidDataException(
                    $"Serialized reference snapshot '{name}' has an invalid value length.");
            }
            if (!result.TryAdd(name, new(descriptor, value)))
            {
                throw new InvalidDataException(
                    $"Serialized property snapshot repeats property '{name}'.");
            }
        }
        if (offset != payload.Length)
        {
            throw new InvalidDataException("Serialized property snapshot has trailing bytes.");
        }
        return result;
    }

    private static ushort ReadUInt16(ReadOnlySpan<byte> payload, ref int offset, string field)
    {
        var bytes = ReadSpan(payload, ref offset, sizeof(ushort), field);
        return BinaryPrimitives.ReadUInt16LittleEndian(bytes);
    }

    private static uint ReadUInt32(ReadOnlySpan<byte> payload, ref int offset, string field)
    {
        var bytes = ReadSpan(payload, ref offset, sizeof(uint), field);
        return BinaryPrimitives.ReadUInt32LittleEndian(bytes);
    }

    private static ulong ReadUInt64(ReadOnlySpan<byte> payload, ref int offset, string field)
    {
        var bytes = ReadSpan(payload, ref offset, sizeof(ulong), field);
        return BinaryPrimitives.ReadUInt64LittleEndian(bytes);
    }

    private static string ReadUtf8(
        ReadOnlySpan<byte> payload,
        ref int offset,
        int length,
        string field)
    {
        var bytes = ReadSpan(payload, ref offset, length, field);
        try
        {
            return new UTF8Encoding(false, true).GetString(bytes);
        }
        catch (DecoderFallbackException error)
        {
            throw new InvalidDataException($"Serialized property snapshot {field} is not UTF-8.", error);
        }
    }

    private static byte[] ReadBytes(
        ReadOnlySpan<byte> payload,
        ref int offset,
        int length,
        string field) => ReadSpan(payload, ref offset, length, field).ToArray();

    private static ReadOnlySpan<byte> ReadSpan(
        ReadOnlySpan<byte> payload,
        ref int offset,
        int length,
        string field)
    {
        if (length < 0 || offset > payload.Length - length)
        {
            throw new InvalidDataException($"Serialized property snapshot {field} is truncated.");
        }
        var value = payload.Slice(offset, length);
        offset += length;
        return value;
    }

    public static bool Write(Object instance, string propertyName, ReadOnlySpan<byte> value)
    {
        ArgumentNullException.ThrowIfNull(instance);
        ArgumentNullException.ThrowIfNull(propertyName);
        return Interop.Reflection.WriteSerializedProperty((void*)instance.Handle, propertyName, value);
    }

    public static bool Copy(Object source, Object destination, string propertyName)
    {
        ArgumentNullException.ThrowIfNull(source);
        ArgumentNullException.ThrowIfNull(destination);
        ArgumentNullException.ThrowIfNull(propertyName);
        return Interop.Reflection.CopySerializedProperty(
            (void*)source.Handle, (void*)destination.Handle, propertyName);
    }

    public static IDisposable Observe(DataModel dataModel, Action<Instance, string> propertyChanged)
    {
        ArgumentNullException.ThrowIfNull(dataModel);
        ArgumentNullException.ThrowIfNull(propertyChanged);
        return new Observation(dataModel.Handle, propertyChanged);
    }

    /// <summary>
    /// Runs a callback from the native scheduler step for this exact DataModel.
    /// This remains active while Studio is idle in edit mode, unlike RunService
    /// signals exposed through the regular reflection event bridge.
    /// </summary>
    public static EngineThreadPump PumpEngineThread(
        DataModel dataModel,
        Action callback)
    {
        ArgumentNullException.ThrowIfNull(dataModel);
        ArgumentNullException.ThrowIfNull(callback);
        return new EngineThreadPump(dataModel.Handle, callback);
    }

    public sealed class EngineThreadPump : IDisposable
    {
        private readonly object _sync = new();
        private GCHandle _state;
        private nuint _nativeHandle;

        internal EngineThreadPump(nuint dataModel, Action callback)
        {
            _state = GCHandle.Alloc(callback);
            try
            {
                _nativeHandle = Interop.RegisterEngineThreadPump(
                    (void*)dataModel,
                    &OnStep,
                    (void*)GCHandle.ToIntPtr(_state));
                if (_nativeHandle == 0)
                {
                    throw new InvalidOperationException("The native engine-thread pump could not be registered.");
                }
            }
            catch
            {
                _state.Free();
                throw;
            }
        }

        public void Wake()
        {
            lock (_sync)
            {
                var nativeHandle = _nativeHandle;
                if (nativeHandle == 0 || !Interop.WakeEngineThreadPump(nativeHandle))
                {
                    throw new InvalidOperationException("The native engine-thread pump could not be woken.");
                }
            }
        }

        [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
        private static void OnStep(void* state)
        {
            if (state == null)
            {
                return;
            }

            try
            {
                ((Action)GCHandle.FromIntPtr((nint)state).Target!).Invoke();
            }
            catch
            {
                // Exceptions must not cross the unmanaged scheduler callback.
            }
        }

        public void Dispose()
        {
            lock (_sync)
            {
                var nativeHandle = _nativeHandle;
                _nativeHandle = 0;
                if (nativeHandle != 0 && !Interop.UnregisterEngineThreadPump(nativeHandle))
                {
                    // A scheduler callback can still own this state during
                    // engine shutdown. Leaking one handle is safer than allowing
                    // a submitted callback into freed state.
                    return;
                }

                if (_state.IsAllocated)
                {
                    _state.Free();
                }
                GC.SuppressFinalize(this);
            }
        }
    }

    private sealed class Observation : IDisposable
    {
        private GCHandle _state;
        private nuint _nativeHandle;

        public Observation(nuint instance, Action<Instance, string> callback)
        {
            _state = GCHandle.Alloc(callback);
            try
            {
                _nativeHandle = Interop.Reflection.ObserveSerializedProperties(
                    (void*)instance,
                    &OnChanged,
                    (void*)GCHandle.ToIntPtr(_state));
                if (_nativeHandle == 0)
                {
                    throw new InvalidOperationException("The engine did not expose native DataModel.ItemChanged observation.");
                }
            }
            catch
            {
                _state.Free();
                throw;
            }
        }

        [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
        private static void OnChanged(void* state, InteropVariant* args, uint argCount)
        {
            if (state == null || args == null || argCount < 2)
            {
                return;
            }
            try
            {
                var callback = (Action<Instance, string>)GCHandle.FromIntPtr((nint)state).Target!;
                if (Reflection.ConvertVariant(args[0], typeof(Instance)) is Instance instance &&
                    Reflection.ConvertVariant(args[1], typeof(string)) is string propertyName)
                {
                    callback(instance, propertyName);
                }
            }
            catch
            {
                // Exceptions must not cross the unmanaged engine callback.
            }
        }

        public void Dispose()
        {
            var nativeHandle = _nativeHandle;
            _nativeHandle = 0;
            if (nativeHandle != 0)
            {
                Interop.Reflection.EventDisconnect(nativeHandle);
            }
            if (_state.IsAllocated)
            {
                _state.Free();
            }
            GC.SuppressFinalize(this);
        }

        ~Observation() => Dispose();
    }
}
