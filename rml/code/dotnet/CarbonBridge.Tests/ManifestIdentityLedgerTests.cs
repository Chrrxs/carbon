using System.Buffers.Binary;
using System.Text;

using Xunit;

namespace Carbon.RmlBridge.Tests;

public sealed class ManifestIdentityLedgerTests
{
    private static readonly ManifestIdentity Capture = ManifestIdentity.Parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    private const string Root = "00000000000000000000000000000001";
    private const string Left = "00000000000000000000000000000002";
    private const string Right = "00000000000000000000000000000003";

    [Fact]
    public void SerializedAttributesDecodeManifestIdentityAfterEverySupportedWireShape()
    {
        var cframeMatrix = new byte[49];
        var serialized = SerializeAttributes(
        [
            new("String", 0x02, StringValue("hello")),
            new("Bool", 0x03, new byte[1]),
            new("Int32", 0x04, new byte[4]),
            new("Float32", 0x05, new byte[4]),
            new("Float64", 0x06, new byte[8]),
            new("UDim", 0x09, new byte[8]),
            new("UDim2", 0x0a, new byte[16]),
            new("BrickColor", 0x0e, new byte[4]),
            new("Color3", 0x0f, new byte[12]),
            new("Vector2", 0x10, new byte[8]),
            new("Vector3", 0x11, new byte[12]),
            new("CFrameMatrix", 0x14, cframeMatrix),
            new("CFrameRotationId", 0x14, CFrameWithRotationId()),
            new("EnumItem", 0x15, Concat(StringValue("Example"), new byte[4])),
            new("NumberSequence", 0x17, SequenceValue(12)),
            new("ColorSequence", 0x19, SequenceValue(20)),
            new("NumberRange", 0x1b, new byte[8]),
            new("Rect", 0x1c, new byte[16]),
            new("Font", 0x21, Concat(new byte[3], StringValue("Family"), StringValue("Face"))),
            new(ManifestIdentityAttributeCodec.AttributeName, 0x02, StringValue(Right)),
        ]);

        var decoded = ManifestIdentityAttributeCodec.Decode(
            serialized,
            "Workspace",
            "Workspace");

        Assert.Equal(Right, decoded);
    }

    [Fact]
    public void SerializedAttributesWithoutManifestIdentityReturnNull()
    {
        Assert.Null(ManifestIdentityAttributeCodec.Decode([], "Part", "Part"));
        Assert.Null(ManifestIdentityAttributeCodec.Decode(
            SerializeAttributes([new("Other", 0x02, StringValue("value"))]),
            "Part",
            "Part"));
    }

    [Fact]
    public void ReflectedManifestIdentityRecoversAnEmptySerializedMarkerRead()
    {
        Assert.Equal(
            Right,
            ManifestIdentityAttributeCodec.DecodeWithReflectedFallback(
                [],
                () => Right,
                "ReplicatedStorage",
                "ReplicatedStorage"));
    }

    [Theory]
    [InlineData(true)]
    [InlineData("not-hex")]
    [InlineData("00000000000000000000000000000000")]
    public void ReflectedManifestIdentityFallbackRejectsInvalidValues(object invalid)
    {
        Assert.Throws<InvalidDataException>(() =>
            ManifestIdentityAttributeCodec.DecodeWithReflectedFallback(
                [],
                () => invalid,
                "ReplicatedStorage",
                "ReplicatedStorage"));
    }

    [Theory]
    [InlineData("not-hex")]
    [InlineData("00000000000000000000000000000000")]
    public void SerializedAttributesRejectMalformedManifestIdentity(string identity)
    {
        var serialized = SerializeAttributes(
        [
            new(ManifestIdentityAttributeCodec.AttributeName, 0x02, StringValue(identity)),
        ]);

        Assert.Throws<InvalidDataException>(() =>
            ManifestIdentityAttributeCodec.Decode(serialized, "Workspace", "Workspace"));
    }


