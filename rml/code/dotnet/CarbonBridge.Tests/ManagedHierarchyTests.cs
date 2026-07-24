using System.Buffers.Binary;
using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;

using Xunit;

namespace Carbon.RmlBridge.Tests;

public sealed class ManagedHierarchyTests
{
    [Fact]
    public void RepeatedCaptureOwnershipRootsDoNotTriggerRebuild()
    {
        var roots = new HashSet<string>(StringComparer.Ordinal) { "mapped-a", "mapped-b" };

        Assert.False(CarbonBridgeMod.UpdateManagedObservationRoots(
            roots,
            ["mapped-b", "mapped-a"],
            replace: true));
        Assert.False(CarbonBridgeMod.UpdateManagedObservationRoots(
            roots,
            ["mapped-a"],
            replace: false));
        Assert.True(CarbonBridgeMod.UpdateManagedObservationRoots(
            roots,
            ["mapped-c"],
            replace: false));
        Assert.True(CarbonBridgeMod.UpdateManagedObservationRoots(
            roots,
            ["mapped-b"],
            replace: true));
        Assert.Equal(["mapped-b"], roots);
    }

    [Fact]
    public void CaptureExcludesOnlyDirectInternalRootsWithNoPublicClass()
    {
        Assert.True(ManagedHierarchy.IsInternalDataModelRoot(""));
        Assert.True(ManagedHierarchy.IsInternalDataModelRoot(null));
        Assert.False(ManagedHierarchy.IsInternalDataModelRoot("Instance"));
    }

    [Theory]
    [InlineData(ManagedHierarchy.RuntimeSerializable, true, true)]
    [InlineData(ManagedHierarchy.RuntimeArchivable, true, false)]
    [InlineData(ManagedHierarchy.RuntimePersistent, true, true)]
    [InlineData(ManagedHierarchy.RuntimeSerializable, false, false)]
    [InlineData(ManagedHierarchy.RuntimePersistent, false, true)]
    public void CaptureServiceShellIgnoresArchivableButDescendantsDoNot(
        byte persistenceFlags,
        bool isServiceShell,
        bool expected)
    {
        Assert.Equal(
            expected,
            CarbonBridgeMod.HasCapturePersistence(persistenceFlags, isServiceShell));
    }

    [Theory]
    [InlineData(true, false, true, true)]
    [InlineData(false, false, true, false)]
    [InlineData(true, true, true, false)]
    [InlineData(true, false, false, false)]
    public void CaptureOmitsOnlyEmptyUnmodifiedServicesFromTheLaunchHydrationBaseline(
        bool launchHydrated,
        bool hasPersistentChildren,
        bool matchesCurrentDefaults,
        bool expected)
    {
        Assert.Equal(
            expected,
            CarbonBridgeMod.ShouldOmitDefaultHydratedService(
                launchHydrated,
                hasPersistentChildren,
                matchesCurrentDefaults));
    }

    [Fact]
    public void UniqueClassNameIndexRequiresOneGlobalSourceCandidate()
    {
        var source = new[]
        {
            new ManagedSourceNode("root", "", "DataModel", "Place"),
            new ManagedSourceNode("shared", "root", "Folder", "Shared", ParentIndex: 0),
            new ManagedSourceNode("example", "shared", "ModuleScript", "Example", ParentIndex: 1),
            new ManagedSourceNode("other", "root", "Folder", "Other", ParentIndex: 0),
        };

        Assert.Equal(2, ManagedHierarchy.UniqueClassNameIndex(source, "ModuleScript", "Example"));
        Assert.Equal(-1, ManagedHierarchy.UniqueClassNameIndex(source, "Part", "Missing"));
    }

    [Fact]
    public void UniqueClassNameIndexRejectsDuplicatesAcrossDifferentParents()
    {
        var source = new[]
        {
            new ManagedSourceNode("root", "", "DataModel", "Place"),
            new ManagedSourceNode("first", "root", "Folder", "First", ParentIndex: 0),
            new ManagedSourceNode("first-child", "first", "ModuleScript", "Duplicate", ParentIndex: 1),
            new ManagedSourceNode("second", "root", "Folder", "Second", ParentIndex: 0),
            new ManagedSourceNode("second-child", "second", "ModuleScript", "Duplicate", ParentIndex: 3),
        };

        Assert.Equal(-1, ManagedHierarchy.UniqueClassNameIndex(source, "ModuleScript", "Duplicate"));
    }

    private static byte[] RuntimePayload(params (ulong Handle, uint Parent, string ClassName, string Name)[] nodes) =>
        RuntimePayloadV5(
            nodes.Select((node, index) => (
                node.Handle,
                node.Parent,
                index == 0 ? ManagedHierarchy.RuntimeArchivable : ManagedHierarchy.RuntimePersistent,
                node.ClassName,
                node.Name)).ToArray(),
            []);

