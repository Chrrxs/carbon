using Xunit;
using System.Runtime.CompilerServices;

namespace Carbon.RmlBridge.Tests;

public sealed class CaptureDirtyPageTableTests
{
    [Fact]
    public void DataModelReloadRenewsDisposedCapturePageTable()
    {
        var first = CarbonBridgeMod.RenewCaptureDirtyPageTable(null);
        var second = CarbonBridgeMod.RenewCaptureDirtyPageTable(first);

        Assert.Throws<ObjectDisposedException>(first.Reset);
        second.Reset();
        second.Dispose();
    }

    [Fact]
    public void AcknowledgedBaselineReusesCleanPagesAndSerializesOnlyDirtyPage()
    {
        using var table = new CaptureDirtyPageTable();
        CapturePageDefinition[] pages =
        [
            new("page-a", [0x10, 0x11]),
            new("page-b", [0x20, 0x21]),
        ];

        var initial = table.Plan(
            "00000000000000000000000000000001",
            Key(),
            hierarchySequence: 7,
            changeSequence: 11,
            pages,
            allowReuse: true);
        Assert.All(initial.Pages, page => Assert.Equal(CapturePageDisposition.Serialize, page.Disposition));

        StoreAndStage(
            table,
            initial,
            hierarchySequence: 7,
            changeSequence: 11,
            [[0x01, 0x02], [0x03, 0x04]]);
        table.Acknowledge(initial.CaptureId);

        table.MarkDirty(0x20, changeSequence: 12);
        table.MarkDirty(0x21, changeSequence: 13);

        var changed = table.Plan(
            "00000000000000000000000000000002",
            Key(),
            hierarchySequence: 7,
            changeSequence: 13,
            pages,
            allowReuse: true);

        Assert.Equal(CapturePageDisposition.Reuse, changed.Pages[0].Disposition);
        Assert.Equal([0x01, 0x02], changed.Pages[0].ReusedPayload!.ReadAllBytes());
        Assert.Equal(CapturePageDisposition.Serialize, changed.Pages[1].Disposition);
        Assert.Null(changed.Pages[1].ReusedPayload);
        Assert.Same(initial.Routes, changed.Routes);
        Assert.Equal(1, table.DirtyPageCount);
    }

    [Fact]
    public void UnacknowledgedCaptureNeverBecomesReusableEvidence()
    {
        using var table = new CaptureDirtyPageTable();
        var initial = table.Plan(CaptureId(1), Key(), 7, 11, Pages(), allowReuse: true);
        StoreAndStage(table, initial, 7, 11, [[0x01], [0x02]]);

        var retry = table.Plan(CaptureId(2), Key(), 7, 11, Pages(), allowReuse: true);

        Assert.All(retry.Pages, page => Assert.Equal(CapturePageDisposition.Serialize, page.Disposition));
        Assert.Throws<InvalidOperationException>(() => table.Acknowledge(initial.CaptureId));
    }

    [Fact]
    public void UnknownDirtyHandlePoisonsReuseAndForcesEveryPage()
    {
        using var table = Seed();

        table.MarkDirty(0x99, changeSequence: 12);
        var changed = table.Plan(CaptureId(2), Key(), 7, 12, Pages(), allowReuse: true);

        Assert.True(table.IsPoisoned);
        Assert.All(changed.Pages, page => Assert.Equal(CapturePageDisposition.Serialize, page.Disposition));
    }

    [Fact]
    public void StructuralInvalidationAndKeyChangesFailClosedToFullSerialization()
    {
        using var structurallyDirty = Seed();
        structurallyDirty.InvalidateStructure();
        var changedStructure = structurallyDirty.Plan(
            CaptureId(2), Key(), 8, 12, Pages(), allowReuse: true);
        Assert.All(
            changedStructure.Pages,
            page => Assert.Equal(CapturePageDisposition.Serialize, page.Disposition));

        using var changedKeyTable = Seed();
        var changedKey = changedKeyTable.Plan(
            CaptureId(3),
            Key() with { ReflectionSchemaHash = "reflection-schema-v2" },
            7,
            12,
            Pages(),
            allowReuse: true);
        Assert.All(
            changedKey.Pages,
            page => Assert.Equal(CapturePageDisposition.Serialize, page.Disposition));
    }