    [Fact]
    public void SerializedAttributesRejectWrongMarkerTypeDuplicateAndMalformedBlobs()
    {
        var wrongType = SerializeAttributes(
        [
            new(ManifestIdentityAttributeCodec.AttributeName, 0x03, [1]),
        ]);
        var duplicate = SerializeAttributes(
        [
            new(ManifestIdentityAttributeCodec.AttributeName, 0x02, StringValue(Left)),
            new(ManifestIdentityAttributeCodec.AttributeName, 0x02, StringValue(Right)),
        ]);
        var unknownType = SerializeAttributes(
        [
            new("Unknown", 0xff, []),
            new(ManifestIdentityAttributeCodec.AttributeName, 0x02, StringValue(Right)),
        ]);
        var truncated = SerializeAttributes(
        [
            new("Other", 0x02, StringValue("value")),
        ])[..^1];
        var trailing = Concat(
            SerializeAttributes(
            [
                new(ManifestIdentityAttributeCodec.AttributeName, 0x02, StringValue(Right)),
            ]),
            [0xff]);

        foreach (var malformed in new[] { wrongType, duplicate, unknownType, truncated, trailing })
        {
            Assert.Throws<InvalidDataException>(() =>
                ManifestIdentityAttributeCodec.Decode(malformed, "Part", "Part"));
        }
    }

    [Fact]
    public void TransportMcpPlaceIdIsIgnoredOnlyWhenEveryAuthoredAttributeIsUnchanged()
    {
        const string transportUuid = "11111111-2222-3333-4444-555555555555";
        var baseline = SerializeAttributes(
        [
            new("Canonical", 0x02, StringValue("keep")),
        ]);
        var authoredMarkerBaseline = SerializeAttributes(
        [
            new("Canonical", 0x02, StringValue("keep")),
            new("__MCPPlaceId", 0x02, StringValue("authored-value")),
        ]);
        var transportOnly = SerializeAttributes(
        [
            new("__MCPPlaceId", 0x02, StringValue(transportUuid)),
            new("Canonical", 0x02, StringValue("keep")),
        ]);
        var authoredChange = SerializeAttributes(
        [
            new("Canonical", 0x02, StringValue("changed")),
            new("__MCPPlaceId", 0x02, StringValue(transportUuid)),
        ]);
        var otherTransportUuid = SerializeAttributes(
        [
            new("Canonical", 0x02, StringValue("keep")),
            new("__MCPPlaceId", 0x02, StringValue("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")),
        ]);

        Assert.True(ManifestIdentityAttributeCodec.MatchesIgnoringTransportMcpPlaceId(
            baseline,
            transportOnly));
        Assert.False(ManifestIdentityAttributeCodec.MatchesIgnoringTransportMcpPlaceId(
            authoredMarkerBaseline,
            transportOnly));
        Assert.False(ManifestIdentityAttributeCodec.MatchesIgnoringTransportMcpPlaceId(
            baseline,
            authoredChange));
        Assert.True(ManifestIdentityAttributeCodec.MatchesIgnoringTransportMcpPlaceId(
            baseline,
            otherTransportUuid));
        var invalidTransport = SerializeAttributes(
        [
            new("Canonical", 0x02, StringValue("keep")),
            new("__MCPPlaceId", 0x02, StringValue("not-a-uuid")),
        ]);
        Assert.False(ManifestIdentityAttributeCodec.MatchesIgnoringTransportMcpPlaceId(
            baseline,
            invalidTransport));
    }

    [Fact]
    public void EmitterVersionIsIgnoredOnlyWhenEveryAuthoredAttributeIsUnchanged()
    {
        var baseline = SerializeAttributes(
        [
            new("Canonical", 0x02, StringValue("keep")),
        ]);
        var normalized = SerializeAttributes(
        [
            new("Canonical", 0x02, StringValue("keep")),
            new("Emitter2D_Version", 0x06, BitConverter.GetBytes(1.26)),
        ]);
        var authoredChange = SerializeAttributes(
        [
            new("Canonical", 0x02, StringValue("changed")),
            new("Emitter2D_Version", 0x06, BitConverter.GetBytes(1.26)),
        ]);
        var wrongVersion = SerializeAttributes(
        [
            new("Canonical", 0x02, StringValue("keep")),
            new("Emitter2D_Version", 0x06, BitConverter.GetBytes(1.27)),
        ]);

        Assert.True(ManifestIdentityAttributeCodec.MatchesIgnoringEmitterVersion(
            baseline,
            normalized));
        Assert.False(ManifestIdentityAttributeCodec.MatchesIgnoringEmitterVersion(
            baseline,
            authoredChange));
        Assert.False(ManifestIdentityAttributeCodec.MatchesIgnoringEmitterVersion(
            baseline,
            wrongVersion));
    }

