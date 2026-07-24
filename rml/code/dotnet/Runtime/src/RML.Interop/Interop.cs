using System.Buffers;
using System.Collections.Concurrent;
using System.Runtime.InteropServices;
using System.Text;

using RML.Logging;

namespace RML.Interop;

public static unsafe class Interop
{
    public static bool IsInitialized => Table != null;

    internal static NativeInterop.InteropTable* Table { get; private set; }

    public static void Log(LogLevel level, string message)
    {
        if (message is null)
        {
            return;
        }

        var table = Table;
        try
        {
            if (table != null && table->Log != null)
            {
                var bytes = Encoding.UTF8.GetBytes(message);
                fixed (byte* p = bytes)
                {
                    table->Log((int)level, (sbyte*)p, bytes.Length);
                }

                return;
            }
        }
        catch (NullReferenceException ex)
        {
            Console.Error.WriteLine($"[RML/Error] Interop.Log native call failed: {ex.Message}");
        }

        var writer = level >= LogLevel.Warn ? Console.Error : Console.Out;
        writer.WriteLine($"[RML/{level}] {message}");
    }

    public static void Initialize(nint tablePtr)
    {
        if (tablePtr == nint.Zero)
        {
            throw new ArgumentException("Invalid interop table pointer or size.");
        }

        if (sizeof(InteropVariant) != 16)
        {
            throw new InvalidOperationException(
                $"InteropVariant ABI mismatch: managed size is {sizeof(InteropVariant)}, expected 16.");
        }

        var table = (NativeInterop.InteropTable*)tablePtr;
        if (table->Version != NativeInterop.InteropTableVersion)
        {
            throw new InvalidOperationException(
                $"Unsupported interop table version: {table->Version} (expected {NativeInterop.InteropTableVersion}).");
        }

        if (table->Size != 0 && table->Size != (uint)sizeof(NativeInterop.InteropTable))
        {
            throw new InvalidOperationException(
                $"Interop table size mismatch: native {table->Size} != managed {sizeof(NativeInterop.InteropTable)}.");
        }

        Table = table;
    }

    public static void Uninitialize()
    {
        Reflection.ClearCaches();
        Table = null;
    }

    public static void FreeNativeString(nint ptr)
    {
        if (ptr == nint.Zero || !IsInitialized || Table == null || Table->FreeString == null)
        {
            return;
        }

        Table->FreeString((sbyte*)ptr);
    }

    public static void FreeNativeArray(nint ptr)
    {
        if (ptr == nint.Zero || !IsInitialized || Table == null || Table->FreeNativePtr == null)
        {
            return;
        }

        Table->FreeNativePtr((void*)ptr);
    }

    public static nuint RegisterEngineThreadPump(
        void* dataModel,
        delegate* unmanaged[Cdecl]<void*, void> callback,
        void* state)
    {
        if (!IsInitialized || Table == null || Table->EngineThreadPumpRegister == null)
        {
            throw new InvalidOperationException(
                "Interop table is not initialized or EngineThreadPumpRegister is unavailable.");
        }

        if (dataModel == null || callback == null)
        {
            throw new ArgumentException("Engine thread pump requires a DataModel and callback.");
        }

        return Table->EngineThreadPumpRegister(dataModel, callback, state);
    }

    public static bool WakeEngineThreadPump(nuint pumpHandle)
    {
        if (pumpHandle == 0 || !IsInitialized || Table == null || Table->EngineThreadPumpWake == null)
        {
            return false;
        }

        return Table->EngineThreadPumpWake(pumpHandle) != 0;
    }

    public static bool UnregisterEngineThreadPump(nuint pumpHandle)
    {
        if (pumpHandle == 0 || !IsInitialized || Table == null || Table->EngineThreadPumpUnregister == null)
        {
            return false;
        }

        return Table->EngineThreadPumpUnregister(pumpHandle) != 0;
    }

    public static bool IsInstanceSerializable(void* instance)
    {
        return instance != null && IsInitialized && Table != null && Table->InstanceIsSerializable != null &&
               Table->InstanceIsSerializable(instance) != 0;
    }