    [Fact]
    public void ForceFullBypassesReusablePagesAndCanSeedTheNextCapture()
    {
        using var table = Seed();
        var forced = table.Plan(CaptureId(2), Key(), 7, 11, Pages(), allowReuse: false);
        Assert.All(forced.Pages, page => Assert.Equal(CapturePageDisposition.Serialize, page.Disposition));

        StoreAndStage(table, forced, 7, 11, [[0x11], [0x22]]);
        table.Acknowledge(forced.CaptureId);
        var next = table.Plan(CaptureId(3), Key(), 7, 11, Pages(), allowReuse: true);
        Assert.All(next.Pages, page => Assert.Equal(CapturePageDisposition.Reuse, page.Disposition));
        Assert.Equal([0x11], next.Pages[0].ReusedPayload!.ReadAllBytes());
        Assert.Equal([0x22], next.Pages[1].ReusedPayload!.ReadAllBytes());
    }

    [Fact]
    public void EpochDriftRejectsStagingAndAChangeAfterStageRejectsAcknowledgement()
    {
        using var table = Seed();
        var plan = table.Plan(CaptureId(2), Key(), 7, 11, Pages(), allowReuse: true);
        Assert.Throws<InvalidOperationException>(() =>
            table.Stage(plan, hierarchySequence: 7, changeSequence: 12));

        table.Stage(plan, hierarchySequence: 7, changeSequence: 11);
        table.MarkDirty(0x10, changeSequence: 12);
        Assert.Throws<InvalidOperationException>(() => table.Acknowledge(plan.CaptureId));
    }

    [Fact]
    public void SelectiveReuseEmitsTheSameCompleteArtifactAsForcedFull()
    {
        using var table = Seed();
        table.MarkDirty(0x20, changeSequence: 12);
        var changed = table.Plan(CaptureId(2), Key(), 7, 12, Pages(), allowReuse: true);
        var changedSecondPage = new byte[] { 0x33, 0x44 };
        var selective = CaptureModelArtifact.Encode(
        [
            new([2], changed.Pages[0].ReusedPayload!.ReadAllBytes()),
            new([3], changedSecondPage),
        ]);
        var forcedFull = CaptureModelArtifact.Encode(
        [
            new([2], [0x01, 0x02]),
            new([3], changedSecondPage),
        ]);

        Assert.Equal(forcedFull, selective);
    }

    [Fact]
    public void PageIdentityCoversRootsFrontiersMembersAndTheirOrder()
    {
        var baseline = CaptureDirtyPageTable.ComputePageId([1], [4], [1, 2, 3], [7], [8]);

        Assert.Equal(baseline, CaptureDirtyPageTable.ComputePageId([1], [4], [1, 2, 3], [7], [8]));
        Assert.NotEqual(baseline, CaptureDirtyPageTable.ComputePageId([9], [4], [1, 2, 3], [7], [8]));
        Assert.NotEqual(baseline, CaptureDirtyPageTable.ComputePageId([1], [8], [1, 2, 3], [7], [8]));
        Assert.NotEqual(baseline, CaptureDirtyPageTable.ComputePageId([1], [4], [1, 3, 2], [7], [8]));
        Assert.NotEqual(baseline, CaptureDirtyPageTable.ComputePageId([1], [4], [1, 2, 3], [9], [8]));
        Assert.NotEqual(baseline, CaptureDirtyPageTable.ComputePageId([1], [4], [1, 2, 3], [7], [9]));
    }

    [Fact]
    public void CachedPageStreamingMatchesTheFullFrameAndRejectsCorruption()
    {
        using var table = Seed();
        var reused = table.Plan(CaptureId(2), Key(), 7, 11, Pages(), allowReuse: true);
        var page = reused.Pages[0].ReusedPayload!;
        var expected = CaptureModelArtifact.Encode([new([2], [0x01, 0x02])]);
        using var output = new MemoryStream();
        var writer = new CaptureModelArtifactWriter(output);
        writer.Begin(1);
        using (var input = page.OpenRead())
        {
            writer.WriteChunk([2], input, page.Length, page.Digest);
        }
        writer.Complete();
        Assert.Equal(expected, output.ToArray());

        File.WriteAllBytes(page.Path, [0xff, 0xee]);
        using var corruptOutput = new MemoryStream();
        var corruptWriter = new CaptureModelArtifactWriter(corruptOutput);
        corruptWriter.Begin(1);
        using var corruptInput = page.OpenRead();
        Assert.Throws<InvalidDataException>(() =>
            corruptWriter.WriteChunk([2], corruptInput, page.Length, page.Digest));
    }