    [Fact]
    public void PackedIdentityRoundTripsWireBytes()
    {
        var identity = ManifestIdentity.Parse("00112233445566778899aabbccddeeff");
        Span<byte> bytes = stackalloc byte[16];
        identity.Write(bytes);

        Assert.Equal("00112233445566778899aabbccddeeff", Convert.ToHexStringLower(bytes));
        Assert.Equal(identity, ManifestIdentity.FromBytes(bytes));
    }

    [Fact]
    public void AllocatorUsesRandomPrefixBlocksAndBigEndianCounters()
    {
        var prefixes = new Queue<byte[]>(
        [
            Enumerable.Repeat((byte)0x11, ManifestIdentityAllocator.PrefixLength).ToArray(),
            Enumerable.Repeat((byte)0x22, ManifestIdentityAllocator.PrefixLength).ToArray(),
        ]);
        var allocator = new ManifestIdentityAllocator(() => prefixes.Dequeue());

        var first = allocator.Next();
        var second = allocator.Next();
        for (var counter = 2; counter < 1 << 16; counter++)
        {
            _ = allocator.Next();
        }
        var nextBlock = allocator.Next();

        Assert.Equal("11111111111111111111111111110000", first.ToString());
        Assert.Equal("11111111111111111111111111110001", second.ToString());
        Assert.Equal("22222222222222222222222222220000", nextBlock.ToString());
    }

    [Fact]
    public void AbandonedAllocationBlockIsNeverResumed()
    {
        var prefixes = new Queue<byte[]>(
        [
            Enumerable.Repeat((byte)0x11, ManifestIdentityAllocator.PrefixLength).ToArray(),
            Enumerable.Repeat((byte)0x22, ManifestIdentityAllocator.PrefixLength).ToArray(),
        ]);
        var allocator = new ManifestIdentityAllocator(() => prefixes.Dequeue());

        Assert.Equal("11111111111111111111111111110000", allocator.Next().ToString());
        allocator.AbandonBlock();

        Assert.Equal("22222222222222222222222222220000", allocator.Next().ToString());
    }

    [Fact]
    public void BootstrapRejectsDuplicateHandlesAndSourceIds()
    {
        var ledger = new ManifestIdentityLedger();

        Assert.Throws<InvalidDataException>(() => ledger.Bootstrap(
            [new(1, Root), new(1, Left)], Root, 2,
            ManifestIdentityLedger.Digest([Root, Left])));
        Assert.Throws<InvalidDataException>(() => ledger.Bootstrap(
            [new(1, Root), new(2, Root)], Root, 2,
            ManifestIdentityLedger.Digest([Root, Root])));
    }