    /// <summary>
    /// Reorders all direct children of <paramref name="parent"/> without changing
    /// any Parent property. The native side accepts only an exact, duplicate-free
    /// permutation of the current child vector.
    /// </summary>
    public static bool ReorderInstanceChildren(nuint parent, ReadOnlySpan<nuint> children)
    {
        if (parent == 0 || !IsInitialized || Table == null || Table->InstanceReorderChildren == null)
        {
            return false;
        }

        fixed (nuint* childHandles = children)
        {
            return Table->InstanceReorderChildren((void*)parent, childHandles, (uint)children.Length) != 0;
        }
    }

    public static byte[] ReadInstanceHierarchy(
        nuint root,
        nuint excludedRoot,
        bool includeCaptureMetadata = false)
    {
        if (root == 0 || !IsInitialized || Table == null || Table->InstanceReadHierarchy == null)
        {
            throw new InvalidOperationException(
                "Interop table is not initialized or InstanceReadHierarchy is unavailable.");
        }

        InteropVariant result = default;
        if (Table->InstanceReadHierarchy(
                (void*)root,
                (void*)excludedRoot,
                includeCaptureMetadata ? 1 : 0,
                &result) == 0 ||
            result.Tag != InteropVariant.Tags.Bytes || result.AsPointer == 0)
        {
            throw new InvalidOperationException("The engine could not read the instance hierarchy.");
        }

        var bytes = (InteropBytes*)result.AsPointer;
        try
        {
            if (bytes->Size > int.MaxValue || (bytes->Size > 0 && bytes->Data == null))
            {
                throw new InvalidOperationException("The engine returned an invalid instance hierarchy.");
            }
            var value = new byte[(int)bytes->Size];
            if (value.Length > 0)
            {
                Marshal.Copy((nint)bytes->Data, value, 0, value.Length);
            }
            return value;
        }
        finally
        {
            FreeNativeArray((nint)result.AsPointer);
        }
    }

    public static bool QueueStudioLocalPlaceSave()
    {
        return IsInitialized && Table != null && Table->StudioQueueLocalPlaceSave != null &&
               Table->StudioQueueLocalPlaceSave() != 0;
    }

    public static nuint ModsMenuAddAction(nuint parent, string text, nint callback, nint state)
    {
        if (!IsInitialized || Table == null || Table->ModsMenuAddAction == null || callback == nint.Zero)
        {
            return 0;
        }

        ArgumentNullException.ThrowIfNull(text);

        var byteCount = Encoding.UTF8.GetByteCount(text);
        var buffer = stackalloc byte[byteCount + 1];
        Encoding.UTF8.GetBytes(text, new Span<byte>(buffer, byteCount));
        buffer[byteCount] = 0;

        var cb = (delegate* unmanaged[Cdecl]<void*, InteropVariant*, uint, void>)callback;
        return Table->ModsMenuAddAction(parent, (sbyte*)buffer, cb, (void*)state);
    }

    public static nuint ModsMenuAddSubmenu(nuint parent, string text)
    {
        if (!IsInitialized || Table == null || Table->ModsMenuAddSubmenu == null)
        {
            return 0;
        }

        ArgumentNullException.ThrowIfNull(text);

        var byteCount = Encoding.UTF8.GetByteCount(text);
        var buffer = stackalloc byte[byteCount + 1];
        Encoding.UTF8.GetBytes(text, new Span<byte>(buffer, byteCount));
        buffer[byteCount] = 0;

        return Table->ModsMenuAddSubmenu(parent, (sbyte*)buffer);
    }

    public static nuint ModsMenuAddSeparator(nuint parent)
    {
        if (!IsInitialized || Table == null || Table->ModsMenuAddSeparator == null)
        {
            return 0;
        }

        return Table->ModsMenuAddSeparator(parent);
    }