    [Fact]
    public void AcknowledgingAChangedCaptureRetainsCleanContentWithoutCopyingIt()
    {
        var directory = Path.Combine(
            Path.GetTempPath(),
            "carbon-capture-page-test",
            Guid.NewGuid().ToString("N"));
        using var table = new CaptureDirtyPageTable(directory);
        Seed(table);
        var initial = table.Plan(CaptureId(2), Key(), 7, 11, Pages(), allowReuse: true);
        var cleanPath = initial.Pages[0].ReusedPayload!.Path;
        Assert.Equal(2, Directory.EnumerateFiles(directory, "*.rbxm").Count());

        table.MarkDirty(0x20, changeSequence: 12);
        var changed = table.Plan(CaptureId(3), Key(), 7, 12, Pages(), allowReuse: true);
        StoreAndStage(table, changed, 7, 12, [null, new byte[] { 0x33, 0x44 }]);
        table.Acknowledge(changed.CaptureId);

        var next = table.Plan(CaptureId(4), Key(), 7, 12, Pages(), allowReuse: true);
        Assert.Equal(cleanPath, next.Pages[0].ReusedPayload!.Path);
        Assert.Equal(2, Directory.EnumerateFiles(directory, "*.rbxm").Count());
        Assert.Equal([0x33, 0x44], next.Pages[1].ReusedPayload!.ReadAllBytes());
    }

    [Fact]
    public void SpoolingAPageDoesNotRetainItsManagedSerializerBuffer()
    {
        using var table = new CaptureDirtyPageTable();
        var plan = table.Plan(CaptureId(1), Key(), 7, 11, Pages(), allowReuse: true);

        var payload = StoreLargePage(table, plan);
        GC.Collect(GC.MaxGeneration, GCCollectionMode.Forced, blocking: true, compacting: true);

        Assert.False(payload.TryGetTarget(out _));
    }

    private static CaptureDirtyPageTable Seed()
    {
        var table = new CaptureDirtyPageTable();
        Seed(table);
        return table;
    }

    private static void Seed(CaptureDirtyPageTable table)
    {
        var initial = table.Plan(CaptureId(1), Key(), 7, 11, Pages(), allowReuse: true);
        StoreAndStage(table, initial, 7, 11, [[0x01, 0x02], [0x03, 0x04]]);
        table.Acknowledge(initial.CaptureId);
    }

    private static void StoreAndStage(
        CaptureDirtyPageTable table,
        CaptureDirtyPagePlan plan,
        long hierarchySequence,
        long changeSequence,
        IReadOnlyList<byte[]?> payloads)
    {
        Assert.Equal(plan.Pages.Length, payloads.Count);
        for (var index = 0; index < payloads.Count; index++)
        {
            if (payloads[index] is { } payload)
            {
                table.StoreSerializedPage(plan, index, payload);
            }
        }
        table.Stage(plan, hierarchySequence, changeSequence);
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private static WeakReference<byte[]> StoreLargePage(
        CaptureDirtyPageTable table,
        CaptureDirtyPagePlan plan)
    {
        var payload = new byte[8 * 1024 * 1024];
        table.StoreSerializedPage(plan, 0, payload);
        return new(payload);
    }

    private static CapturePageDefinition[] Pages() =>
    [
        new("page-a", [0x10, 0x11]),
        new("page-b", [0x20, 0x21]),
    ];

    private static string CaptureId(int value) => value.ToString("x32");

    private static CapturePageTableKey Key() => new(
        EngineGeneration: 3,
        StudioSessionId: "studio-session",
        InstanceId: "studio-instance",
        ManagedContractId: "managed-contract",
        ReflectionSchemaHash: "reflection-schema",
        MappingFingerprint: "mapped-roots",
        ManifestIdentitiesAuthoritative: true);
}
