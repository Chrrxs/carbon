using System.Security.Cryptography;
using System.Net;
using System.Net.Sockets;
using System.Reflection;
using System.Runtime.CompilerServices;

using Xunit;

namespace Carbon.RmlBridge.Tests;

public sealed class CaptureLeaseTests
{
    [Fact]
    public void ChunkRootsArePackedAsOneSerializerListArgument()
    {
        var roots = Array.Empty<Roblox.Instance>();
        var arguments = CarbonBridgeMod.CaptureSerializerArguments(roots);

        Assert.Single(arguments);
        Assert.Same(roots, Assert.IsAssignableFrom<IReadOnlyList<Roblox.Instance>>(arguments[0]));
    }

    [Fact]
    public void OnlyTheExactExcludedEditCameraBecomesAWorkspaceReset()
    {
        const nuint editCamera = 0x1234;
        const nuint authoredCamera = 0x5678;

        Assert.True(CarbonBridgeMod.IsExcludedEditCameraReference(
            "Workspace", "CurrentCamera", editCamera, editCamera));
        Assert.False(CarbonBridgeMod.IsExcludedEditCameraReference(
            "Workspace", "CurrentCamera", authoredCamera, editCamera));
        Assert.False(CarbonBridgeMod.IsExcludedEditCameraReference(
            "Workspace", "CameraSubject", editCamera, editCamera));
        Assert.False(CarbonBridgeMod.IsExcludedEditCameraReference(
            "Folder", "CurrentCamera", editCamera, editCamera));
        Assert.False(CarbonBridgeMod.IsExcludedEditCameraReference(
            "Workspace", "CurrentCamera", editCamera, 0));
    }

    [Fact]
    public void MappedReferenceResolutionSkipsNilKnownAndDuplicateTargets()
    {
        IReadOnlyDictionary<nuint, string> known = new Dictionary<nuint, string>
        {
            [0x20] = "known",
        };

        Assert.Equal(
            [0x10u, 0x30u],
            CarbonBridgeMod.SelectUnboundCaptureReferenceHandles(
                [0, 0x30, 0x20, 0x10, 0x30, 0x10],
                known));
    }

    [Fact]
    public void UnknownShellReferenceIsOmittedButEveryDescendantReferenceRemains()
    {
        IReadOnlyDictionary<string, string[]> shellSchema =
            new Dictionary<string, string[]>(StringComparer.Ordinal)
            {
                ["Workspace"] = ["CurrentCamera"],
            };

        Assert.False(CarbonBridgeMod.ShouldIncludeCaptureExternalReference(
            ownerIsShell: true,
            ownerClass: "Workspace",
            property: "LodEntity",
            shellSchema));
        Assert.True(CarbonBridgeMod.ShouldIncludeCaptureExternalReference(
            ownerIsShell: false,
            ownerClass: "ObjectValue",
            property: "Value",
            shellSchema));
        Assert.True(CarbonBridgeMod.ShouldIncludeCaptureExternalReference(
            ownerIsShell: false,
            ownerClass: "ObjectValue",
            property: "Value",
            shellSchema));
    }

    [Fact]
    public void CrossDomainValidationAllowsManifestReferencesToMappedTargetsOnly()
    {
        Assert.False(CarbonBridgeMod.CrossesCaptureOwnershipBarrier(
            ownerIsMapped: false, ownerIsManifest: true,
            targetIsMapped: true, targetIsManifest: false));
        Assert.True(CarbonBridgeMod.CrossesCaptureOwnershipBarrier(
            ownerIsMapped: true, ownerIsManifest: false,
            targetIsMapped: false, targetIsManifest: true));
        Assert.False(CarbonBridgeMod.CrossesCaptureOwnershipBarrier(
            ownerIsMapped: false, ownerIsManifest: false,
            targetIsMapped: true, targetIsManifest: false));
        Assert.False(CarbonBridgeMod.CrossesCaptureOwnershipBarrier(
            ownerIsMapped: true, ownerIsManifest: false,
            targetIsMapped: false, targetIsManifest: false));
    }

    [Fact]
    public void OnlyKnownClientDisconnectErrorsAreLifecycleInformation()
    {
        Assert.True(CarbonBridgeMod.IsExpectedClientDisconnect(new HttpListenerException(1229)));
        Assert.True(CarbonBridgeMod.IsExpectedClientDisconnect(
            new SocketException((int)SocketError.NotConnected)));
        Assert.True(CarbonBridgeMod.IsExpectedClientDisconnect(
            new IOException("wrapped", new SocketException((int)SocketError.ConnectionReset))));
        Assert.False(CarbonBridgeMod.IsExpectedClientDisconnect(new HttpListenerException(5)));
        Assert.False(CarbonBridgeMod.IsExpectedClientDisconnect(new InvalidOperationException("request failed")));
    }


    [Fact]
    public void ShellSchemaAllowsCompleteSupersetForLateCreatedServices()
    {
        CaptureLeaseManager.EnsureShellSchemaCoverage(
            ["DataModel", "UIDragDetectorService"],
            ["DataModel", "UIDragDetectorService", "Workspace"]);
    }