    public static nuint ModsMenuAddCheckable(nuint parent, string text, bool initial, nint callback, nint state)
    {
        if (!IsInitialized || Table == null || Table->ModsMenuAddCheckable == null || callback == nint.Zero)
        {
            return 0;
        }

        ArgumentNullException.ThrowIfNull(text);

        var byteCount = Encoding.UTF8.GetByteCount(text);
        var buffer = stackalloc byte[byteCount + 1];
        Encoding.UTF8.GetBytes(text, new Span<byte>(buffer, byteCount));
        buffer[byteCount] = 0;

        var cb = (delegate* unmanaged[Cdecl]<void*, InteropVariant*, uint, void>)callback;
        return Table->ModsMenuAddCheckable(parent, (sbyte*)buffer, initial ? 1 : 0, cb, (void*)state);
    }

    public static void ModsMenuSetItemIcon(nuint id, string path)
    {
        if (id == 0 || !IsInitialized || Table == null || Table->ModsMenuSetItemIcon == null)
        {
            return;
        }

        ArgumentNullException.ThrowIfNull(path);

        var byteCount = Encoding.UTF8.GetByteCount(path);
        var buffer = stackalloc byte[byteCount + 1];
        Encoding.UTF8.GetBytes(path, new Span<byte>(buffer, byteCount));
        buffer[byteCount] = 0;

        Table->ModsMenuSetItemIcon(id, (sbyte*)buffer);
    }

    public static void ModsMenuRemove(nuint id)
    {
        if (id == 0 || !IsInitialized || Table == null || Table->ModsMenuRemove == null)
        {
            return;
        }

        Table->ModsMenuRemove(id);
    }

    public class Reflection
    {
        private const int StackAllocArgThreshold = 64;
        private const int MaxArgCount = 4096;

        private static readonly ConcurrentDictionary<string, nint> CachedMemberNames = new(StringComparer.Ordinal);

        internal static void ClearCaches()
        {
            foreach (var ptr in CachedMemberNames.Values)
            {
                if (ptr != nint.Zero)
                {
                    Marshal.FreeHGlobal(ptr);
                }
            }

            CachedMemberNames.Clear();
        }

        private static sbyte* GetCachedMemberName(string memberName)
        {
            var ptr = CachedMemberNames.GetOrAdd(memberName, static name =>
            {
                var bytes = Encoding.UTF8.GetBytes(name);
                var mem = Marshal.AllocHGlobal(bytes.Length + 1);
                Marshal.Copy(bytes, 0, mem, bytes.Length);
                Marshal.WriteByte(mem + bytes.Length, 0);
                return mem;
            });

            return (sbyte*)ptr;
        }

        public static InteropVariant Invoke(void* instance, string methodName, params object?[]? args)
        {
            if (!IsInitialized || Table == null || Table->ReflectionInvoke == null)
            {
                throw new InvalidOperationException(
                    "Interop table is not initialized or ReflectionInvoke is unavailable.");
            }

            ArgumentNullException.ThrowIfNull(methodName);

            var nameS = GetCachedMemberName(methodName);
            var argCount = args?.Length ?? 0;
            InteropVariant result = default;

            if (argCount == 0)
            {
                Table->ReflectionInvoke(instance, nameS, null, 0, &result);
                return result;
            }

            ValidateArgCount(argCount);

            if (argCount <= StackAllocArgThreshold)
            {
                var tempPtrs = stackalloc nint[argCount];
                var argVariants = stackalloc InteropVariant[argCount];
                var tempPtrCount = 0;

                try
                {
                    BuildArgVariants(args!, argCount, tempPtrs, ref tempPtrCount, argVariants);
                    Table->ReflectionInvoke(instance, nameS, argVariants, (uint)argCount, &result);
                    return result;
                }
                finally
                {
                    FreeTempPtrs(tempPtrs, tempPtrCount);
                }
            }

            var tempPtrsArr = ArrayPool<nint>.Shared.Rent(argCount);
            var argVariantsArr = ArrayPool<InteropVariant>.Shared.Rent(argCount);

            try
            {
                fixed (nint* tempPtrs = tempPtrsArr)
                fixed (InteropVariant* argVariants = argVariantsArr)
                {
                    var tempPtrCount = 0;
                    try
                    {
                        BuildArgVariants(args!, argCount, tempPtrs, ref tempPtrCount, argVariants);
                        Table->ReflectionInvoke(instance, nameS, argVariants, (uint)argCount, &result);
                        return result;
                    }
                    finally
                    {
                        FreeTempPtrs(tempPtrs, tempPtrCount);
                    }
                }
            }
            finally
            {
                ArrayPool<nint>.Shared.Return(tempPtrsArr);
                ArrayPool<InteropVariant>.Shared.Return(argVariantsArr);
            }
        }

