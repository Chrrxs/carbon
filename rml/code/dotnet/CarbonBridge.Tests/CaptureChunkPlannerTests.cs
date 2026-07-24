using Xunit;

namespace Carbon.RmlBridge.Tests;

public sealed class CaptureChunkPlannerTests
{
    [Fact]
    public void ArtifactFramesOrderedMultiRootChunksWithoutLegacyPayloads()
    {
        var artifact = CaptureModelArtifact.Encode(
        [
            new([2, 3], [0x11, 0x22]),
            new([7], [0x33]),
        ]);
        using var reader = new BinaryReader(new MemoryStream(artifact));

        Assert.Equal(CaptureModelArtifact.Magic.ToArray(), reader.ReadBytes(CaptureModelArtifact.Magic.Length));
        Assert.Equal(2U, reader.ReadUInt32());
        Assert.Equal(2U, reader.ReadUInt32());
        Assert.Equal(2U, reader.ReadUInt32());
        Assert.Equal(3U, reader.ReadUInt32());
        Assert.Equal(2UL, reader.ReadUInt64());
        Assert.Equal([0x11, 0x22], reader.ReadBytes(2));
        Assert.Equal(1U, reader.ReadUInt32());
        Assert.Equal(7U, reader.ReadUInt32());
        Assert.Equal(1UL, reader.ReadUInt64());
        Assert.Equal([0x33], reader.ReadBytes(1));
        Assert.Equal(reader.BaseStream.Length, reader.BaseStream.Position);
    }

    [Fact]
    public void EmptyArtifactHasAnExplicitZeroChunkHeader()
    {
        var artifact = CaptureModelArtifact.Encode([]);
        Assert.Equal("CARBONCM2"u8.ToArray(), artifact[..CaptureModelArtifact.Magic.Length]);
        Assert.Equal([0, 0, 0, 0], artifact[CaptureModelArtifact.Magic.Length..]);
    }

    [Fact]
    public void FlatRootsArePackedIntoBoundedMultiRootChunks()
    {
        var nodes = new List<CaptureHierarchyNode>
        {
            new(CaptureEnvelope.NoParent, "DataModel", "game"),
            new(0, "Workspace", "Workspace"),
        };
        nodes.AddRange(Enumerable.Range(0, 10)
            .Select(index => new CaptureHierarchyNode(1, "Folder", $"Root{index}")));

        var chunks = CaptureChunkPlanner.Plan(
            nodes,
            Enumerable.Range(2, 10).Select(value => checked((uint)value)).ToArray(),
            nodeBudget: 4);

        Assert.Equal([4U, 4U, 2U], chunks.Select(chunk => chunk.NodeCount));
        Assert.All(chunks, chunk => Assert.Empty(chunk.FrontierOrdinals));
        Assert.Equal(
            Enumerable.Range(2, 10).Select(value => checked((uint)value)),
            chunks.SelectMany(chunk => chunk.MemberOrdinals));
        Assert.Equal(
            Enumerable.Range(2, 10).Select(value => checked((uint)value)),
            chunks.SelectMany(chunk => chunk.RootOrdinals));
    }

    [Fact]
    public void DeepTreeCutsOnlyAtDeterministicChildFrontiers()
    {
        var nodes = new List<CaptureHierarchyNode>
        {
            new(CaptureEnvelope.NoParent, "DataModel", "game"),
            new(0, "Workspace", "Workspace"),
        };
        for (uint ordinal = 2; ordinal < 12; ordinal++)
        {
            nodes.Add(new(ordinal == 2 ? 1 : ordinal - 1, "Folder", $"Node{ordinal}"));
        }

        var chunks = CaptureChunkPlanner.Plan(nodes, [2], nodeBudget: 4);

        Assert.Equal(3, chunks.Length);
        Assert.Equal([2U], chunks[0].RootOrdinals);
        Assert.Equal([6U], chunks[0].FrontierOrdinals);
        Assert.Equal([6U], chunks[1].RootOrdinals);
        Assert.Equal([10U], chunks[1].FrontierOrdinals);
        Assert.Equal([10U], chunks[2].RootOrdinals);
        Assert.Empty(chunks[2].FrontierOrdinals);
        Assert.All(chunks, chunk => Assert.InRange(chunk.NodeCount, 1U, 4U));
        Assert.Equal(
            Enumerable.Range(2, 10).Select(value => checked((uint)value)),
            chunks.SelectMany(chunk => chunk.MemberOrdinals).Order());
    }

    [Fact]
    public void CrossChunkJointEndpointIsAddedAsAnIsolatedSerializerDependency()
    {
        CaptureHierarchyNode[] nodes =
        [
            new(CaptureEnvelope.NoParent, "DataModel", "game"),
            new(0, "Workspace", "Workspace"),
            new(1, "Part", "HumanoidRootPart"),
            new(1, "Part", "CollisionPart"),
            new(3, "ManualWeld", "Weld"),
        ];

        var chunks = CaptureChunkPlanner.Plan(
            nodes,
            [2, 3],
            nodeBudget: 2,
            referenceDependencies: [new(4, 2), new(4, 2)]);

        Assert.Equal(2, chunks.Length);
        Assert.Empty(chunks[0].DependencyOrdinals);
        Assert.Equal([2U], chunks[1].DependencyOrdinals);
        Assert.Equal([3U, 4U], chunks[1].MemberOrdinals);
    }