	private static byte[] RuntimePayloadV5(
		(ulong Handle, uint Parent, byte Flags, string ClassName, string Name)[] nodes,
		(uint Owner, ulong Target, string Property)[] references,
		params (uint Owner, string Property)[] contentObjects)
	{
		using var output = new MemoryStream();
		using var writer = new BinaryWriter(output, Encoding.UTF8, true);
		writer.Write("RMLHIER5"u8);
		writer.Write((uint)nodes.Length);
		foreach (var node in nodes)
		{
			var className = Encoding.UTF8.GetBytes(node.ClassName);
			var name = Encoding.UTF8.GetBytes(node.Name);
			writer.Write(node.Handle);
			writer.Write(node.Parent);
			writer.Write(node.Flags);
			writer.Write((ushort)className.Length);
			writer.Write((uint)name.Length);
			writer.Write(className);
			writer.Write(name);
		}
		writer.Write((uint)references.Length);
		foreach (var reference in references)
		{
			var property = Encoding.UTF8.GetBytes(reference.Property);
			writer.Write(reference.Owner);
			writer.Write(reference.Target);
			writer.Write((ushort)property.Length);
			writer.Write(property);
		}
		writer.Write((uint)contentObjects.Length);
		foreach (var contentObject in contentObjects)
		{
			var property = Encoding.UTF8.GetBytes(contentObject.Property);
			writer.Write(contentObject.Owner);
			writer.Write((ushort)property.Length);
			writer.Write(property);
		}
		return output.ToArray();
	}

    private static byte[] GroupedParentIndexedShapedPayload(params ManagedSourceNode[] nodes)
    {
        using var output = new MemoryStream();
        using var writer = new BinaryWriter(output, Encoding.UTF8, true);
        writer.Write("CARBONID4"u8);
        writer.Write((uint)nodes.Length);
        foreach (var node in nodes)
        {
            writer.Write(Convert.FromHexString(node.SourceId));
            writer.Write(node.ParentIndex < 0 ? uint.MaxValue : checked((uint)node.ParentIndex));
            writer.Write(checked((uint)node.ShapeId));
            writer.Write(checked((byte)node.ChildShapeMode));
            var className = Encoding.UTF8.GetBytes(node.ClassName);
            var name = Encoding.UTF8.GetBytes(node.Name);
            writer.Write((ushort)className.Length);
            writer.Write((uint)name.Length);
            writer.Write(className);
            writer.Write(name);
        }
        return output.ToArray();
    }

    [Fact]
    public void CompactHierarchyRoundTripsWithoutUniqueIdMetadata()
    {
        var root = new ManagedSourceNode(
            "00000000000000000000000000000001",
            "",
            "DataModel",
            "Place",
            ShapeId: 1,
            ChildShapeMode: 0);
        var child = new ManagedSourceNode(
            "00000000000000000000000000000002",
            root.SourceId,
            "Folder",
            "Gameplay",
            ParentIndex: 0,
            ShapeId: 0,
            ChildShapeMode: 0);

        Assert.Equal(
            [root, child],
            ManagedHierarchy.Parse(GroupedParentIndexedShapedPayload(root, child)));
    }

    [Fact]
    public void CompactHierarchyRestoresPrecomputedCanonicalShapeIds()
    {
        var root = new ManagedSourceNode(
            "00000000000000000000000000000001",
            "",
            "DataModel",
            "Place",
            ShapeId: 1);
        var child = new ManagedSourceNode(
            "00000000000000000000000000000002",
            root.SourceId,
            "Folder",
            "Gameplay",
            ParentIndex: 0,
            ShapeId: 0);

        var groupedRoot = root with { ChildShapeMode = 0 };
        var groupedChild = child with { ChildShapeMode = 0 };
        Assert.Equal(
            [groupedRoot, groupedChild],
            ManagedHierarchy.Parse(GroupedParentIndexedShapedPayload(groupedRoot, groupedChild)));
    }