        public static void InvokeAsync(
            void* instance,
            string methodName,
            delegate* unmanaged[Cdecl]<void*, InteropVariant*, sbyte*, void> callback,
            void* state,
            params object?[]? args)
        {
            if (!IsInitialized || Table == null || Table->ReflectionInvokeAsync == null)
            {
                throw new InvalidOperationException(
                    "Interop table is not initialized or ReflectionInvokeAsync is unavailable.");
            }

            ArgumentNullException.ThrowIfNull(methodName);

            var nameS = GetCachedMemberName(methodName);
            var argCount = args?.Length ?? 0;

            if (argCount == 0)
            {
                Table->ReflectionInvokeAsync(instance, nameS, null, 0, callback, state);
                return;
            }

            ValidateArgCount(argCount);

            if (argCount <= StackAllocArgThreshold)
            {
                var tempPtrs = stackalloc nint[argCount];
                var argVariants = stackalloc InteropVariant[argCount];
                var tempPtrCount = 0;

                try
                {
                    BuildArgVariants(args!, argCount, tempPtrs, ref tempPtrCount, argVariants);
                    Table->ReflectionInvokeAsync(instance, nameS, argVariants, (uint)argCount, callback, state);
                }
                finally
                {
                    FreeTempPtrs(tempPtrs, tempPtrCount);
                }

                return;
            }

            var tempPtrsArr = ArrayPool<nint>.Shared.Rent(argCount);
            var argVariantsArr = ArrayPool<InteropVariant>.Shared.Rent(argCount);

            try
            {
                fixed (nint* tempPtrs = tempPtrsArr)
                fixed (InteropVariant* argVariants = argVariantsArr)
                {
                    var tempPtrCount = 0;
                    try
                    {
                        BuildArgVariants(args!, argCount, tempPtrs, ref tempPtrCount, argVariants);
                        Table->ReflectionInvokeAsync(instance, nameS, argVariants, (uint)argCount, callback, state);
                    }
                    finally
                    {
                        FreeTempPtrs(tempPtrs, tempPtrCount);
                    }
                }
            }
            finally
            {
                ArrayPool<nint>.Shared.Return(tempPtrsArr);
                ArrayPool<InteropVariant>.Shared.Return(argVariantsArr);
            }
        }

        private static void ValidateArgCount(int argCount)
        {
            if (argCount > MaxArgCount)
            {
                throw new ArgumentException(
                    $"Argument count {argCount} exceeds the maximum supported count of {MaxArgCount}.", nameof(argCount));
            }
        }

        private static void BuildArgVariants(
            object?[] args, int argCount, nint* tempPtrs, ref int tempPtrCount, InteropVariant* argVariants)
        {
            for (var i = 0; i < argCount; i++)
            {
                argVariants[i] = BuildVariant(args[i], tempPtrs, ref tempPtrCount);
            }
        }

        private static void FreeTempPtrs(nint* tempPtrs, int tempPtrCount)
        {
            for (var i = 0; i < tempPtrCount; i++)
            {
                if (tempPtrs[i] != 0)
                {
                    Marshal.FreeHGlobal(tempPtrs[i]);
                }
            }
        }

        public static InteropVariant GetProperty(void* instance, string propertyName)
        {
            if (!IsInitialized || Table == null || Table->ReflectionGetProperty == null)
            {
                throw new InvalidOperationException(
                    "Interop table is not initialized or ReflectionGetProperty is unavailable.");
            }

            ArgumentNullException.ThrowIfNull(propertyName);

            var nameS = GetCachedMemberName(propertyName);
            InteropVariant result = default;
            Table->ReflectionGetProperty(instance, nameS, &result);
            return result;
        }