    [Fact]
    public void ShellSchemaRejectsUnknownCaptureWithExactSetDifference()
    {
        var error = Assert.Throws<InvalidOperationException>(() =>
            CaptureLeaseManager.EnsureShellSchemaCoverage(
                ["DataModel", "LateService"],
                ["DataModel", "Workspace"]));

        Assert.Contains("missing=[LateService]", error.Message, StringComparison.Ordinal);
        Assert.Contains("extra=[Workspace]", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public async Task ShellSchemaValidationNamesTheExactDuplicateProperty()
    {
        await using var manager = new CaptureLeaseManager(
            TemporaryDirectory(),
            (request, phase, modelWriter, cancellationToken) =>
                Task.FromResult(WriteSnapshot(modelWriter)));
        var request = Request() with
        {
            ShellClasses = [new("MaterialService", ["Use2022MaterialsXml", "Use2022MaterialsXml"])],
        };

        var error = Assert.Throws<InvalidDataException>(() => manager.Start(request));
        Assert.Contains(
            "'MaterialService' repeats property 'Use2022MaterialsXml'",
            error.Message,
            StringComparison.Ordinal);
    }

    [Fact]
    public async Task MappedRootSourceIdsAreBoundedDistinctAndCanonical()
    {
        await using var manager = new CaptureLeaseManager(
            TemporaryDirectory(),
            (request, phase, modelWriter, cancellationToken) =>
                Task.FromResult(WriteSnapshot(modelWriter)));

        var duplicate = Assert.Throws<InvalidDataException>(() => manager.Start(
            Request() with { MappedRootSourceIds = [SourceId, SourceId] }));
        Assert.Contains("contains a duplicate", duplicate.Message, StringComparison.Ordinal);

        var invalid = Assert.Throws<InvalidDataException>(() => manager.Start(
            Request() with { MappedRootSourceIds = ["not-a-source-id"] }));
        Assert.Contains("not 128-bit hexadecimal", invalid.Message, StringComparison.Ordinal);

        var oversized = Assert.Throws<InvalidDataException>(() => manager.Start(
            Request() with { MappedRootSourceIds = Enumerable.Repeat(SourceId, 4097).ToArray() }));
        Assert.Contains("missing or too large", oversized.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void MappedBindingsSelectOnlyExactRequestedGraftAnchors()
    {
        var planned = new Dictionary<string, CaptureMappedBinding>(StringComparer.Ordinal)
        {
            [ContractId] = new(ContractId, CaptureEnvelope.SyntheticNode, 0),
            [SourceId] = new(SourceId, CaptureEnvelope.SyntheticNode, 1),
        };

        var selected = CarbonBridgeMod.SelectCaptureMappedBindings(
            [SourceId],
            planned);
        var binding = Assert.Single(selected);
        Assert.Equal(SourceId, binding.SourceId);
        Assert.Equal(CaptureEnvelope.SyntheticNode, binding.HierarchyOrdinal);
        Assert.Equal(1u, binding.ParentOrdinal);

        var missing = Assert.Throws<InvalidOperationException>(() =>
            CarbonBridgeMod.SelectCaptureMappedBindings(
                ["11111111111111111111111111111111"],
                planned));
        Assert.Contains("no verified graft anchor", missing.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void CapturePhaseAttestationRejectsChangesBeforeLaunchAndDuringSettlement()
    {
        CarbonBridgeMod.EnsureCaptureLeaseEpochsUnchanged(
            10,
            20,
            10,
            20,
            "between phases");

        var beforeLaunch = Assert.Throws<InvalidOperationException>(() =>
            CarbonBridgeMod.EnsureCaptureLeaseEpochsUnchanged(
                10,
                20,
                11,
                20,
                "between the native hierarchy read and serializer launch"));
        Assert.Contains("native hierarchy read and serializer launch", beforeLaunch.Message);

        var duringSettlement = Assert.Throws<InvalidOperationException>(() =>
            CarbonBridgeMod.EnsureCaptureLeaseEpochsUnchanged(
                10,
                20,
                10,
                21,
                "while the capture serializer was running"));
        Assert.Contains("while the capture serializer was running", duringSettlement.Message);
    }

    [Fact]
    public async Task SerializerFailureStillRestoresAndPreservesTheFailure()
    {
        var serializerFailure = new InvalidDataException("serializer failed");
        var restoreCalls = 0;

        var observed = await Assert.ThrowsAsync<InvalidDataException>(() =>
            CarbonBridgeMod.AwaitCaptureSerializerWithRestoration(
                Task.FromException<int>(serializerFailure),
                () =>
                {
                    restoreCalls++;
                    return Task.CompletedTask;
                }));

        Assert.Same(serializerFailure, observed);
        Assert.Equal(1, restoreCalls);
    }

    [Fact]
    public async Task SerializerCancellationWaitsForSettlementAndThenRestores()
    {
        var serializer = new TaskCompletionSource<int>(
            TaskCreationOptions.RunContinuationsAsynchronously);
        var restoreCalls = 0;
        var capture = CarbonBridgeMod.AwaitCaptureSerializerWithRestoration(
            serializer.Task,
            () =>
            {
                restoreCalls++;
                return Task.CompletedTask;
            });

        Assert.False(capture.IsCompleted);
        Assert.Equal(0, restoreCalls);
        serializer.SetCanceled(new CancellationToken(canceled: true));

        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => capture);
        Assert.True(capture.IsCanceled);
        Assert.Equal(1, restoreCalls);
    }

    [Fact]
    public async Task StartedLaunchCannotDropSerializerSettlementCleanupOnCancellation()
    {
        var gate = new CaptureLeaseLaunchGate();
        gate.Start(CancellationToken.None);
        var cancellationCalls = 0;
        Assert.False(gate.CancelBeforeStart(() => cancellationCalls++));

        var serializer = new TaskCompletionSource<int>(
            TaskCreationOptions.RunContinuationsAsynchronously);
        var restoreCalls = 0;
        var capture = CarbonBridgeMod.AwaitCaptureSerializerWithRestoration(
            serializer.Task,
            () =>
            {
                restoreCalls++;
                return Task.CompletedTask;
            });
        serializer.SetResult(42);

        Assert.Equal(42, await capture);
        Assert.Equal(0, cancellationCalls);
        Assert.Equal(1, restoreCalls);
    }

    [Fact]
    public void QueuedLaunchCancellationWinsBeforeAnyEngineMutationStarts()
    {
        var gate = new CaptureLeaseLaunchGate();
        var cancellationCalls = 0;

        Assert.True(gate.CancelBeforeStart(() => cancellationCalls++));
        Assert.Equal(1, cancellationCalls);
        Assert.Throws<OperationCanceledException>(() => gate.Start(CancellationToken.None));
    }

    [Fact]
    public async Task RestorationFailureHardFailsAnOtherwiseSuccessfulCapture()
    {
        var restorationFailure = new IOException("setter failed");

        var observed = await Assert.ThrowsAsync<InvalidOperationException>(() =>
            CarbonBridgeMod.AwaitCaptureSerializerWithRestoration(
                Task.FromResult(42),
                () => Task.FromException(restorationFailure)));

        Assert.Contains("Archivable restoration failed", observed.Message, StringComparison.Ordinal);
        Assert.Same(restorationFailure, observed.InnerException);
    }

    [Fact]
    public async Task SerializerAndRestorationFailuresAreBothPreserved()
    {
        var serializerFailure = new InvalidDataException("serializer failed");
        var restorationFailure = new IOException("setter failed");

        var observed = await Assert.ThrowsAsync<AggregateException>(() =>
            CarbonBridgeMod.AwaitCaptureSerializerWithRestoration(
                Task.FromException<int>(serializerFailure),
                () => Task.FromException(restorationFailure)));

        Assert.Same(serializerFailure, observed.InnerExceptions[0]);
        var explicitRestorationFailure = Assert.IsType<InvalidOperationException>(
            observed.InnerExceptions[1]);
        Assert.Same(restorationFailure, explicitRestorationFailure.InnerException);
    }

    [Fact]
    public void ArchivableMaskSuppressionIsExactToExpectedPropertyAndRootNotification()
    {
        const nuint maskedRoot = 0x1234;
        var masks = new CaptureArchivableMaskTracker();
        masks.Register(CaptureId, [maskedRoot]);
        masks.ExpectNotification(CaptureId, maskedRoot);

        Assert.False(masks.TryConsume("Name", maskedRoot));
        Assert.False(masks.TryConsume("archivable", maskedRoot));
        Assert.False(masks.TryConsume("Archivable", 0x5678));
        Assert.True(masks.TryConsume("Archivable", maskedRoot));

        // The root is still owned until restoration, but ownership by itself
        // must not hide a concurrent user mutation.
        Assert.False(masks.TryConsume("Archivable", maskedRoot));
        Assert.True(masks.Contains(maskedRoot));
    }

    [Fact]
    public void DelayedArchivableNotificationsRemainQuarantinedThroughRestoration()
    {
        const nuint maskedRoot = 0x1234;
        var masks = new CaptureArchivableMaskTracker();
        masks.Register(CaptureId, [maskedRoot]);
        masks.ExpectNotification(CaptureId, maskedRoot);
        masks.ExpectNotification(CaptureId, maskedRoot);

        // Roblox can deliver both setter callbacks after the synchronous
        // restoration method has returned.
        masks.CompleteRestoration(CaptureId, [maskedRoot]);
        Assert.True(masks.Contains(maskedRoot));
        Assert.True(masks.TryConsume("Archivable", maskedRoot));
        Assert.True(masks.Contains(maskedRoot));
        Assert.True(masks.TryConsume("Archivable", maskedRoot));
        Assert.False(masks.Contains(maskedRoot));
        Assert.False(masks.TryConsume("Archivable", maskedRoot));
    }

    [Fact]
    public void SettledArchivableMaskRetiresWithoutSuppressingFutureEdits()
    {
        const nuint maskedRoot = 0x1234;
        var masks = new CaptureArchivableMaskTracker();
        masks.Register(CaptureId, [maskedRoot]);
        masks.ExpectNotification(CaptureId, maskedRoot);
        Assert.True(masks.TryConsume("Archivable", maskedRoot));

        masks.CompleteRestoration(CaptureId, [maskedRoot]);

        Assert.False(masks.Contains(maskedRoot));
        Assert.False(masks.TryConsume("Archivable", maskedRoot));
    }

    [Fact]
    public void ArchivableMaskRegistrationAndCompletionAreAtomicAndOwned()
    {
        const nuint first = 0x1234;
        const nuint second = 0x5678;
        var masks = new CaptureArchivableMaskTracker();
        masks.Register(CaptureId, [first]);

        Assert.Throws<InvalidOperationException>(() =>
            masks.Register(OtherCaptureId, [second, first]));
        Assert.False(masks.Contains(second));
        Assert.Throws<InvalidOperationException>(() =>
            masks.CompleteRestoration(OtherCaptureId, [first]));
        Assert.True(masks.Contains(first));
    }

    [Fact]
    public void EnvelopeIsVersionedCompactAndCarriesServiceAndMappedOrdinals()
    {
        var model = new byte[] { 1, 3, 5, 7, 9 };
        var encoded = Encode(Envelope(), model);
        using var input = new MemoryStream(encoded);
        using var reader = new BinaryReader(input);

        Assert.Equal(CaptureEnvelope.Magic.ToArray(), reader.ReadBytes(CaptureEnvelope.Magic.Length));
        Assert.Equal(CaptureEnvelope.Version, reader.ReadUInt16());
        Assert.Equal(0, reader.ReadUInt16());
        Assert.Equal(Convert.FromHexString(CaptureId), reader.ReadBytes(16));
        Assert.Equal(7, reader.ReadInt64());
        Assert.Equal(11, reader.ReadInt64());
        Assert.Equal(11, reader.ReadInt64());
        Assert.Equal(13, reader.ReadInt64());
        Assert.Equal(13, reader.ReadInt64());
        Assert.Equal((ulong)model.Length, reader.ReadUInt64());
        Assert.Equal(SHA256.HashData(model), reader.ReadBytes(32));

        var stringCount = reader.ReadUInt32();
        Assert.Equal(3u, reader.ReadUInt32());
        Assert.Equal(1u, reader.ReadUInt32());
        Assert.Equal(1u, reader.ReadUInt32());
        Assert.Equal(1u, reader.ReadUInt32());
        Assert.Equal(1u, reader.ReadUInt32());
        Assert.Equal(0u, reader.ReadUInt32());
        Assert.Equal(1u, reader.ReadUInt32());
        var studioSessionIndex = reader.ReadUInt32();
        var instanceIdIndex = reader.ReadUInt32();
        var contractIndex = reader.ReadUInt32();
        var reflectionIndex = reader.ReadUInt32();
        var sourceGenerationIndex = reader.ReadUInt32();
        var digestAlgorithmIndex = reader.ReadUInt32();
        var strings = new string[stringCount];
        for (var index = 0; index < strings.Length; index++)
        {
            strings[index] = System.Text.Encoding.UTF8.GetString(
                reader.ReadBytes(checked((int)reader.ReadUInt32())));
        }
        Assert.Equal("studio-session", strings[studioSessionIndex]);
        Assert.Equal("studio-instance", strings[instanceIdIndex]);
        Assert.Equal(ContractId, strings[contractIndex]);
        Assert.Equal("reflection-v1", strings[reflectionIndex]);
        Assert.Equal("source-hash", strings[sourceGenerationIndex]);
        Assert.Equal("sha256", strings[digestAlgorithmIndex]);

        for (var index = 0; index < 3; index++)
        {
            _ = reader.ReadUInt32();
            var nodeClassIndex = reader.ReadUInt32();
            Assert.Contains(strings[nodeClassIndex], new[] { "DataModel", "Workspace", "Folder" });
            Assert.Contains(
                System.Text.Encoding.UTF8.GetString(
                    reader.ReadBytes(checked((int)reader.ReadUInt32()))),
                new[] { "Game", "Workspace", "Captured" });
            _ = reader.ReadUInt32();
        }
        for (var index = 1; index <= 3; index++)
        {
            Assert.Equal(Convert.FromHexString(index.ToString("x32")), reader.ReadBytes(16));
        }
        Assert.Equal(1u, reader.ReadUInt32());
        var rootClassIndex = reader.ReadUInt32();
        var rootNameIndex = reader.ReadUInt32();
        Assert.Equal("Workspace", strings[rootClassIndex]);
        Assert.Equal("Workspace", strings[rootNameIndex]);
        Assert.Equal(0u, reader.ReadUInt32());
        Assert.Equal(1u, reader.ReadUInt32());
        Assert.Equal(Convert.FromHexString(SourceId), reader.ReadBytes(16));
        Assert.Equal(CaptureEnvelope.SyntheticNode, reader.ReadUInt32());
        Assert.Equal(1u, reader.ReadUInt32());
        Assert.Equal(2u, reader.ReadUInt32());
        var referencePropertyIndex = reader.ReadUInt32();
        Assert.Equal("ServiceTarget", strings[referencePropertyIndex]);
        Assert.Equal(1u, reader.ReadUInt32());
        Assert.Equal(1u, reader.ReadUInt32());
        var shellPropertyIndex = reader.ReadUInt32();
        var shellTypeIndex = reader.ReadUInt32();
        Assert.Equal("Gravity", strings[shellPropertyIndex]);
        Assert.Equal("Float64", strings[shellTypeIndex]);
        var gravityBytes = reader.ReadBytes(checked((int)reader.ReadUInt32()));
        Assert.Equal(196.2, BitConverter.ToDouble(gravityBytes));
        Assert.Equal(2u, reader.ReadUInt32());
        Assert.Equal(input.Length, input.Position);
    }

    [Fact]
    public void EnvelopeCarriesAuthoritativeIdentityModeAndRejectsDuplicateIds()
    {
        var authoritative = Envelope() with { ManifestIdentitiesAuthoritative = true };
        var encoded = Encode(authoritative, [1, 2, 3]);
        using (var input = new MemoryStream(encoded))
        using (var reader = new BinaryReader(input))
        {
            _ = reader.ReadBytes(CaptureEnvelope.Magic.Length);
            Assert.Equal(CaptureEnvelope.Version, reader.ReadUInt16());
            Assert.Equal(CaptureEnvelope.AuthoritativeIdentitiesFlag, reader.ReadUInt16());
        }

        var duplicate = Envelope() with
        {
            ManifestIdentities =
            [
                ManifestIdentity.Parse("00000000000000000000000000000001"),
                ManifestIdentity.Parse("00000000000000000000000000000001"),
                ManifestIdentity.Parse("00000000000000000000000000000003"),
            ],
        };
        Assert.Throws<InvalidDataException>(() => Encode(duplicate, [1, 2, 3]));
    }

    [Fact]
    public void EnvelopeCarriesMappedReferenceTargetsAsSourceIdentities()
    {
        var mapped = Envelope() with
        {
            ExternalReferences =
            [
                new(2, "Value", CaptureEnvelope.MappedReference, SourceId),
            ],
        };
        Assert.NotEmpty(Encode(mapped, [1, 2, 3]));

        var missingIdentity = mapped with
        {
            ExternalReferences = [new(2, "Value", CaptureEnvelope.MappedReference)],
        };
        Assert.Throws<InvalidDataException>(() => Encode(missingIdentity, [1, 2, 3]));
    }

    [Fact]
    public void ShellCarrierRootsAreADenseSyntheticSerializerSuffix()
    {
        var envelope = Envelope() with
        {
            ShellCarriers =
            [
                new(1, "ModelOnly", "NetAssetRef", "Workspace", 1),
            ],
            SerializedRootOrdinals = [2, CaptureEnvelope.SyntheticNode],
        };

        Assert.NotEmpty(Encode(envelope, [1, 2, 3]));
        Assert.Throws<InvalidDataException>(() => Encode(
            envelope with { SerializedRootOrdinals = [2, 2] },
            [1, 2, 3]));
        Assert.Throws<InvalidDataException>(() => Encode(
            envelope with
            {
                ShellCarriers = [new(1, "ModelOnly", "NetAssetRef", "Workspace", 2)],
            },
            [1, 2, 3]));
    }

    [Fact]
    public async Task SuccessfulLeaseSpoolsFramedChunksSupportsRangesAndReleasesFiles()
    {
        var directory = TemporaryDirectory();
        CaptureModelChunk[] chunks =
        [
            new([2], Enumerable.Range(0, 32).Select(value => (byte)value).ToArray()),
        ];
        var model = CaptureModelArtifact.Encode(chunks);
        await using var manager = new CaptureLeaseManager(
            directory,
            (request, phase, modelWriter, cancellationToken) =>
            {
                phase(CaptureLeasePhase.Serializing);
                modelWriter.Begin(chunks.Length);
                foreach (var chunk in chunks)
                {
                    modelWriter.WriteChunk(chunk.RootOrdinals, chunk.Payload);
                }
                return Task.FromResult(Envelope());
            });

        manager.Start(Request());
        var status = await WaitForState(manager, CaptureId, "ready");
        Assert.Equal(model.LongLength, status.ModelBytes);
        Assert.Equal(1, status.TotalChunks);
        Assert.Equal(1, status.CompletedChunks);
        Assert.Equal(chunks[0].Payload.LongLength, status.SerializedBytes);
        Assert.Equal(model.LongLength, status.CommittedModelBytes);
        Assert.Equal(CaptureEnvelope.DigestAlgorithm, status.DigestAlgorithm);
        Assert.Equal(Convert.ToHexStringLower(SHA256.HashData(model)), status.ModelDigest);

        var complete = manager.OpenFile(CaptureId, envelope: false, rangeHeader: null);
        Assert.False(complete.IsPartial);
        Assert.Equal(model, await Read(complete));
        var middle = manager.OpenFile(CaptureId, envelope: false, "bytes=5-12");
        Assert.True(middle.IsPartial);
        Assert.Equal(model[5..13], await Read(middle));
        var suffix = manager.OpenFile(CaptureId, envelope: false, "bytes=-4");
        Assert.Equal(model[^4..], await Read(suffix));
        Assert.True(manager.OpenFile(CaptureId, envelope: true, "bytes=0-7").IsPartial);

        var modelPath = complete.Path;
        var envelopePath = manager.OpenFile(CaptureId, envelope: true, null).Path;
        manager.EnsureReadyCapture(CaptureId);
        Assert.Equal(CaptureId, manager.EnsureReadyLease(CaptureId));
        Assert.Throws<InvalidOperationException>(() => manager.EnsureReadyCapture(OtherCaptureId));
        var deleted = manager.Delete(CaptureId);
        Assert.True(deleted.Released);
        Assert.Throws<InvalidOperationException>(() => manager.EnsureReadyCapture(CaptureId));
        Assert.Throws<KeyNotFoundException>(() => manager.EnsureReadyLease(CaptureId));
        Assert.False(File.Exists(modelPath));
        Assert.False(File.Exists(envelopePath));
    }

    [Fact]
    public async Task SuccessfulLeaseStreamsEachSerializerPayloadBeforeTheNextChunk()
    {
        var directory = TemporaryDirectory();
        var firstPayloadReleasedBeforeSecond = false;
        await using var manager = new CaptureLeaseManager(
            directory,
            (request, phase, modelWriter, cancellationToken) =>
            {
                phase(CaptureLeasePhase.Serializing);
                modelWriter.Begin(2);
                var firstPayload = WriteLargeModelChunk(modelWriter, 2);
                GC.Collect(GC.MaxGeneration, GCCollectionMode.Forced, blocking: true, compacting: true);
                firstPayloadReleasedBeforeSecond = !firstPayload.TryGetTarget(out _);
                modelWriter.WriteChunk([3], new byte[8 * 1024 * 1024]);
                return Task.FromResult(Envelope());
            });

        manager.Start(Request());
        var status = await WaitForState(manager, CaptureId, "ready");

        Assert.True(firstPayloadReleasedBeforeSecond);
        Assert.True(status.ModelBytes > 16 * 1024 * 1024);
        Assert.True(manager.Delete(CaptureId).Released);
    }

    [Fact]
    public async Task CompletedFramesAreRangeReadableBeforeTheFinalSeal()
    {
        var directory = TemporaryDirectory();
        CaptureModelChunk[] chunks =
        [
            new([2], [1, 2, 3, 4]),
            new([3], [5, 6, 7, 8]),
        ];
        var completeModel = CaptureModelArtifact.Encode(chunks);
        var firstCommitted = new TaskCompletionSource(
            TaskCreationOptions.RunContinuationsAsynchronously);
        var releaseSecond = new TaskCompletionSource(
            TaskCreationOptions.RunContinuationsAsynchronously);
        await using var manager = new CaptureLeaseManager(
            directory,
            async (request, phase, modelWriter, cancellationToken) =>
            {
                phase(CaptureLeasePhase.Serializing);
                modelWriter.Begin(chunks.Length);
                modelWriter.WriteChunk(chunks[0].RootOrdinals, chunks[0].Payload);
                firstCommitted.SetResult();
                await releaseSecond.Task;
                modelWriter.WriteChunk(chunks[1].RootOrdinals, chunks[1].Payload);
                return Envelope();
            });

        manager.Start(Request());
        await firstCommitted.Task.WaitAsync(TimeSpan.FromSeconds(2));
        try
        {
            var serializing = manager.Get(CaptureId);
            Assert.Equal("serializing", serializing.State);
            Assert.Equal(1, serializing.CompletedChunks);
            Assert.True(serializing.CommittedModelBytes > CaptureModelArtifact.Magic.Length);
            var partial = manager.OpenFile(CaptureId, envelope: false, rangeHeader: null);
            Assert.Equal(serializing.CommittedModelBytes, partial.Length);
            Assert.Equal(
                completeModel[..checked((int)serializing.CommittedModelBytes)],
                await Read(partial));
        }
        finally
        {
            releaseSecond.TrySetResult();
        }
        var ready = await WaitForState(manager, CaptureId, "ready");
        Assert.Equal(completeModel.LongLength, ready.CommittedModelBytes);
        Assert.True(manager.Delete(CaptureId).Released);
    }

    [Fact]
    public async Task ProgressivePayloadResponseDoesNotBlockFinalSealOnWindows()
    {
        if (!OperatingSystem.IsWindows())
        {
            return;
        }

        var directory = TemporaryDirectory();
        var temporaryPath = Path.Combine(directory, "capture.rbxm.tmp");
        var sealedPath = Path.Combine(directory, "capture.rbxm");
        const long payloadLength = 256L * 1024 * 1024;
        await using (var payload = new FileStream(
            temporaryPath,
            FileMode.CreateNew,
            FileAccess.Write,
            FileShare.Read,
            bufferSize: 4_096,
            FileOptions.Asynchronous))
        {
            payload.SetLength(payloadLength);
        }

        var port = ReserveLoopbackPort();
        using var listener = new HttpListener();
        listener.Prefixes.Add($"http://127.0.0.1:{port}/");
        listener.Start();
        using var cancellation = new CancellationTokenSource(TimeSpan.FromSeconds(10));
        using var client = new HttpClient(new HttpClientHandler { UseProxy = false });
        var contextTask = listener.GetContextAsync();
        var responseTask = client.GetAsync(
            $"http://127.0.0.1:{port}/payload",
            HttpCompletionOption.ResponseHeadersRead,
            cancellation.Token);
        var context = await contextTask.WaitAsync(TimeSpan.FromSeconds(3));
        var replyTask = ReplyFileAsync(
            context.Response,
            new CaptureLeaseFile(temporaryPath, 0, payloadLength, payloadLength, false),
            cancellation.Token);

        try
        {
            using var response = await responseTask.WaitAsync(TimeSpan.FromSeconds(3));
            Assert.Equal(HttpStatusCode.OK, response.StatusCode);

            File.Move(temporaryPath, sealedPath);

            Assert.True(File.Exists(sealedPath));
        }
        finally
        {
            cancellation.Cancel();
            listener.Stop();
            try
            {
                await replyTask.WaitAsync(TimeSpan.FromSeconds(3));
            }
            catch (Exception error) when (CarbonBridgeMod.IsExpectedClientDisconnect(error)
                                          || error is OperationCanceledException
                                          || error is ObjectDisposedException
                                          || error is TimeoutException)
            {
            }
            Directory.Delete(directory, recursive: true);
        }
    }

    [Fact]
    public async Task IncompleteStreamFailsWithoutPublishingPartialArtifacts()
    {
        var directory = TemporaryDirectory();
        await using var manager = new CaptureLeaseManager(
            directory,
            (request, phase, modelWriter, cancellationToken) =>
            {
                phase(CaptureLeasePhase.Serializing);
                modelWriter.Begin(2);
                modelWriter.WriteChunk([2], [1, 2, 3]);
                return Task.FromResult(Envelope());
            });

        manager.Start(Request());
        var status = await WaitForState(manager, CaptureId, "failed");

        Assert.Contains("received 1 of 2 chunks", status.Error, StringComparison.Ordinal);
        Assert.Empty(Directory.GetFiles(directory));
        Assert.True(manager.Delete(CaptureId).Released);
    }

    [Fact]
    public async Task CancellationKeepsLeaseExclusiveUntilSerializerSettles()
    {
        var directory = TemporaryDirectory();
        var serializerStarted = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var serializerSettled = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var captures = 0;
        await using var manager = new CaptureLeaseManager(
            directory,
            async (request, phase, modelWriter, cancellationToken) =>
            {
                var capture = Interlocked.Increment(ref captures);
                phase(CaptureLeasePhase.Serializing);
                if (capture == 1)
                {
                    serializerStarted.SetResult();
                    await serializerSettled.Task;
                }
                cancellationToken.ThrowIfCancellationRequested();
                return WriteSnapshot(modelWriter);
            });

        manager.Start(Request());
        await serializerStarted.Task.WaitAsync(TimeSpan.FromSeconds(2));
        var cancelling = manager.Delete(CaptureId);
        Assert.False(cancelling.Released);
        Assert.Equal("cancelling", cancelling.Status.State);
        Assert.False(cancelling.Status.SerializerSettled);
        Assert.Throws<CaptureLeaseConflictException>(() => manager.Start(
            Request() with { CaptureId = OtherCaptureId }));

        serializerSettled.SetResult();
        await WaitUntilAsync(() =>
        {
            try
            {
                _ = manager.Get(CaptureId);
                return false;
            }
            catch (KeyNotFoundException)
            {
                return true;
            }
        });
        manager.Start(Request() with { CaptureId = OtherCaptureId });
        await WaitForState(manager, OtherCaptureId, "ready");
        Assert.Equal(2, captures);
        Assert.True(manager.Delete(OtherCaptureId).Released);
    }

    [Theory]
    [InlineData(null, 0, 10, false)]
    [InlineData("bytes=2-4", 2, 3, true)]
    [InlineData("bytes=7-", 7, 3, true)]
    [InlineData("bytes=-4", 6, 4, true)]
    [InlineData("bytes=8-99", 8, 2, true)]
    public void ByteRangesAreSingleBoundedAndDeterministic(
        string? header,
        long expectedOffset,
        long expectedLength,
        bool expectedPartial)
    {
        Assert.Equal(
            new CaptureByteRange(expectedOffset, expectedLength, expectedPartial),
            CaptureLeaseManager.ParseRange(header, 10));
    }

    [Theory]
    [InlineData("items=0-1")]
    [InlineData("bytes=0-1,3-4")]
    [InlineData("bytes=20-")]
    [InlineData("bytes=4-2")]
    [InlineData("bytes=-0")]
    public void InvalidByteRangesFailClosed(string header)
    {
        Assert.Throws<InvalidDataException>(() => CaptureLeaseManager.ParseRange(header, 10));
    }

    private static CaptureLeaseRequest Request() => new(
        CaptureId,
        "studio-session",
        "studio-instance",
        7,
        "source-hash",
        ContractId,
        "reflection-v1",
        false,
        true,
        [SourceId],
        [
            new("DataModel", []),
            new("Workspace", ["Gravity"]),
        ]);

    private static byte[] Encode(CaptureEnvelopeData envelope, byte[] model)
    {
        using var output = new MemoryStream();
        CaptureEnvelope.Write(output, envelope, model.LongLength, SHA256.HashData(model));
        return output.ToArray();
    }

    private static CaptureEnvelopeData WriteSnapshot(CaptureModelArtifactWriter modelWriter)
    {
        CaptureModelChunk[] chunks =
        [
            new([2], [2, 4, 6, 8]),
        ];
        modelWriter.Begin(chunks.Length);
        foreach (var chunk in chunks)
        {
            modelWriter.WriteChunk(chunk.RootOrdinals, chunk.Payload);
        }
        return Envelope();
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private static WeakReference<byte[]> WriteLargeModelChunk(
        CaptureModelArtifactWriter modelWriter,
        uint rootOrdinal)
    {
        var payload = new byte[8 * 1024 * 1024];
        modelWriter.WriteChunk([rootOrdinal], payload);
        return new(payload);
    }

    private static CaptureEnvelopeData Envelope() => new(
        CaptureId,
        7,
        "source-hash",
        11,
        11,
        13,
        13,
        "studio-session",
        "studio-instance",
        ContractId,
        "reflection-v1",
        [
            new(CaptureEnvelope.NoParent, "DataModel", "Game", 2),
            new(0, "Workspace", "Workspace", 2),
            new(1, "Folder", "Captured", 1),
        ],
        [new(1, "Workspace", "Workspace", 0, 1)],
        [new(SourceId, CaptureEnvelope.SyntheticNode, 1)],
        [new(2, "ServiceTarget", 1)],
        [new(1, "Gravity", "Float64", BitConverter.GetBytes(196.2))],
        [],
        [2],
        false,
        [
            ManifestIdentity.Parse("00000000000000000000000000000001"),
            ManifestIdentity.Parse("00000000000000000000000000000002"),
            ManifestIdentity.Parse("00000000000000000000000000000003"),
        ]);

    private static async Task<CaptureLeaseStatus> WaitForState(
        CaptureLeaseManager manager,
        string leaseId,
        string state)
    {
        CaptureLeaseStatus? status = null;
        await WaitUntilAsync(() =>
        {
            status = manager.Get(leaseId);
            return status.State == state;
        });
        return status!;
    }

    private static async Task WaitUntilAsync(Func<bool> condition)
    {
        var timeout = DateTime.UtcNow + TimeSpan.FromSeconds(3);
        while (!condition())
        {
            if (DateTime.UtcNow >= timeout)
            {
                throw new TimeoutException("capture lease test condition did not settle");
            }
            await Task.Delay(10);
        }
    }

    private static async Task<byte[]> Read(CaptureLeaseFile file)
    {
        await using var input = new FileStream(
            file.Path,
            FileMode.Open,
            FileAccess.Read,
            FileShare.ReadWrite,
            bufferSize: 4_096,
            FileOptions.Asynchronous | FileOptions.SequentialScan);
        input.Position = file.Offset;
        var result = new byte[checked((int)file.Length)];
        await input.ReadExactlyAsync(result);
        return result;
    }

    private static Task ReplyFileAsync(
        HttpListenerResponse response,
        CaptureLeaseFile file,
        CancellationToken cancellationToken)
    {
        var method = typeof(CarbonBridgeMod).GetMethod(
            "ReplyFileAsync",
            BindingFlags.NonPublic | BindingFlags.Static)
            ?? throw new MissingMethodException(nameof(CarbonBridgeMod), "ReplyFileAsync");
        return (Task)(method.Invoke(
            null,
            [response, file, "application/vnd.roblox.rbxm", cancellationToken])
            ?? throw new InvalidOperationException("capture lease response task is unavailable"));
    }

    private static int ReserveLoopbackPort()
    {
        var listener = new TcpListener(IPAddress.Loopback, 0);
        listener.Start();
        var port = ((IPEndPoint)listener.LocalEndpoint).Port;
        listener.Stop();
        return port;
    }

    private static string TemporaryDirectory()
    {
        var directory = Path.Combine(Path.GetTempPath(), "carbon-capture-tests", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(directory);
        return directory;
    }

    private const string CaptureId = "00112233445566778899aabbccddeeff";
    private const string OtherCaptureId = "ffeeddccbbaa99887766554433221100";
    private const string ContractId = "102132435465768798a9bacbdcedfe0f";
    private const string SourceId = "abcdef0123456789abcdef0123456789";
}