    [Fact]
    public void ParentIndexedHierarchyRejectsAForwardParent()
    {
        var root = new ManagedSourceNode(
            "00000000000000000000000000000001",
            "",
            "DataModel",
            "Place",
            ShapeId: 0);
        var payload = GroupedParentIndexedShapedPayload(root with { ChildShapeMode = 0 });
        BitConverter.GetBytes(0u).CopyTo(payload, 12 + 16);

        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.Parse(payload));
    }

    [Fact]
    public void CompactNativeHierarchyRestoresParentDebugIdsAndHandles()
    {
        var payload = RuntimePayload(
            (0x1000, uint.MaxValue, "DataModel", "Place"),
            (0x2000, 0, "Workspace", "Workspace"),
            (0x3000, 1, "Terrain", "Terrain"));

        Assert.Equal(
            [
                new ManagedRuntimeNode(
                    "native:1000", "", "DataModel", "Place", 0x1000, -1,
                    ManagedHierarchy.RuntimeArchivable),
                new ManagedRuntimeNode(
                    "native:2000", "native:1000", "Workspace", "Workspace", 0x2000, 0,
                    ManagedHierarchy.RuntimePersistent),
                new ManagedRuntimeNode(
                    "native:3000", "native:2000", "Terrain", "Terrain", 0x3000, 1,
                    ManagedHierarchy.RuntimePersistent),
            ],
            ManagedHierarchy.ParseRuntime(payload));
        Assert.True(ManagedHierarchy.TryParseRuntimeIdentity("native:2000", out var handle));
        Assert.Equal((nuint)0x2000, handle);
        Assert.False(ManagedHierarchy.TryParseRuntimeIdentity("native:02000", out _));
        Assert.False(ManagedHierarchy.TryParseRuntimeIdentity("0_2000", out _));
    }

    [Fact]
    public void NativeHierarchyCarriesSerializableAndArchivableFlags()
    {
        var runtime = ManagedHierarchy.ParseRuntime(RuntimePayloadV5(
            [
                (0x1000, uint.MaxValue, ManagedHierarchy.RuntimeArchivable, "DataModel", "Place"),
                (0x2000, 0, ManagedHierarchy.RuntimePersistent, "Workspace", "Workspace"),
                (0x3000, 1, ManagedHierarchy.RuntimeSerializable, "Folder", "NotArchivable"),
            ],
            []));

        Assert.Equal(ManagedHierarchy.RuntimeArchivable, runtime[0].PersistenceFlags);
        Assert.Equal(ManagedHierarchy.RuntimePersistent, runtime[1].PersistenceFlags);
        Assert.Equal(ManagedHierarchy.RuntimeSerializable, runtime[2].PersistenceFlags);
        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.ParseRuntime(RuntimePayloadV5(
            [(0x1000, uint.MaxValue, 0x80, "DataModel", "Place")],
            [])));
    }

    [Fact]
    public void NativeHierarchyCarriesReferenceOwnersPropertiesAndTargets()
    {
        var payload = ManagedHierarchy.ParseRuntimePayload(RuntimePayloadV5(
            [
                (0x1000, uint.MaxValue, ManagedHierarchy.RuntimeArchivable, "DataModel", "Place"),
                (0x2000, 0, ManagedHierarchy.RuntimePersistent, "Workspace", "Workspace"),
                (0x3000, 1, ManagedHierarchy.RuntimePersistent, "ObjectValue", "Owner"),
            ],
            [
                (2, 0x2000, "Value"),
                (1, 0, "CurrentTarget"),
            ]));

        Assert.Equal(3, payload.Nodes.Count);
        Assert.Equal(
            [
                new ManagedRuntimeReference(2, "Value", 0x2000),
                new ManagedRuntimeReference(1, "CurrentTarget", 0),
            ],
            payload.References);
        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.ParseRuntimePayload(RuntimePayloadV5(
            [(0x1000, uint.MaxValue, ManagedHierarchy.RuntimeArchivable, "DataModel", "Place")],
            [(1, 0, "Outside")])));
    }

    [Fact]
    public void CaptureHierarchyUsesDenseInternedNodesAndSortedValidation()
    {
        var payload = ManagedHierarchy.ParseCaptureRuntimePayload(RuntimePayloadV5(
            [
                (0x1000, uint.MaxValue, ManagedHierarchy.RuntimeArchivable, "DataModel", "Place"),
                (0x2000, 0, ManagedHierarchy.RuntimePersistent, "Folder", "First"),
                (0x3000, 0, ManagedHierarchy.RuntimePersistent, "Folder", "Second"),
            ],
			[
				(2, 0x2000, "Value"),
				(1, 0, "Value"),
			],
			(2, "ImageContent")));

        Assert.Equal(3, payload.Nodes.Length);
        Assert.Equal(new CaptureRuntimeNode(0x2000, 0, "Folder", "First", ManagedHierarchy.RuntimePersistent),
            payload.Nodes[1]);
        Assert.Same(payload.Nodes[1].ClassName, payload.Nodes[2].ClassName);
        Assert.Same(payload.References[0].Property, payload.References[1].Property);
        Assert.Equal(1, payload.References[0].OwnerIndex);
        Assert.Equal(2, payload.References[1].OwnerIndex);
		Assert.Equal([new CaptureRuntimeContentObject(2, "ImageContent")], payload.ContentObjects);

        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.ParseCaptureRuntimePayload(
            RuntimePayloadV5(
                [
                    (0x1000, uint.MaxValue, ManagedHierarchy.RuntimeArchivable, "DataModel", "Place"),
					(0x1000, 0, ManagedHierarchy.RuntimePersistent, "Folder", "Duplicate"),
				],
				[])));
        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.ParseCaptureRuntimePayload(
			RuntimePayloadV5(
				[(0x1000, uint.MaxValue, ManagedHierarchy.RuntimeArchivable, "DataModel", "Place")],
				[(0, 0, "Value"), (0, 0, "Value")])));
		Assert.Throws<InvalidDataException>(() => ManagedHierarchy.ParseCaptureRuntimePayload(
			RuntimePayloadV5(
				[(0x1000, uint.MaxValue, ManagedHierarchy.RuntimeArchivable, "DataModel", "Place")],
				[],
				(0, "ImageContent"),
				(0, "ImageContent"))));
    }

    [Fact]
    public void CancelledCaptureStopsBeforeDenseHierarchyPlanning()
    {
        using var cancellation = new CancellationTokenSource();
        cancellation.Cancel();
        var payload = RuntimePayloadV5(
            [
                (0x1000, uint.MaxValue, ManagedHierarchy.RuntimeArchivable, "DataModel", "Place"),
                (0x2000, 0, ManagedHierarchy.RuntimePersistent, "Workspace", "Workspace"),
			],
			[]);

        Assert.Throws<OperationCanceledException>(() =>
            ManagedHierarchy.ParseCaptureRuntimePayload(payload, cancellation.Token));
    }

    [Fact]
    public void MillionNodeCaptureParserAndEnvelopeStayInsideStandaloneMemoryGate()
    {
        if (!string.Equals(
            Environment.GetEnvironmentVariable("CARBON_RUN_MILLION_CAPTURE_TEST"),
            "1",
            StringComparison.Ordinal))
        {
            return;
        }

        const int nodeCount = 1_000_000;
        const long memoryGate = 512L * 1024 * 1024;
        GC.Collect(GC.MaxGeneration, GCCollectionMode.Aggressive, blocking: true, compacting: true);
        var allocatedBefore = GC.GetTotalAllocatedBytes(precise: true);
        var timer = Stopwatch.StartNew();
        var payload = MillionCapturePayload(nodeCount);
        var runtime = ManagedHierarchy.ParseCaptureRuntimePayload(payload);
        var envelopeNodes = new CaptureHierarchyNode[nodeCount];
        for (var index = 0; index < envelopeNodes.Length; index++)
        {
            var node = runtime.Nodes[index];
            envelopeNodes[index] = new(
                index == 0 ? CaptureEnvelope.NoParent : checked((uint)(index - 1)),
                node.ClassName,
                node.Name,
                index == 0 ? 2u : 1u);
        }
        var envelope = new CaptureEnvelopeData(
            "00112233445566778899aabbccddeeff",
            1,
            "million-source",
            1,
            1,
            1,
            1,
            "million-session",
            "million-instance",
            "102132435465768798a9bacbdcedfe0f",
            "million-reflection",
            envelopeNodes,
            Array.Empty<CaptureServiceRoot>(),
            Array.Empty<CaptureMappedBinding>(),
            Array.Empty<CaptureExternalReference>(),
            Array.Empty<CaptureShellProperty>(),
            Array.Empty<CaptureShellCarrier>(),
            Array.Empty<uint>(),
            false,
            Enumerable.Range(1, nodeCount)
                .Select(index => ManifestIdentity.Parse(index.ToString("x32")))
                .ToArray());
        var path = Path.Combine(Path.GetTempPath(), $"carbon-million-{Guid.NewGuid():N}.envelope");
        long envelopeBytes;
        try
        {
            using var output = new FileStream(
                path,
                FileMode.CreateNew,
                FileAccess.Write,
                FileShare.None,
                1024 * 1024,
                FileOptions.SequentialScan);
            CaptureEnvelope.Write(
                output,
                envelope,
                modelLength: 0,
                SHA256.HashData(Array.Empty<byte>()));
            output.Flush();
            envelopeBytes = output.Length;
        }
        finally
        {
            File.Delete(path);
        }
        timer.Stop();

        var process = Process.GetCurrentProcess();
        var peakWorkingSet = process.PeakWorkingSet64;
        var managedLive = GC.GetTotalMemory(forceFullCollection: false);
        var allocated = GC.GetTotalAllocatedBytes(precise: true) - allocatedBefore;
        Console.WriteLine(
            $"million capture nodes={nodeCount} payload={payload.LongLength} " +
            $"envelope={envelopeBytes} elapsedMs={timer.ElapsedMilliseconds} " +
            $"managedLive={managedLive} allocated={allocated} peakWorkingSet={peakWorkingSet}");
        Assert.Equal(nodeCount, runtime.Nodes.Length);
        Assert.True(
            peakWorkingSet < memoryGate,
            $"standalone million capture peak {peakWorkingSet} exceeded {memoryGate}");
    }

    private static byte[] MillionCapturePayload(int count)
    {
        const int recordBytes = sizeof(ulong) + sizeof(uint) + sizeof(byte)
            + sizeof(ushort) + sizeof(uint) + 6 + 8;
		var payload = new byte[checked(8 + sizeof(uint) + count * recordBytes + 2 * sizeof(uint))];
		"RMLHIER5"u8.CopyTo(payload);
        BinaryPrimitives.WriteUInt32LittleEndian(payload.AsSpan(8), checked((uint)count));
        var offset = 8 + sizeof(uint);
        for (var index = 0; index < count; index++)
        {
            BinaryPrimitives.WriteUInt64LittleEndian(
                payload.AsSpan(offset),
                checked((ulong)index + 1));
            offset += sizeof(ulong);
            BinaryPrimitives.WriteUInt32LittleEndian(
                payload.AsSpan(offset),
                index == 0 ? uint.MaxValue : checked((uint)(index - 1)));
            offset += sizeof(uint);
            payload[offset++] = ManagedHierarchy.RuntimePersistent;
            BinaryPrimitives.WriteUInt16LittleEndian(payload.AsSpan(offset), 6);
            offset += sizeof(ushort);
            BinaryPrimitives.WriteUInt32LittleEndian(payload.AsSpan(offset), 8);
            offset += sizeof(uint);
            "Folder"u8.CopyTo(payload.AsSpan(offset));
            offset += 6;
            payload[offset] = (byte)'N';
            var value = index;
            for (var digit = 7; digit > 0; digit--)
            {
                payload[offset + digit] = checked((byte)('0' + value % 10));
                value /= 10;
            }
            offset += 8;
        }
        BinaryPrimitives.WriteUInt32LittleEndian(payload.AsSpan(offset), 0);
		offset += sizeof(uint);
		BinaryPrimitives.WriteUInt32LittleEndian(payload.AsSpan(offset), 0);
        return payload;
    }

    [Fact]
    public void NativeHierarchyParserRejectsTruncationTrailingDataAndForwardParents()
    {
        var payload = RuntimePayload((0x1000, uint.MaxValue, "DataModel", "Place"));
        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.ParseRuntime(payload[..^1]));
        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.ParseRuntime([.. payload, 0]));
        "RMLHIER4"u8.CopyTo(payload);
        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.ParseRuntime(payload));
        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.ParseRuntime(
            RuntimePayload(
                (0x1000, uint.MaxValue, "DataModel", "Place"),
                (0x2000, 1, "Workspace", "Workspace"))));
    }

    [Fact]
    public void RuntimeNormalizationMatchesTheExactSourceHierarchyContract()
    {
        var runtime = ManagedHierarchy.ParseRuntime(RuntimePayload(
            (0x1000, uint.MaxValue, "DataModel", "Place"),
            (0x1100, 0, "Model", "Character"),
            (0x1200, 1, "Humanoid", "Humanoid"),
            (0x1300, 2, "Status", "Status"),
            (0x1400, 3, "Folder", "Excluded descendant"),
            (0x1500, 0, "Folder", "Authored"),
            (0x1600, 5, "Status", "Status"),
            (0x1700, 0, "ConfigureServerService", "ConfigureServerService"),
            (0x1800, 7, "Folder", "Excluded service descendant"),
            (0x1900, 5, "ConfigureServerService", "ConfigureServerService"),
            (0x2000, 1, "Part", "Head"),
            (0x2100, 10, "Weld", "HeadWeld"),
            (0x2200, 1, "Accessory", "Hat"),
            (0x2300, 12, "Part", "Handle"),
            (0x2400, 13, "Weld", "AccessoryWeld"),
            (0x2500, 13, "RigidConstraint", "AccessoryRigidConstraint"),
            (0x2600, 5, "Part", "Head"),
            (0x2700, 16, "Weld", "HeadWeld"),
            (0x2800, 5, "Part", "Handle"),
            (0x2900, 18, "Weld", "AccessoryWeld")));

        var normalized = ManagedHierarchy.NormalizeRuntime(
            runtime,
            node => node.Handle == 0x2100 ? 0x2300u : 0);

        Assert.DoesNotContain(normalized, node => node.Handle is
            0x1300 or 0x1400 or 0x1700 or 0x1800 or 0x2100 or 0x2400);
        Assert.Contains(normalized, node => node.Handle == 0x1600);
        Assert.Contains(normalized, node => node.Handle == 0x1900);
        Assert.Contains(normalized, node => node.Handle == 0x2700);
        Assert.Contains(normalized, node => node.Handle == 0x2900);
        Assert.All(normalized.Skip(1), node =>
        {
            Assert.InRange(node.ParentIndex, 0, normalized.Count - 1);
            Assert.Equal(normalized[node.ParentIndex].DebugId, node.ParentDebugId);
        });
    }

    [Fact]
    public void RuntimeNormalizationPreservesNearCollisionWelds()
    {
        var runtime = ManagedHierarchy.ParseRuntime(RuntimePayload(
            (0x1000, uint.MaxValue, "DataModel", "Place"),
            (0x1100, 0, "Part", "Head"),
            (0x1200, 1, "Weld", "HeadWeld"),
            (0x1300, 0, "Accessory", "Hat"),
            (0x1400, 3, "Part", "Handle"),
            (0x1500, 4, "Weld", "AccessoryWeld"),
            (0x1600, 4, "RigidConstraint", "DifferentName")));

        Assert.Equal(
            runtime,
            ManagedHierarchy.NormalizeRuntime(runtime, _ => 0x1100));
    }

    [Fact]
    public void MatcherIgnoresRuntimeOnlyInstancesAndPreservesDuplicateSiblingOccurrence()
    {
        var rootId = "00000000000000000000000000000001";
        var firstId = "00000000000000000000000000000002";
        var secondId = "00000000000000000000000000000003";
        var source = new[]
        {
            new ManagedSourceNode(rootId, "", "DataModel", "Place"),
            new ManagedSourceNode(firstId, rootId, "Folder", "Duplicate"),
            new ManagedSourceNode(secondId, rootId, "Folder", "Duplicate"),
        };
        var runtime = new[]
        {
            new ManagedRuntimeNode("game", "", "DataModel", "Different runtime name"),
            new ManagedRuntimeNode("camera", "game", "Camera", "Camera"),
            new ManagedRuntimeNode("first", "game", "Folder", "Duplicate"),
            new ManagedRuntimeNode("second", "game", "Folder", "Duplicate"),
        };

        var bindings = ManagedHierarchy.Match(source, runtime, "game");

        Assert.Equal("game", bindings[0].DebugId);
        Assert.Equal("first", bindings[1].DebugId);
        Assert.Equal("second", bindings[2].DebugId);
        Assert.All(bindings.Skip(1), binding => Assert.Equal(binding.SourceId, binding.RootSourceId));
    }

    [Fact]
    public void IndexedMatcherDoesNotTreatTheDataModelDisplayNameAsPersistentIdentity()
    {
        var rootId = "00000000000000000000000000000001";
        var childId = "00000000000000000000000000000002";
        var source = new[]
        {
            new ManagedSourceNode(rootId, "", "DataModel", "SourceName"),
            new ManagedSourceNode(childId, rootId, "Folder", "Gameplay", ParentIndex: 0),
        };
        var runtime = new[]
        {
            new ManagedRuntimeNode("game", "", "DataModel", "RuntimeName"),
            new ManagedRuntimeNode("folder", "game", "Folder", "Gameplay", ParentIndex: 0),
        };
        var strategy = string.Empty;

        var bindings = ManagedHierarchy.Match(source, runtime, "game", value => strategy = value);

        Assert.Equal("folder", bindings[1].DebugId);
        Assert.StartsWith("indexed parent occurrence", strategy, StringComparison.Ordinal);
    }

    [Fact]
    public void MatcherFailsClosedWhenARequiredSourceNodeIsMissing()
    {
        var rootId = "00000000000000000000000000000001";
        var source = new[]
        {
            new ManagedSourceNode(rootId, "", "DataModel", "Place"),
            new ManagedSourceNode("00000000000000000000000000000002", rootId, "Script", "Required"),
        };
        var runtime = new[]
        {
            new ManagedRuntimeNode("game", "", "DataModel", "Place"),
        };

        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.Match(source, runtime, "game"));
    }

    [Fact]
    public void MatcherFailsClosedWhenARuntimeOnlyNodeCollidesWithAnAuthoredIdentity()
    {
        var rootId = "00000000000000000000000000000001";
        var workspaceId = "00000000000000000000000000000002";
        var source = new[]
        {
            new ManagedSourceNode(rootId, "", "DataModel", "Place"),
            new ManagedSourceNode(workspaceId, rootId, "Workspace", "Workspace"),
            new ManagedSourceNode("00000000000000000000000000000003", workspaceId, "Camera", "Camera"),
        };
        var runtime = new[]
        {
            new ManagedRuntimeNode("game", "", "DataModel", "Place"),
            new ManagedRuntimeNode("workspace", "game", "Workspace", "Workspace"),
            new ManagedRuntimeNode("edit-camera", "workspace", "Camera", "Camera"),
            new ManagedRuntimeNode("authored-camera", "workspace", "Camera", "Camera"),
        };

        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.Match(source, runtime, "game"));
    }

    [Fact]
    public void MatcherDisambiguatesReorderedDuplicateSiblingsByTheirUnorderedSubtrees()
    {
        var rootId = "00000000000000000000000000000001";
        var folderModelId = "00000000000000000000000000000002";
        var folderChildId = "00000000000000000000000000000003";
        var scriptModelId = "00000000000000000000000000000004";
        var scriptChildId = "00000000000000000000000000000005";
        var source = new[]
        {
            new ManagedSourceNode(rootId, "", "DataModel", "Place", ShapeId: 4, ChildShapeMode: 1),
            new ManagedSourceNode(folderModelId, rootId, "Model", "Duplicate", ParentIndex: 0, ShapeId: 2),
            new ManagedSourceNode(folderChildId, folderModelId, "Folder", "Gameplay", ParentIndex: 1, ShapeId: 0),
            new ManagedSourceNode(scriptModelId, rootId, "Model", "Duplicate", ParentIndex: 0, ShapeId: 3),
            new ManagedSourceNode(scriptChildId, scriptModelId, "Script", "Main", ParentIndex: 3, ShapeId: 1),
        };
        var runtime = new[]
        {
            new ManagedRuntimeNode("game", "", "DataModel", "Place"),
            new ManagedRuntimeNode("runtime-script", "game", "Model", "Duplicate", ParentIndex: 0),
            new ManagedRuntimeNode("runtime-folder", "game", "Model", "Duplicate", ParentIndex: 0),
            new ManagedRuntimeNode("runtime-script-child", "runtime-script", "Script", "Main", ParentIndex: 1),
            new ManagedRuntimeNode("runtime-only", "runtime-script", "Folder", "RuntimeOnly", ParentIndex: 1),
            new ManagedRuntimeNode("runtime-folder-child", "runtime-folder", "Folder", "Gameplay", ParentIndex: 2),
        };
        var strategy = string.Empty;

        var bindings = ManagedHierarchy.Match(
                source,
                runtime,
                "game",
                value => strategy = value,
                ManagedHierarchy.PrecomputeRuntimeShapes(runtime))
            .ToDictionary(binding => binding.SourceId, StringComparer.Ordinal);

        Assert.StartsWith("indexed parent occurrence", strategy, StringComparison.Ordinal);
        Assert.Equal("runtime-folder", bindings[folderModelId].DebugId);
        Assert.Equal("runtime-folder-child", bindings[folderChildId].DebugId);
        Assert.Equal("runtime-script", bindings[scriptModelId].DebugId);
        Assert.Equal("runtime-script-child", bindings[scriptChildId].DebugId);
    }

    [Fact]
    public void MatcherUsesTheWholeDuplicateGroupToResolveSubsetCompatibleShapes()
    {
        var rootId = "00000000000000000000000000000001";
        var leafId = "00000000000000000000000000000002";
        var richId = "00000000000000000000000000000003";
        var childId = "00000000000000000000000000000004";
        var source = new[]
        {
            new ManagedSourceNode(rootId, "", "DataModel", "Place"),
            new ManagedSourceNode(leafId, rootId, "Keyframe", "Keyframe"),
            new ManagedSourceNode(richId, rootId, "Keyframe", "Keyframe"),
            new ManagedSourceNode(childId, richId, "Pose", "Root"),
        };
        var runtime = new[]
        {
            new ManagedRuntimeNode("game", "", "DataModel", "Place"),
            new ManagedRuntimeNode("runtime-rich", "game", "Keyframe", "Keyframe"),
            new ManagedRuntimeNode("runtime-leaf", "game", "Keyframe", "Keyframe"),
            new ManagedRuntimeNode("runtime-pose", "runtime-rich", "Pose", "Root"),
        };

        var bindings = ManagedHierarchy.Match(source, runtime, "game")
            .ToDictionary(binding => binding.SourceId, StringComparer.Ordinal);

        Assert.Equal("runtime-leaf", bindings[leafId].DebugId);
        Assert.Equal("runtime-rich", bindings[richId].DebugId);
        Assert.Equal("runtime-pose", bindings[childId].DebugId);
    }

    [Fact]
    public void MatcherFailsClosedWhenSubsetCompatibleDuplicateShapesRemainAmbiguous()
    {
        var rootId = "00000000000000000000000000000001";
        var leafId = "00000000000000000000000000000002";
        var richId = "00000000000000000000000000000003";
        var childId = "00000000000000000000000000000004";
        var source = new[]
        {
            new ManagedSourceNode(rootId, "", "DataModel", "Place", ParentIndex: -1),
            new ManagedSourceNode(leafId, rootId, "Keyframe", "Keyframe", ParentIndex: 0),
            new ManagedSourceNode(richId, rootId, "Keyframe", "Keyframe", ParentIndex: 0),
            new ManagedSourceNode(childId, richId, "Pose", "Root", ParentIndex: 2),
        };
        var runtime = new[]
        {
            new ManagedRuntimeNode("game", "", "DataModel", "Place", ParentIndex: -1),
            new ManagedRuntimeNode("runtime-rich-1", "game", "Keyframe", "Keyframe", ParentIndex: 0),
            new ManagedRuntimeNode("runtime-rich-2", "game", "Keyframe", "Keyframe", ParentIndex: 0),
            new ManagedRuntimeNode("runtime-pose-1", "runtime-rich-1", "Pose", "Root", ParentIndex: 1),
            new ManagedRuntimeNode("runtime-pose-2", "runtime-rich-2", "Pose", "Root", ParentIndex: 2),
        };

        Assert.Throws<InvalidDataException>(() =>
            ManagedHierarchy.Match(source, runtime, "game"));
    }

    [Fact]
    public void PreVerificationPolicyAllowsRuntimeOnlyAdditionsAndTheirInitializationProperties()
    {
        ManagedHierarchy.ValidatePreVerificationChanges(
        [
            new ManagedHierarchyChange("Add", "stats-item", "stats"),
            new ManagedHierarchyChange("Property", "stats-item", "stats"),
            new ManagedHierarchyChange("Add", "stats-child", "stats"),
            new ManagedHierarchyChange("Property", "stats-child", "stats"),
        ], new HashSet<string>(StringComparer.Ordinal) { "stats" });
    }

    [Theory]
    [InlineData("stats", "workspace")]
    [InlineData("workspace", "stats")]
    public void PreVerificationPolicyRejectsPropertiesUnlessTheSameNodeWasAddedUnderTheSameRuntimeOnlyRoot(
        string addedRootDebugId,
        string propertyRootDebugId)
    {
        Assert.Throws<InvalidOperationException>(() => ManagedHierarchy.ValidatePreVerificationChanges(
        [
            new ManagedHierarchyChange("Add", "stats-item", addedRootDebugId),
            new ManagedHierarchyChange("Property", "stats-item", propertyRootDebugId),
        ], new HashSet<string>(StringComparer.Ordinal) { "stats" }));
    }

    [Fact]
    public void PreVerificationPolicyRejectsAPropertyBeforeItsRuntimeOnlyAdd()
    {
        Assert.Throws<InvalidOperationException>(() => ManagedHierarchy.ValidatePreVerificationChanges(
        [
            new ManagedHierarchyChange("Property", "stats-item", "stats"),
            new ManagedHierarchyChange("Add", "stats-item", "stats"),
        ], new HashSet<string>(StringComparer.Ordinal) { "stats" }));
    }

    [Fact]
    public void SourceRootMatchesReturnOnlyDirectDataModelChildren()
    {
        var source = new[]
        {
            new ManagedSourceNode("source-game", "", "DataModel", "Place", ParentIndex: -1),
            new ManagedSourceNode("source-workspace", "source-game", "Workspace", "Workspace", ParentIndex: 0),
            new ManagedSourceNode("source-folder", "source-workspace", "Folder", "Folder", ParentIndex: 1),
            new ManagedSourceNode("source-storage", "source-game", "ServerStorage", "ServerStorage", ParentIndex: 0),
        };
        var matches = new[]
        {
            new ManagedHierarchyMatch("source-game", "native:1", "source-game", "native:1"),
            new ManagedHierarchyMatch("source-workspace", "native:2", "source-workspace", "native:2"),
            new ManagedHierarchyMatch("source-folder", "native:3", "source-workspace", "native:2"),
            new ManagedHierarchyMatch("source-storage", "native:4", "source-storage", "native:4"),
        };

        Assert.Equal(
            [matches[1], matches[3]],
            ManagedHierarchy.SourceRootMatches(source, matches));
    }

    [Theory]
    [InlineData("Add", "workspace-child", "workspace")]
    [InlineData("Add", "core-gui", "core-gui")]
    [InlineData("Remove", "plugin-ui", "core-gui")]
    [InlineData("Rename", "plugin-ui", "core-gui")]
    [InlineData("Remove", "workspace-child", "workspace")]
    [InlineData("Rename", "workspace-child", "workspace")]
    [InlineData("Property", "workspace-child", "workspace")]
    [InlineData("Property", "plugin-ui", "core-gui")]
    [InlineData("add", "plugin-ui", "core-gui")]
    [InlineData("Unknown", "plugin-ui", "core-gui")]
    public void PreVerificationPolicyFailsClosedOutsideRuntimeOnlyAdditions(
        string kind,
        string debugId,
        string rootDebugId)
    {
        Assert.Throws<InvalidOperationException>(() => ManagedHierarchy.ValidatePreVerificationChanges(
        [
            new ManagedHierarchyChange(kind, debugId, rootDebugId),
        ], new HashSet<string>(StringComparer.Ordinal) { "core-gui" }));
    }

    [Fact]
    public void PreVerificationFailureIdentifiesTheExactMutationAndRoot()
    {
        var error = Assert.Throws<InvalidOperationException>(() =>
            ManagedHierarchy.ValidatePreVerificationChanges(
            [
                new ManagedHierarchyChange(
                    "Add",
                    "child-id",
                    "workspace-id",
                    "Folder",
                    "LateFolder",
                    "Workspace",
                    "Workspace"),
            ], new HashSet<string>(StringComparer.Ordinal) { "core-gui" }));

        Assert.Contains(
            "Add Folder LateFolder under Workspace Workspace workspace-id",
            error.Message,
            StringComparison.Ordinal);
    }

    [Fact]
    public void PreVerificationPropertyFailureIdentifiesTheExactProperty()
    {
        var error = Assert.Throws<InvalidOperationException>(() =>
            ManagedHierarchy.ValidatePreVerificationChanges(
            [
                new ManagedHierarchyChange(
                    "Property",
                    "script-context-id",
                    "script-context-id",
                    "ScriptContext",
                    "Script Context",
                    "ScriptContext",
                    "Script Context",
                    "Capabilities"),
            ], new HashSet<string>(StringComparer.Ordinal)));

        Assert.Contains(
            "Property ScriptContext Script Context.Capabilities under " +
            "ScriptContext Script Context script-context-id",
            error.Message,
            StringComparison.Ordinal);
    }

    [Fact]
    public void PreVerificationFailureReportsEveryRejectedMutation()
    {
        var error = Assert.Throws<InvalidOperationException>(() =>
            ManagedHierarchy.ValidatePreVerificationChanges(
            [
                new ManagedHierarchyChange(
                    "Add",
                    "first-id",
                    "workspace-id",
                    "Folder",
                    "First",
                    "Workspace",
                    "Workspace"),
                new ManagedHierarchyChange(
                    "Property",
                    "second-id",
                    "workspace-id",
                    "Part",
                    "Second",
                    "Workspace",
                    "Workspace",
                    "Anchored"),
            ], new HashSet<string>(StringComparer.Ordinal)));

        Assert.Contains("Add Folder First", error.Message, StringComparison.Ordinal);
        Assert.Contains("Property Part Second.Anchored", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void RuntimeOnlyRootsRequireExactDirectKnownStudioServices()
    {
        var runtime = new[]
        {
            new ManagedRuntimeNode("game", "", "DataModel", "Place"),
            new ManagedRuntimeNode("core-gui", "game", "CoreGui", "CoreGui"),
            new ManagedRuntimeNode("nested-core-gui", "workspace", "CoreGui", "CoreGui"),
            new ManagedRuntimeNode("wrong-case", "game", "coregui", "CoreGui"),
            new ManagedRuntimeNode("renamed-core-gui", "game", "CoreGui", "Other"),
            new ManagedRuntimeNode(
                "plugin-gui",
                "game",
                "RobloxPluginGuiService",
                "RobloxPluginGuiService"),
            new ManagedRuntimeNode(
                "renamed-plugin-gui",
                "game",
                "RobloxPluginGuiService",
                "Other"),
            new ManagedRuntimeNode("workspace", "game", "Workspace", "Workspace"),
            new ManagedRuntimeNode(
                "visualization",
                "game",
                "VisualizationModeService",
                "VisualizationModeService"),
            new ManagedRuntimeNode(
                "studio-sdk",
                "game",
                "StudioSdkService",
                "StudioSdkService"),
            new ManagedRuntimeNode(
                "nested-visualization",
                "workspace",
                "VisualizationModeService",
                "VisualizationModeService"),
            new ManagedRuntimeNode(
                "renamed-studio-sdk",
                "game",
                "StudioSdkService",
                "Other"),
            new ManagedRuntimeNode("stats", "game", "Stats", "Stats"),
            new ManagedRuntimeNode("nested-stats", "workspace", "Stats", "Stats"),
            new ManagedRuntimeNode("renamed-stats", "game", "Stats", "Other"),
            new ManagedRuntimeNode("wrong-case-stats", "game", "stats", "Stats"),
        };

        Assert.Equal(
            [runtime[1], runtime[5], runtime[8], runtime[9], runtime[12]],
            ManagedHierarchy.RuntimeOnlyRoots(runtime, "game"));
    }

    [Fact]
    public void ParserRejectsTruncationAndTrailingData()
    {
        var root = new ManagedSourceNode(
            "00000000000000000000000000000001",
            "",
            "DataModel",
            "Place",
            ShapeId: 0,
            ChildShapeMode: 0);
        var payload = GroupedParentIndexedShapedPayload(root);
        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.Parse(payload[..^1]));
        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.Parse([.. payload, 0]));
        "CARBONID3"u8.CopyTo(payload);
        Assert.Throws<InvalidDataException>(() => ManagedHierarchy.Parse(payload));
    }

    [Fact]
    public void GiantDuplicateSiblingVerificationRemainsLinear()
    {
        const int instanceCount = 277_141;
        var rootId = 1.ToString("x32");
        var source = new List<ManagedSourceNode>(instanceCount)
        {
            new(rootId, "", "DataModel", "Place"),
        };
        var runtime = new List<ManagedRuntimeNode>(instanceCount)
        {
            new("game", "", "DataModel", "Place"),
        };
        for (var index = 1; index < instanceCount; index++)
        {
            source.Add(new(
                (index + 1).ToString("x32"),
                rootId,
                "Folder",
                "Duplicate",
                ParentIndex: 0));
            runtime.Add(new(
                $"debug-{index}",
                "game",
                "Folder",
                "Duplicate",
                ParentIndex: 0));
        }

        var started = System.Diagnostics.Stopwatch.StartNew();
        var bindings = ManagedHierarchy.Match(source, runtime, "game");
        started.Stop();

        Assert.Equal(instanceCount, bindings.Count);
        Assert.True(
            started.Elapsed < TimeSpan.FromSeconds(5),
            $"managed hierarchy verification took {started.Elapsed.TotalSeconds:F3}s; expected linear near-instant matching");
    }
}