        public static void SetProperty(void* instance, string propertyName, InteropVariant value)
        {
            if (!IsInitialized || Table == null || Table->ReflectionSetProperty == null)
            {
                throw new InvalidOperationException(
                    "Interop table is not initialized or ReflectionSetProperty is unavailable.");
            }

            ArgumentNullException.ThrowIfNull(propertyName);

            var nameS = GetCachedMemberName(propertyName);
            Table->ReflectionSetProperty(instance, nameS, &value);
        }

        public static bool TryDescribeSerializedProperty(
            void* instance, string propertyName, out uint flags, out string typeName)
        {
            flags = 0;
            typeName = string.Empty;
            if (!IsInitialized || Table == null || Table->SerializedPropertyDescribe == null)
            {
                return false;
            }

            ArgumentNullException.ThrowIfNull(propertyName);
            var nameS = GetCachedMemberName(propertyName);
            NativeSerializedPropertyInfo info = default;
            if (Table->SerializedPropertyDescribe(instance, nameS, &info) == 0)
            {
                return false;
            }

            flags = (uint)info.Flags;
            if (info.TypeName != null)
            {
                try
                {
                    typeName = Marshal.PtrToStringUTF8((nint)info.TypeName) ?? string.Empty;
                }
                finally
                {
                    FreeNativeString((nint)info.TypeName);
                }
            }
            return true;
        }

        public static byte[] ReadSerializedProperty(void* instance, string propertyName)
        {
            if (!IsInitialized || Table == null || Table->SerializedPropertyRead == null)
            {
                throw new InvalidOperationException(
                    "Interop table is not initialized or SerializedPropertyRead is unavailable.");
            }

            ArgumentNullException.ThrowIfNull(propertyName);
            var nameS = GetCachedMemberName(propertyName);
            InteropVariant result = default;
            if (Table->SerializedPropertyRead(instance, nameS, &result) == 0 ||
                result.Tag != InteropVariant.Tags.Bytes || result.AsPointer == 0)
            {
                throw new InvalidOperationException($"Serialized property '{propertyName}' is not accessible.");
            }

            var bytes = (InteropBytes*)result.AsPointer;
            try
            {
                if (bytes->Size > int.MaxValue)
                {
                    throw new InvalidOperationException(
                        $"Serialized property '{propertyName}' exceeds the managed array size limit.");
                }
                if (bytes->Size == 0)
                {
                    return [];
                }
                if (bytes->Data == null)
                {
                    throw new InvalidOperationException($"Serialized property '{propertyName}' returned invalid data.");
                }

                var value = new byte[(int)bytes->Size];
                Marshal.Copy((nint)bytes->Data, value, 0, value.Length);
                return value;
            }
            finally
            {
                FreeNativeArray((nint)result.AsPointer);
            }
        }

        public static byte[] SnapshotSerializedProperties(void* instance)
        {
            if (!IsInitialized || Table == null || Table->SerializedPropertySnapshot == null)
            {
                throw new InvalidOperationException(
                    "Interop table is not initialized or SerializedPropertySnapshot is unavailable.");
            }

            InteropVariant result = default;
            if (Table->SerializedPropertySnapshot(instance, &result) == 0 ||
                result.Tag != InteropVariant.Tags.Bytes || result.AsPointer == 0)
            {
                throw new InvalidOperationException("The engine could not snapshot serialized properties.");
            }

            var bytes = (InteropBytes*)result.AsPointer;
            try
            {
                if (bytes->Size > int.MaxValue || (bytes->Size > 0 && bytes->Data == null))
                {
                    throw new InvalidOperationException(
                        "The engine returned an invalid serialized property snapshot.");
                }
                var value = new byte[(int)bytes->Size];
                if (value.Length > 0)
                {
                    Marshal.Copy((nint)bytes->Data, value, 0, value.Length);
                }
                return value;
            }
            finally
            {
                FreeNativeArray((nint)result.AsPointer);
            }
        }