    [Fact]
    public void FrontierJointEndpointIsNeverMaskedInItsOwningSerializerChunk()
    {
        CaptureHierarchyNode[] nodes =
        [
            new(CaptureEnvelope.NoParent, "DataModel", "game"),
            new(0, "Workspace", "Workspace"),
            new(1, "Model", "Rig"),
            new(2, "Part", "LowerTorso"),
            new(3, "Motor6D", "Root"),
            new(2, "Part", "HumanoidRootPart"),
        ];

        var chunks = CaptureChunkPlanner.Plan(
            nodes,
            [2],
            nodeBudget: 3,
            referenceDependencies: [new(4, 3), new(4, 5)]);

        Assert.Equal(
            Enumerable.Range(2, 4).Select(value => checked((uint)value)),
            chunks.SelectMany(chunk => chunk.MemberOrdinals).Order());
        Assert.All(chunks, chunk => Assert.InRange(chunk.NodeCount, 1U, 3U));
        Assert.DoesNotContain(
            chunks,
            chunk => chunk.DependencyOrdinals
                .Intersect(chunk.FrontierOrdinals)
                .Any());
        Assert.Contains(
            chunks,
            chunk => chunk.RootOrdinals.SequenceEqual([3U])
                && chunk.DependencyOrdinals.SequenceEqual([5U]));
    }

    [Fact]
    public void PlannerRejectsAServiceChildMissingFromDirectRoots()
    {
        CaptureHierarchyNode[] nodes =
        [
            new(CaptureEnvelope.NoParent, "DataModel", "game"),
            new(0, "Workspace", "Workspace"),
            new(1, "Folder", "First"),
            new(1, "Folder", "Second"),
        ];

        var error = Assert.Throws<InvalidDataException>(
            () => CaptureChunkPlanner.Plan(nodes, [2], nodeBudget: 4));
        Assert.Contains("covers 1 of 2", error.Message);
    }

    [Fact]
    public void MillionFlatRootsRemainLinearAndUseMultiRootChunks()
    {
        const int rootCount = 1_000_000;
        var nodes = new List<CaptureHierarchyNode>(rootCount + 2)
        {
            new(CaptureEnvelope.NoParent, "DataModel", "game"),
            new(0, "Workspace", "Workspace"),
        };
        var roots = new uint[rootCount];
        for (var index = 0; index < rootCount; index++)
        {
            roots[index] = checked((uint)index + 2);
            nodes.Add(new(1, "Folder", "Repeated"));
        }

        var allocatedBefore = GC.GetAllocatedBytesForCurrentThread();
        var started = System.Diagnostics.Stopwatch.StartNew();
        var chunks = CaptureChunkPlanner.Plan(nodes, roots);
        started.Stop();
        var allocated = GC.GetAllocatedBytesForCurrentThread() - allocatedBefore;

        Assert.Equal(31, chunks.Length);
        Assert.Equal(rootCount, chunks.Sum(chunk => chunk.RootOrdinals.Length));
        Assert.All(chunks, chunk =>
        {
            Assert.InRange(chunk.NodeCount, 1U, CaptureChunkPlanner.DefaultNodeBudget);
            Assert.Empty(chunk.FrontierOrdinals);
            Assert.Equal(chunk.RootOrdinals, chunk.MemberOrdinals);
        });
        Assert.True(
            started.Elapsed < TimeSpan.FromSeconds(5),
            $"million-root chunk planning took {started.Elapsed.TotalSeconds:F3}s");
        Assert.True(
            allocated < 64L * 1024 * 1024,
            $"million-root chunk planning allocated {allocated / (1024.0 * 1024.0):F1} MiB");
    }

    [Theory]
    [InlineData(4_096U, 16)]
    [InlineData(8_192U, 8)]
    [InlineData(16_384U, 4)]
    [InlineData(32_768U, 2)]
    public void FlatRootBudgetMatrixPreservesCoverage(uint budget, int expectedChunks)
    {
        const int rootCount = 65_536;
        var nodes = new List<CaptureHierarchyNode>(rootCount + 2)
        {
            new(CaptureEnvelope.NoParent, "DataModel", "game"),
            new(0, "Workspace", "Workspace"),
        };
        var roots = new uint[rootCount];
        for (var index = 0; index < rootCount; index++)
        {
            roots[index] = checked((uint)index + 2);
            nodes.Add(new(1, "Folder", "Repeated"));
        }

        var chunks = CaptureChunkPlanner.Plan(nodes, roots, budget);

        Assert.Equal(expectedChunks, chunks.Length);
        Assert.Equal(rootCount, chunks.Sum(chunk => chunk.RootOrdinals.Length));
        Assert.All(chunks, chunk => Assert.InRange(chunk.NodeCount, 1U, budget));
    }
}