    [Fact]
    public void BootstrapMismatchReportsTheFailedContractDimensions()
    {
        var ledger = new ManifestIdentityLedger();
        var error = Assert.Throws<InvalidDataException>(() => ledger.Bootstrap(
            [new(1, Root)],
            Root,
            2,
            ManifestIdentityLedger.Digest([Root, Left])));

        Assert.Contains("expected count 2, actual 1", error.Message, StringComparison.Ordinal);
        Assert.Contains("root present True", error.Message, StringComparison.Ordinal);
        Assert.Contains("expected digest", error.Message, StringComparison.Ordinal);
        Assert.Contains("actual", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void BootstrapRebindsEveryStudioRehydratedKindAndDescendantToCanonicalIdentity()
    {
        const string Workspace = "00000000000000000000000000000004";
        const string Character = "00000000000000000000000000000005";
        const string Humanoid = "00000000000000000000000000000006";
        const string Status = "00000000000000000000000000000007";
        const string Accessory = "00000000000000000000000000000008";
        const string Handle = "00000000000000000000000000000009";
        const string AccessoryWeld = "0000000000000000000000000000000a";
        const string Head = "0000000000000000000000000000000b";
        const string HeadWeld = "0000000000000000000000000000000c";
        const string Configure = "0000000000000000000000000000000d";
        const string ConfigureChild = "0000000000000000000000000000000e";
        const string FilteredSelection = "0000000000000000000000000000000f";
        var runtime = new CaptureRuntimeHierarchyPayload(
        [
            new(1, -1, "DataModel", "Place", ManagedHierarchy.RuntimeArchivable),
            new(2, 0, "Workspace", "Workspace", ManagedHierarchy.RuntimePersistent),
            new(3, 1, "Model", "Character", ManagedHierarchy.RuntimePersistent),
            new(4, 2, "Humanoid", "Humanoid", ManagedHierarchy.RuntimePersistent),
            new(40, 3, "Status", "Status", ManagedHierarchy.RuntimePersistent),
            new(5, 2, "Accessory", "Hat", ManagedHierarchy.RuntimePersistent),
            new(6, 5, "Part", "Handle", ManagedHierarchy.RuntimePersistent),
            new(60, 6, "Weld", "AccessoryWeld", ManagedHierarchy.RuntimePersistent),
            new(7, 2, "Part", "Head", ManagedHierarchy.RuntimePersistent),
            new(70, 8, "Weld", "HeadWeld", ManagedHierarchy.RuntimePersistent),
            new(80, 0, "ConfigureServerService", "ConfigureServerService", ManagedHierarchy.RuntimePersistent),
            new(81, 10, "Folder", "Child", ManagedHierarchy.RuntimePersistent),
            new(90, 0, "Instance", "FilteredSelection", ManagedHierarchy.RuntimePersistent),
        ],
        [new(9, "Part1", 6)],
        []);
        var bindings = ManifestIdentityBootstrapResolver.Resolve(
            runtime,
            [
                new(1, Root),
                new(2, Workspace),
                new(3, Character),
                new(4, Humanoid),
                new(5, Accessory),
                new(6, Handle),
                new(7, Head),
            ],
            [
                new(Status, Humanoid, "Status", "Status", "humanoidStatus", null),
                new(AccessoryWeld, Handle, "Weld", "AccessoryWeld", "accessoryWeld", null),
                new(HeadWeld, Head, "Weld", "HeadWeld", "headWeld", Handle),
                new(Configure, Root, "ConfigureServerService", "ConfigureServerService", "configureServerService", null),
                new(ConfigureChild, Configure, "Folder", "Child", "descendant", null),
                new(FilteredSelection, Root, "Instance", "FilteredSelection", "filteredSelection", null),
            ]);
        var ledger = new ManifestIdentityLedger();

        ledger.Bootstrap(
            bindings,
            Root,
            13,
            ManifestIdentityLedger.Digest([
                Root,
                Workspace,
                Character,
                Humanoid,
                Status,
                Accessory,
                Handle,
                AccessoryWeld,
                Head,
                HeadWeld,
                Configure,
                ConfigureChild,
                FilteredSelection,
            ]));

        Assert.Equal(Status, ledger.GetOrCreate(40));
        Assert.Equal(AccessoryWeld, ledger.GetOrCreate(60));
        Assert.Equal(HeadWeld, ledger.GetOrCreate(70));
        Assert.Equal(Configure, ledger.GetOrCreate(80));
        Assert.Equal(ConfigureChild, ledger.GetOrCreate(81));
        Assert.Equal(FilteredSelection, ledger.GetOrCreate(90));
        Assert.True(ledger.IsAuthoritative);
    }

    [Fact]
    public void BootstrapRejectsAmbiguousStudioRehydratedReplacements()
    {
        const string Handle = "00000000000000000000000000000006";
        const string Weld = "00000000000000000000000000000007";
        var runtime = new CaptureRuntimeHierarchyPayload(
        [
            new(1, -1, "DataModel", "Place", ManagedHierarchy.RuntimeArchivable),
            new(2, 0, "Workspace", "Workspace", ManagedHierarchy.RuntimePersistent),
            new(3, 1, "Accessory", "Hat", ManagedHierarchy.RuntimePersistent),
            new(4, 2, "Part", "Handle", ManagedHierarchy.RuntimePersistent),
            new(50, 3, "Weld", "AccessoryWeld", ManagedHierarchy.RuntimePersistent),
            new(51, 3, "Weld", "AccessoryWeld", ManagedHierarchy.RuntimePersistent),
        ],
        [],
        []);

        var error = Assert.Throws<InvalidDataException>(() =>
            ManifestIdentityBootstrapResolver.Resolve(
                runtime,
                [new(1, Root), new(4, Handle)],
                [new(Weld, Handle, "Weld", "AccessoryWeld", "accessoryWeld", null)]));

        Assert.Contains("matched 2 instances", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void BootstrapBindsANewCanonicalServiceWithoutAStaleTransportMarker()
    {
        const string ServerStorage = "00000000000000000000000000000004";
        var runtime = new CaptureRuntimeHierarchyPayload(
        [
            new(1, -1, "DataModel", "Place", ManagedHierarchy.RuntimeArchivable),
            new(2, 0, "ServerStorage", "ServerStorage", ManagedHierarchy.RuntimePersistent),
        ],
        [],
        []);

        var bindings = ManifestIdentityBootstrapResolver.Resolve(
            runtime,
            [new(1, Root)],
            [],
            [new(ServerStorage, "ServerStorage", "ServerStorage")]);

        Assert.Contains(new ManifestIdentityBinding(2, ServerStorage), bindings);
    }

    [Fact]
    public void DeletingEitherOfTwoStructurallyIdenticalInstancesPreservesTheSurvivor()
    {
        var ledger = new ManifestIdentityLedger();
        ledger.Bootstrap(
            [new(1, Root), new(2, Left), new(3, Right)],
            Root,
            3,
            ManifestIdentityLedger.Digest([Root, Left, Right]));

        ledger.Release(2);
        Assert.Equal(Right, ledger.GetOrCreate(3));
        Assert.Equal([Root, Right], ledger.Snapshot([1, 3]).Select(identity => identity.ToString()));

        ledger.Release(3);
        Assert.Equal([Root], ledger.Snapshot([1]).Select(identity => identity.ToString()));
    }

    [Fact]
    public void ReparentKeepsIdentityWhileCloneGetsANewIdentity()
    {
        var generated = "10000000000000000000000000000001";
        var ledger = new ManifestIdentityLedger(() => generated);
        ledger.Bootstrap(
            [new(1, Root), new(2, Left)],
            Root,
            2,
            ManifestIdentityLedger.Digest([Root, Left]));

        // Reparenting does not change the native handle.
        Assert.Equal(Left, ledger.GetOrCreate(2));
        Assert.Equal(generated, ledger.GetOrCreate(7));
        Assert.NotEqual(Left, ledger.GetOrCreate(7));
    }

    [Fact]
    public void ReleasedIdentityIsNeverReusedWhenNativeHandleIsRecycled()
    {
        var generated = new Queue<string>(
        [
            "10000000000000000000000000000001",
            Left,
            "10000000000000000000000000000002",
        ]);
        var ledger = new ManifestIdentityLedger(() => generated.Dequeue());
        ledger.Bootstrap(
            [new(1, Root), new(2, Left)],
            Root,
            2,
            ManifestIdentityLedger.Digest([Root, Left]));

        ledger.Release(2);
        Assert.Equal("10000000000000000000000000000001", ledger.GetOrCreate(2));
        ledger.Release(2);
        Assert.Equal("10000000000000000000000000000002", ledger.GetOrCreate(2));
    }

    [Fact]
    public void AdoptionRemapMakesProvisionalIdentitiesAuthoritative()
    {
        const string provisional = "10000000000000000000000000000001";
        var ledger = new ManifestIdentityLedger(() => provisional);

        Assert.Equal(provisional, ledger.GetOrCreate(9));
        Assert.False(ledger.IsAuthoritative);

        var remap = new Dictionary<ManifestIdentity, ManifestIdentity>
        {
            [ManifestIdentity.Parse(provisional)] = ManifestIdentity.Parse(Left),
        };
        ledger.ApplyRemap(Capture, remap);
        ledger.ApplyRemap(Capture, remap);
        Assert.Throws<InvalidOperationException>(() => ledger.ApplyRemap(
            ManifestIdentity.Parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            remap));

        Assert.True(ledger.IsAuthoritative);
        Assert.Equal(Left, ledger.GetOrCreate(9));
    }

    [Fact]
    public void AdoptionRemapMustCoverEveryCapturedNativeInstance()
    {
        var generated = new Queue<string>(
        [
            "10000000000000000000000000000001",
            "10000000000000000000000000000002",
        ]);
        var ledger = new ManifestIdentityLedger(() => generated.Dequeue());
        var first = ledger.GetOrCreate(9);
        _ = ledger.GetOrCreate(10);

        Assert.Throws<InvalidDataException>(() => ledger.ApplyRemap(
            Capture,
            new Dictionary<ManifestIdentity, ManifestIdentity>
            {
                [ManifestIdentity.Parse(first)] = ManifestIdentity.Parse(Left),
            }));
        Assert.False(ledger.IsAuthoritative);
    }

    [Fact]
    public void FailedAdoptionRetryPrunesProvisionalHandlesOutsideTheNextCapture()
    {
        var generated = new Queue<string>(
        [
            "10000000000000000000000000000001",
            "10000000000000000000000000000002",
        ]);
        var ledger = new ManifestIdentityLedger(() => generated.Dequeue());
        _ = ledger.Snapshot([9, 10]);

        var retry = ledger.Snapshot([9]);

        Assert.Single(retry);
        Assert.Equal(1, ledger.Count);
        Assert.False(ledger.Contains(10));
    }

    [Fact]
    public void BootstrapRetryWithTheSameContractIsIdempotent()
    {
        var digest = ManifestIdentityLedger.Digest([Root, Left]);
        var ledger = new ManifestIdentityLedger();
        ledger.Bootstrap([new(1, Root), new(2, Left)], Root, 2, digest);

        ledger.Bootstrap([], Root, 2, digest);

        Assert.True(ledger.IsAuthoritative);
        Assert.Equal(2, ledger.Count);
    }

    [Fact]
    public void ReplacementBootstrapAtomicallyAdoptsANewAuthoritativeContract()
    {
        const string Replacement = "00000000000000000000000000000004";
        var originalDigest = ManifestIdentityLedger.Digest([Root, Left]);
        var replacementDigest = ManifestIdentityLedger.Digest([Root, Replacement]);
        var ledger = new ManifestIdentityLedger();
        ledger.Bootstrap([new(1, Root), new(2, Left)], Root, 2, originalDigest);

        Assert.Throws<InvalidDataException>(() => ledger.ReplaceBootstrap(
            [new(1, Root)], Root, 2, replacementDigest));
        Assert.Equal(originalDigest, ledger.ActiveDigest());
        Assert.True(ledger.Contains(2));

        ledger.ReplaceBootstrap(
            [new(1, Root), new(3, Replacement)], Root, 2, replacementDigest);

        Assert.True(ledger.IsAuthoritative);
        Assert.Equal(replacementDigest, ledger.ActiveDigest());
        Assert.False(ledger.Contains(2));
        Assert.True(ledger.Contains(3));
    }

    [Fact]
    public void ActiveLedgerAdoptsReloadContractWithoutStaleTransportMarkers()
    {
        const string Captured = "00000000000000000000000000000004";
        const string RuntimeOnly = "00000000000000000000000000000005";
        const string Missing = "00000000000000000000000000000006";
        var originalDigest = ManifestIdentityLedger.Digest([Root, Left]);
        var capturedDigest = ManifestIdentityLedger.Digest([Root, Left, Captured]);
        var generated = new Queue<string>([Captured, RuntimeOnly]);
        var ledger = new ManifestIdentityLedger(() => generated.Dequeue());
        ledger.Bootstrap([new(1, Root), new(2, Left)], Root, 2, originalDigest);
        Assert.Equal(Captured, ledger.GetOrCreate(3));
        Assert.Equal(RuntimeOnly, ledger.GetOrCreate(4));

        Assert.False(ledger.TryAdoptActiveContract(
            [Root, Left, Missing],
            Root,
            3,
            ManifestIdentityLedger.Digest([Root, Left, Missing])));
        Assert.True(ledger.TryAdoptActiveContract(
            [Root, Left, Captured],
            Root,
            3,
            capturedDigest));
        ledger.Bootstrap([], Root, 3, capturedDigest);

        Assert.True(ledger.IsAuthoritative);
        Assert.Equal(3, ledger.Count);
        Assert.False(ledger.Contains(4));
        Assert.Equal(capturedDigest, ledger.ActiveDigest());
    }

    [Fact]
    public void ReplacementBootstrapCanCombineRetainedAndNewContractBindings()
    {
        const string Replacement = "00000000000000000000000000000004";
        var originalDigest = ManifestIdentityLedger.Digest([Root, Left]);
        var replacementDigest = ManifestIdentityLedger.Digest([Root, Left, Replacement]);
        var ledger = new ManifestIdentityLedger();
        ledger.Bootstrap([new(1, Root), new(2, Left)], Root, 2, originalDigest);

        var retained = ledger.SnapshotExpectedBindings([Root, Left, Replacement]);
        var replacement = retained.Append(new ManifestIdentityBinding(3, Replacement));
        ledger.ReplaceBootstrap(replacement, Root, 3, replacementDigest);

        Assert.True(ledger.IsAuthoritative);
        Assert.Equal(replacementDigest, ledger.ActiveDigest());
        Assert.True(ledger.Contains(1));
        Assert.True(ledger.Contains(2));
        Assert.True(ledger.Contains(3));
    }

    [Fact]
    public void AuthoritativeRootIdentitySurvivesEditDataModelReattachment()
    {
        var digest = ManifestIdentityLedger.Digest([Root, Left]);
        var ledger = new ManifestIdentityLedger();
        ledger.Bootstrap([new(1, Root), new(2, Left)], Root, 2, digest);

        ledger.RebindHandle(1, 9);

        Assert.True(ledger.IsAuthoritative);
        Assert.False(ledger.Contains(1));
        Assert.True(ledger.Contains(9));
        Assert.Equal(Root, ledger.GetOrCreate(9));
        Assert.Equal(2, ledger.Count);
        Assert.Equal(digest, ledger.ActiveDigest());
    }

    [Fact]
    public void EditDataModelReattachmentRequiresTheRetainedHierarchy()
    {
        var digest = ManifestIdentityLedger.Digest([Root, Left]);
        var ledger = new ManifestIdentityLedger();
        ledger.Bootstrap([new(1, Root), new(2, Left)], Root, 2, digest);

        Assert.True(ledger.MatchesRetainedAttachment([9, 2], 1, 9));
        Assert.False(ledger.MatchesRetainedAttachment([9], 1, 9));
        Assert.False(ledger.MatchesRetainedAttachment([9, 2], 7, 9));

        var provisional = new ManifestIdentityLedger(() => Left);
        _ = provisional.GetOrCreate(2);
        Assert.False(provisional.MatchesRetainedAttachment([9, 2], 1, 9));
    }

    private static byte[] SerializeAttributes(IReadOnlyList<AttributeWire> attributes)
    {
        using var stream = new MemoryStream();
        using var writer = new BinaryWriter(stream, Encoding.UTF8, leaveOpen: true);
        writer.Write((uint)attributes.Count);
        foreach (var attribute in attributes)
        {
            writer.Write((uint)Encoding.UTF8.GetByteCount(attribute.Name));
            writer.Write(Encoding.UTF8.GetBytes(attribute.Name));
            writer.Write(attribute.TypeId);
            writer.Write(attribute.Value);
        }
        return stream.ToArray();
    }

    private static byte[] StringValue(string value)
    {
        var encoded = Encoding.UTF8.GetBytes(value);
        var result = new byte[sizeof(uint) + encoded.Length];
        BinaryPrimitives.WriteUInt32LittleEndian(result, (uint)encoded.Length);
        encoded.CopyTo(result.AsSpan(sizeof(uint)));
        return result;
    }

    private static byte[] SequenceValue(int stride)
    {
        var result = new byte[sizeof(uint) + stride];
        BinaryPrimitives.WriteUInt32LittleEndian(result, 1);
        return result;
    }

    private static byte[] CFrameWithRotationId()
    {
        var result = new byte[13];
        result[12] = 1;
        return result;
    }

    private static byte[] Concat(params byte[][] values)
    {
        var result = new byte[values.Sum(value => value.Length)];
        var offset = 0;
        foreach (var value in values)
        {
            value.CopyTo(result, offset);
            offset += value.Length;
        }
        return result;
    }

    private readonly record struct AttributeWire(string Name, byte TypeId, byte[] Value);
}