        public static bool WriteSerializedProperty(void* instance, string propertyName, ReadOnlySpan<byte> value)
        {
            if (!IsInitialized || Table == null || Table->SerializedPropertyWrite == null)
            {
                throw new InvalidOperationException(
                    "Interop table is not initialized or SerializedPropertyWrite is unavailable.");
            }

            ArgumentNullException.ThrowIfNull(propertyName);
            var nameS = GetCachedMemberName(propertyName);
            fixed (byte* data = value)
            {
                var bytes = new InteropBytes { Data = data, Size = (ulong)value.Length };
                var variant = InteropVariant.FromBytes((nuint)(nint)(&bytes));
                return Table->SerializedPropertyWrite(instance, nameS, &variant) != 0;
            }
        }

        public static bool CopySerializedProperty(void* source, void* destination, string propertyName)
        {
            if (!IsInitialized || Table == null || Table->SerializedPropertyCopy == null)
            {
                throw new InvalidOperationException(
                    "Interop table is not initialized or SerializedPropertyCopy is unavailable.");
            }

            ArgumentNullException.ThrowIfNull(propertyName);
            var nameS = GetCachedMemberName(propertyName);
            return Table->SerializedPropertyCopy(source, destination, nameS) != 0;
        }

        public static nuint ObserveSerializedProperties(
            void* instance,
            delegate* unmanaged[Cdecl]<void*, InteropVariant*, uint, void> callback,
            void* state)
        {
            if (!IsInitialized || Table == null || Table->SerializedPropertyObserve == null)
            {
                throw new InvalidOperationException(
                    "Interop table is not initialized or SerializedPropertyObserve is unavailable.");
            }
            return Table->SerializedPropertyObserve(instance, callback, state);
        }

        public static nuint EventConnect(
            void* instance,
            string eventName,
            delegate* unmanaged[Cdecl]<void*, InteropVariant*, uint, void> callback,
            void* state)
        {
            if (!IsInitialized || Table == null || Table->ReflectionEventConnect == null)
            {
                throw new InvalidOperationException(
                    "Interop table is not initialized or ReflectionEventConnect is unavailable.");
            }

            ArgumentNullException.ThrowIfNull(eventName);

            var nameS = GetCachedMemberName(eventName);
            return Table->ReflectionEventConnect(instance, nameS, callback, state);
        }

        public static void EventDisconnect(nuint connectionHandle)
        {
            if (connectionHandle == 0 || !IsInitialized || Table == null || Table->ReflectionEventDisconnect == null)
            {
                return;
            }

            Table->ReflectionEventDisconnect(connectionHandle);
        }

        public static void EventFire(void* instance, string eventName, params object?[]? args)
        {
            if (!IsInitialized || Table == null || Table->ReflectionEventFire == null)
            {
                throw new InvalidOperationException(
                    "Interop table is not initialized or ReflectionEventFire is unavailable.");
            }

            ArgumentNullException.ThrowIfNull(eventName);

            var nameS = GetCachedMemberName(eventName);
            var argCount = args?.Length ?? 0;

            if (argCount == 0)
            {
                Table->ReflectionEventFire(instance, nameS, null, 0);
                return;
            }

            ValidateArgCount(argCount);

            if (argCount <= StackAllocArgThreshold)
            {
                var tempPtrs = stackalloc nint[argCount];
                var argVariants = stackalloc InteropVariant[argCount];
                var tempPtrCount = 0;

                try
                {
                    BuildArgVariants(args!, argCount, tempPtrs, ref tempPtrCount, argVariants);
                    Table->ReflectionEventFire(instance, nameS, argVariants, (uint)argCount);
                }
                finally
                {
                    FreeTempPtrs(tempPtrs, tempPtrCount);
                }

                return;
            }

            var tempPtrsArr = ArrayPool<nint>.Shared.Rent(argCount);
            var argVariantsArr = ArrayPool<InteropVariant>.Shared.Rent(argCount);

            try
            {
                fixed (nint* tempPtrs = tempPtrsArr)
                fixed (InteropVariant* argVariants = argVariantsArr)
                {
                    var tempPtrCount = 0;
                    try
                    {
                        BuildArgVariants(args!, argCount, tempPtrs, ref tempPtrCount, argVariants);
                        Table->ReflectionEventFire(instance, nameS, argVariants, (uint)argCount);
                    }
                    finally
                    {
                        FreeTempPtrs(tempPtrs, tempPtrCount);
                    }
                }
            }
            finally
            {
                ArrayPool<nint>.Shared.Return(tempPtrsArr);
                ArrayPool<InteropVariant>.Shared.Return(argVariantsArr);
            }
        }

        public static void EventDisconnectAll(void* instance, string eventName)
        {
            if (!IsInitialized || Table == null || Table->ReflectionEventDisconnectAll == null)
            {
                return;
            }

            ArgumentNullException.ThrowIfNull(eventName);
            Table->ReflectionEventDisconnectAll(instance, GetCachedMemberName(eventName));
        }

        public static nuint[] EventSlots(void* instance, string eventName)
        {
            if (!IsInitialized || Table == null || Table->ReflectionEventSlots == null)
            {
                return [];
            }

            ArgumentNullException.ThrowIfNull(eventName);

            var nameS = GetCachedMemberName(eventName);
            uint count = 0;
            var ptr = Table->ReflectionEventSlots(instance, nameS, &count);
            if (ptr == null || count == 0)
            {
                return [];
            }

            try
            {
                var handles = new nuint[count];
                for (uint i = 0; i < count; i++)
                {
                    handles[i] = ptr[i];
                }

                return handles;
            }
            finally
            {
                FreeNativeArray((nint)ptr);
            }
        }

        public static void EventSlotFire(void* instance, string eventName, nuint slotHandle, params object?[]? args)
        {
            if (slotHandle == 0 || !IsInitialized || Table == null || Table->EventSlotFire == null)
            {
                return;
            }

            ArgumentNullException.ThrowIfNull(eventName);

            var nameS = GetCachedMemberName(eventName);
            var argCount = args?.Length ?? 0;

            if (argCount == 0)
            {
                Table->EventSlotFire(instance, nameS, slotHandle, null, 0);
                return;
            }

            ValidateArgCount(argCount);

            var tempPtrsArr = ArrayPool<nint>.Shared.Rent(argCount);
            var argVariantsArr = ArrayPool<InteropVariant>.Shared.Rent(argCount);

            try
            {
                fixed (nint* tempPtrs = tempPtrsArr)
                fixed (InteropVariant* argVariants = argVariantsArr)
                {
                    var tempPtrCount = 0;
                    try
                    {
                        BuildArgVariants(args!, argCount, tempPtrs, ref tempPtrCount, argVariants);
                        Table->EventSlotFire(instance, nameS, slotHandle, argVariants, (uint)argCount);
                    }
                    finally
                    {
                        FreeTempPtrs(tempPtrs, tempPtrCount);
                    }
                }
            }
            finally
            {
                ArrayPool<nint>.Shared.Return(tempPtrsArr);
                ArrayPool<InteropVariant>.Shared.Return(argVariantsArr);
            }
        }

        public static void EventSlotDisconnect(nuint slotHandle)
        {
            if (slotHandle == 0 || !IsInitialized || Table == null || Table->EventSlotDisconnect == null)
            {
                return;
            }

            Table->EventSlotDisconnect(slotHandle);
        }

        public static void EventSlotRelease(nuint slotHandle)
        {
            if (slotHandle == 0 || !IsInitialized || Table == null || Table->EventSlotRelease == null)
            {
                return;
            }

            Table->EventSlotRelease(slotHandle);
        }

        public static nuint CreateInstanceByName(string className, int creatorRole)
        {
            if (!IsInitialized || Table == null || Table->CreateInstanceByName == null)
            {
                throw new InvalidOperationException(
                    "Interop table is not initialized or CreateInstanceByName is unavailable.");
            }

            ArgumentNullException.ThrowIfNull(className);

            var bytes = Encoding.UTF8.GetBytes(className);
            fixed (byte* p = bytes)
            {
                return Table->CreateInstanceByName((sbyte*)p, creatorRole);
            }
        }

        private static InteropVariant BuildVariant(object? arg, nint* tempPtrs, ref int tempPtrCount)
        {
            if (arg is null)
            {
                return default;
            }

            return arg switch
            {
                string s => BuildStringVariant(s, tempPtrs, ref tempPtrCount),
                byte[] bytes => BuildBytesVariant(bytes, tempPtrs, ref tempPtrCount),
                bool b => InteropVariant.FromBool(b),
                double d => InteropVariant.FromDouble(d),
                float f => InteropVariant.FromFloat(f),
                Enum e => InteropVariant.FromInt64(Convert.ToInt64(e)),
                IInteropInstance instance => InteropVariant.FromPointer(instance.InteropHandle),
                IEnumerable<IInteropInstance> instances => BuildInstanceArrayVariant(
                    instances, tempPtrs, ref tempPtrCount),
                nuint nu => InteropVariant.FromPointer(nu),
                nint ni => InteropVariant.FromPointer((nuint)ni),
                _ => BuildFallbackVariant(arg, tempPtrs, ref tempPtrCount)
            };
        }

        private static InteropVariant BuildBytesVariant(
            byte[] value, nint* tempPtrs, ref int tempPtrCount)
        {
            // Keep the length-bearing ABI header and its payload in one allocation.
            // BuildArgVariants reserves one temporary pointer per argument, so a
            // contiguous block also preserves that capacity invariant.
            var allocationSize = checked(sizeof(InteropBytes) + value.Length);
            var allocation = Marshal.AllocHGlobal(allocationSize);
            tempPtrs[tempPtrCount++] = allocation;
            var data = (byte*)allocation + sizeof(InteropBytes);
            if (value.Length > 0)
            {
                Marshal.Copy(value, 0, (nint)data, value.Length);
            }
            *(InteropBytes*)allocation = new InteropBytes
            {
                Data = data,
                Size = checked((ulong)value.Length),
            };
            return InteropVariant.FromBytes((nuint)allocation);
        }

        private static InteropVariant BuildInstanceArrayVariant(
            IEnumerable<IInteropInstance> instances, nint* tempPtrs, ref int tempPtrCount)
        {
            var handles = instances.Select(instance => instance.InteropHandle).ToArray();
            var byteCount = checked(sizeof(ulong) + handles.Length * sizeof(nuint));
            var buffer = Marshal.AllocHGlobal(byteCount);
            tempPtrs[tempPtrCount++] = buffer;

            // The native ABI uses an eight-byte count header so the handle array
            // remains pointer-aligned on both 32-bit and 64-bit hosts.
            *(ulong*)buffer = checked((uint)handles.Length);
            var destination = (nuint*)((byte*)buffer + sizeof(ulong));
            for (var i = 0; i < handles.Length; i++)
            {
                destination[i] = handles[i];
            }

            return InteropVariant.FromInstanceArray((nuint)buffer);
        }

        private static InteropVariant BuildFallbackVariant(object arg, nint* tempPtrs, ref int tempPtrCount)
        {
            var type = arg.GetType();
            if (type.IsValueType && !type.IsPrimitive && !type.IsEnum)
            {
                try
                {
                    var size = Marshal.SizeOf(arg);
                    var p = Marshal.AllocHGlobal(size);
                    Marshal.StructureToPtr(arg, p, false);
                    tempPtrs[tempPtrCount++] = p;
                    return InteropVariant.FromBlittable((nuint)p);
                }
                catch (ArgumentException)
                {
                }
            }
            else if (arg is IConvertible)
            {
                try
                {
                    return InteropVariant.FromInt64(Convert.ToInt64(arg));
                }
                catch (Exception ex) when (ex is InvalidCastException or FormatException or OverflowException)
                {
                }
            }

            throw new NotSupportedException($"Cannot marshal argument of type {type}");
        }

        private static InteropVariant BuildStringVariant(string s, nint* tempPtrs, ref int tempPtrCount)
        {
            var bytes = Encoding.UTF8.GetBytes(s);
            var p = Marshal.AllocHGlobal(bytes.Length + 1);
            Marshal.Copy(bytes, 0, p, bytes.Length);
            Marshal.WriteByte(p + bytes.Length, 0);
            tempPtrs[tempPtrCount++] = p;
            return InteropVariant.FromString((nuint)(ulong)p);
        }
    }
}
