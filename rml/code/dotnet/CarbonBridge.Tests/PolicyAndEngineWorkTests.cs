using System.Collections.Concurrent;

using Roblox;

using Xunit;

namespace Carbon.RmlBridge.Tests;

public sealed class PolicyAndEngineWorkTests
{
	[Fact]
	public void RawDataModelPlaceLoadIsNotReachableOverHttp()
	{
		Assert.False(CarbonBridgeMod.IsDiagnosticRouteSupported("/v1/diagnostics/load-place"));
		Assert.True(CarbonBridgeMod.IsDiagnosticRouteSupported("/v1/diagnostics/rejected-yield"));
		Assert.True(CarbonBridgeMod.IsDiagnosticRouteSupported("/v1/diagnostics/save-local-place"));
	}

    [Theory]
    [InlineData((int)DataModelType.Edit, true)]
    [InlineData((int)DataModelType.Client, false)]
    [InlineData((int)DataModelType.Server, false)]
    [InlineData((int)DataModelType.Standalone, true)]
    [InlineData((int)DataModelType.Null, false)]
    [InlineData(1_097_167_477, true)]
    public void StudioEditCandidateIncludesStandaloneAndUnknownNativeTypes(
        int rawDataModelType,
        bool expected)
    {
        Assert.Equal(
            expected,
            CarbonBridgeMod.IsEditDataModelCandidate((DataModelType)rawDataModelType));
    }

    [Theory]
    [InlineData((int)DataModelType.Edit, false, true)]
    [InlineData((int)DataModelType.Edit, true, false)]
    [InlineData((int)DataModelType.Client, false, false)]
    [InlineData((int)DataModelType.Standalone, false, true)]
    [InlineData(1_097_167_477, false, true)]
    [InlineData(1_097_167_477, true, false)]
    [InlineData((int)DataModelType.Standalone, true, false)]
    public void AuthenticatedEditRouteRejectsReplacementDataModels(
        int rawDataModelType,
        bool hasAuthenticatedEditDataModel,
        bool expected)
    {
        Assert.Equal(
            expected,
            CarbonBridgeMod.ShouldAttachEditDataModelCandidate(
                (DataModelType)rawDataModelType,
                hasAuthenticatedEditDataModel));
    }

    [Theory]
    [InlineData((int)DataModelType.Edit, (int)DataModelType.Edit, true, false)]
    [InlineData((int)DataModelType.Edit, (int)DataModelType.Edit, false, true)]
    [InlineData((int)DataModelType.Edit, (int)DataModelType.Standalone, false, false)]
    [InlineData((int)DataModelType.Edit, 1_097_167_477, false, false)]
    [InlineData((int)DataModelType.Standalone, (int)DataModelType.Edit, false, true)]
    [InlineData((int)DataModelType.Standalone, (int)DataModelType.Standalone, false, true)]
    [InlineData((int)DataModelType.Standalone, 1_097_167_477, false, false)]
    [InlineData(1_097_167_477, (int)DataModelType.Standalone, false, true)]
    public void NativeSelectedUnauthenticatedDataModelPreservesCandidatePriority(
        int rawCurrentDataModelType,
        int rawCandidateDataModelType,
        bool sameDataModel,
        bool expected)
    {
        Assert.Equal(
            expected,
            CarbonBridgeMod.ShouldReplaceUnauthenticatedEditDataModel(
                (DataModelType)rawCurrentDataModelType,
                (DataModelType)rawCandidateDataModelType,
                sameDataModel));
    }

    [Theory]
    [InlineData("session\ninstance", "session", "instance")]
    [InlineData("session-with-dashes\ninstance-with-dashes", "session-with-dashes", "instance-with-dashes")]
    public void StudioRouteRequiresOneNonEmptySeparator(
        string value,
        string expectedSession,
        string expectedInstance)
    {
        var route = Assert.IsType<(string StudioSessionId, string InstanceId)>(
            CarbonBridgeMod.ParseStudioRoute(value));

        Assert.Equal(expectedSession, route.StudioSessionId);
        Assert.Equal(expectedInstance, route.InstanceId);
    }

    [Theory]
    [InlineData("")]
    [InlineData("session")]
    [InlineData("\ninstance")]
    [InlineData("session\n")]
    [InlineData("session\ninstance\nextra")]
    public void StudioRouteRejectsMalformedValues(string value)
    {
        Assert.Null(CarbonBridgeMod.ParseStudioRoute(value));
    }

    [Fact]
    public void StudioRouteKeyIsStableAndSeparatesBothIdentityParts()
    {
        Assert.Equal(
            "fc03bbceded1831a",
            CarbonBridgeMod.StudioRouteKey("studio-session", "studio-instance"));
        Assert.NotEqual(
            CarbonBridgeMod.StudioRouteKey("studio-session", "studio-instance"),
            CarbonBridgeMod.StudioRouteKey("studio-session", "other-instance"));
        Assert.NotEqual(
            CarbonBridgeMod.StudioRouteKey("studio-session", "studio-instance"),
            CarbonBridgeMod.StudioRouteKey("other-session", "studio-instance"));
    }

    [Fact]
    public void ActiveStudioRouteRequiresExactlyOneLiveMarker()
    {
        Assert.Null(CarbonBridgeMod.UniqueStudioRoute(
            Array.Empty<(string StudioSessionId, string InstanceId)>()));
        Assert.Equal(
            ("studio-session", "studio-instance"),
            CarbonBridgeMod.UniqueStudioRoute(
                [("studio-session", "studio-instance")]));
        Assert.Null(CarbonBridgeMod.UniqueStudioRoute(
            [
                ("studio-session", "studio-instance"),
                ("studio-session", "studio-instance")
            ]));
    }

    [Fact]
    public void TransientStudioRouteReadFailurePreservesTheVerifiedCandidate()
    {
        var candidates = new Dictionary<
            nuint,
            (string StudioSessionId, string InstanceId)>
        {
            [42] = ("studio-session", "studio-instance"),
        };

        CarbonBridgeMod.UpdateStudioRouteCandidate(
            candidates,
            42,
            readSucceeded: false,
            route: null);

        Assert.Equal(("studio-session", "studio-instance"), candidates[42]);

        CarbonBridgeMod.UpdateStudioRouteCandidate(
            candidates,
            42,
            readSucceeded: true,
            route: null);

        Assert.Empty(candidates);
    }

    [Fact]
    public void ManifestLedgerResumeRequiresTheSameActiveStudioRoute()
    {
        var detached = ("studio-session", "studio-instance");

        Assert.True(CarbonBridgeMod.CanResumeStudioRoute(41, detached, detached));
        Assert.False(CarbonBridgeMod.CanResumeStudioRoute(0, detached, detached));
        Assert.False(CarbonBridgeMod.CanResumeStudioRoute(
            41,
            detached,
            ("other-session", "other-instance")));
        Assert.False(CarbonBridgeMod.CanResumeStudioRoute(41, detached, null));
        Assert.False(CarbonBridgeMod.CanResumeStudioRoute(41, null, detached));
    }

    [Theory]
    [InlineData("BoolValue", "__CarbonManagedBaselineReady", false, "CoreGui", true, true)]
    [InlineData("StringValue", "__CarbonManagedBaselineReady", false, "CoreGui", true, false)]
    [InlineData("BoolValue", "other", false, "CoreGui", true, false)]
    [InlineData("BoolValue", "__CarbonManagedBaselineReady", true, "CoreGui", true, false)]
    [InlineData("BoolValue", "__CarbonManagedBaselineReady", false, "Folder", true, false)]
    [InlineData("BoolValue", "__CarbonManagedBaselineReady", false, "CoreGui", false, false)]
    public void ManagedSnapshotReadyRequiresTheExactDirectCoreGuiMarker(
        string className,
        string name,
        bool archivable,
        string parentClassName,
        bool parentIsDirectDataModelChild,
        bool expected)
    {
        Assert.Equal(
            expected,
            CarbonBridgeMod.IsManagedBaselineReadyMarker(
                className,
                name,
                archivable,
                parentClassName,
                parentIsDirectDataModelChild));
    }

    [Fact]
    public void ManagedStartupAttestationUsesShortSettlingWindowAndRetriesUntilReady()
    {
        Assert.Equal(
            TimeSpan.FromMilliseconds(500),
            CarbonBridgeMod.ManagedSnapshotQuietPeriodFor(false));
        Assert.Equal(
            TimeSpan.FromMilliseconds(250),
            CarbonBridgeMod.ManagedSnapshotQuietPeriodFor(true));
        Assert.Equal(
            TimeSpan.FromMilliseconds(100),
            CarbonBridgeMod.ManagedSnapshotRetryPeriod);
        Assert.Equal(
            TimeSpan.FromSeconds(30),
            CarbonBridgeMod.ManagedSnapshotReadinessTimeout);
    }

    [Fact]
    public void IdenticalManagedRestagePreservesTheInFlightContractReference()
    {
        var source = new ManagedSourceNode[]
        {
            new("game", "", "DataModel", "Place"),
            new("workspace", "game", "Workspace", "Workspace", ParentIndex: 0),
        };
        var inFlight = new ManagedStageFixture("0123456789abcdef0123456789abcdef", source);
        var duplicate = new ManagedStageFixture(
            inFlight.ContractId,
            source.Select(node => node with { }).ToArray());

        var retained = CarbonBridgeMod.RetainIdempotentManagedStage(
            inFlight,
            duplicate,
            static stage => stage.ContractId,
            static stage => stage.Source);

        Assert.Same(inFlight, retained);
    }

    [Fact]
    public void FortyThousandManifestNodesRetainOnlyMappedObservationDetails()
    {
        const int manifestNodes = 40_000;
        const int mappedNodes = 2;
        var hierarchyAttestations = 0;
        var changeAttestations = 0;
        var retainedInstances = 0;
        var journalRows = 0;

        for (var index = 0; index < manifestNodes; index++)
        {
            var plan = CarbonBridgeMod.PlanObservationRetention(
                isPersistentAuthoredMutation: true,
                isMapped: index < mappedNodes,
                isHierarchyMutation: true,
                recordsNativeChange: true);
            hierarchyAttestations += plan.AttestHierarchy ? 1 : 0;
            changeAttestations += plan.AttestChange ? 1 : 0;
            retainedInstances += plan.RetainDetails ? 1 : 0;
            journalRows += plan.RetainDetails ? 1 : 0;
        }

        Assert.Equal(manifestNodes, hierarchyAttestations);
        Assert.Equal(manifestNodes, changeAttestations);
        Assert.Equal(mappedNodes, retainedInstances);
        Assert.Equal(mappedNodes, journalRows);
    }

    [Fact]
    public void ZeroMappingWorkspaceRenameNeverEntersManagedResolutionOrDetailState()
    {
        const int manifestNodes = 40_000;
        var source = new List<ManagedSourceNode>(manifestNodes + 2)
        {
            new("game", "", "DataModel", "Place", ParentIndex: -1),
            new("workspace", "game", "Workspace", "Workspace", ParentIndex: 0),
        };
        source.AddRange(Enumerable.Range(0, manifestNodes).Select(index =>
            new ManagedSourceNode(
                $"folder-{index}",
                "workspace",
                "Folder",
                $"Folder{index}",
                ParentIndex: 1)));

        var owned = ManagedHierarchy.ExpandOwnedSourceIds(source, []);
        var resolutionAttempts = 0;
        if (CarbonBridgeMod.ShouldResolveManagedObservation("workspace", owned)
            || CarbonBridgeMod.ShouldResolveManagedObservation("folder-39999", owned))
        {
            resolutionAttempts++;
        }
        var plan = CarbonBridgeMod.PlanObservationRetention(
            isPersistentAuthoredMutation: true,
            isMapped: resolutionAttempts != 0,
            isHierarchyMutation: true,
            recordsNativeChange: true);

        Assert.Empty(owned);
        Assert.Equal(0, resolutionAttempts);
        Assert.True(plan.AttestHierarchy);
        Assert.True(plan.AttestChange);
        Assert.False(plan.RetainDetails);
    }

    [Fact]
    public void RuntimeEditorMutationsNeitherAdvanceNorRetainObservationState()
    {
        var plan = CarbonBridgeMod.PlanObservationRetention(
            isPersistentAuthoredMutation: false,
            isMapped: true,
            isHierarchyMutation: true,
            recordsNativeChange: true);

        Assert.False(plan.AttestHierarchy);
        Assert.False(plan.AttestChange);
        Assert.False(plan.RetainDetails);
    }

    [Fact]
    public void DelayedLaunchAttributeNotificationIsIgnoredOnlyAtTheExactAttachedBaseline()
    {
        var descriptor = new SerializedPropertyDescriptor(
            "AttributesSerialize",
            "BinaryString",
            SerializedPropertyAttributes.Accessible);
        var baseline = new Dictionary<string, SerializedPropertySnapshot>(StringComparer.Ordinal)
        {
            ["AttributesSerialize"] = new(descriptor, [1, 2, 3]),
        };

        Assert.True(CarbonBridgeMod.MatchesLaunchBaselineProperty(
            "Attributes",
            descriptor,
            [1, 2, 3],
            baseline));
        Assert.False(CarbonBridgeMod.MatchesLaunchBaselineProperty(
            "Attributes",
            descriptor,
            [1, 2, 4],
            baseline));
        Assert.False(CarbonBridgeMod.MatchesLaunchBaselineProperty(
            "Attributes",
            descriptor with { TypeName = "String" },
            [1, 2, 3],
            baseline));
        Assert.False(CarbonBridgeMod.MatchesLaunchBaselineProperty(
            "Other",
            descriptor,
            [1, 2, 3],
            baseline));
    }

    [Theory]
    [InlineData("String", SerializedPropertyAttributes.Accessible, true, true)]
    [InlineData("String", SerializedPropertyAttributes.Accessible | SerializedPropertyAttributes.Excluded, false, false)]
    [InlineData("Object", SerializedPropertyAttributes.Reference, false, true)]
    [InlineData("NetAssetRef", SerializedPropertyAttributes.None, true, true)]
    [InlineData("PhysicalProperties", SerializedPropertyAttributes.None, true, true)]
    [InlineData("NumberSequence", SerializedPropertyAttributes.None, true, true)]
    [InlineData("NetAssetRef", SerializedPropertyAttributes.Excluded, false, false)]
    [InlineData("String", SerializedPropertyAttributes.None, false, false)]
    public void CaptureAndObservationShareTheExactSerializedPropertyPolicy(
        string typeName,
        SerializedPropertyAttributes attributes,
        bool canRead,
        bool canObserve)
    {
        var descriptor = new SerializedPropertyDescriptor("Fixture", typeName, attributes);

        Assert.Equal(canRead, CarbonBridgeMod.CanReadForCapture(descriptor));
        Assert.Equal(canObserve, CarbonBridgeMod.CanObserve("Fixture", descriptor));
    }

    [Theory]
    [InlineData("NetAssetRef", SerializedPropertyAttributes.Accessible, true)]
    [InlineData("NumberSequence", SerializedPropertyAttributes.None, true)]
    [InlineData("String", SerializedPropertyAttributes.Accessible, false)]
    public void ModelOnlyPropertiesUseExactClassCarriersInsteadOfCloningTheirOwner(
        string typeName,
        SerializedPropertyAttributes attributes,
        bool expected)
    {
        var descriptor = new SerializedPropertyDescriptor("Fixture", typeName, attributes);
        Assert.Equal(expected, CarbonBridgeMod.UsesSerializedPropertyCarrier(descriptor));
    }

    [Fact]
    public void PhysicalPropertiesUseAParentablePartCarrierForSpecialEngineInstances()
    {
        var physical = new SerializedPropertyDescriptor(
            "CustomPhysicalProperties",
            "PhysicalProperties",
            SerializedPropertyAttributes.None);
        var asset = new SerializedPropertyDescriptor(
            "MeshContent",
            "NetAssetRef",
            SerializedPropertyAttributes.Accessible);

        Assert.Equal("Part", CarbonBridgeMod.SerializedPropertyCarrierClass("Terrain", physical));
        Assert.Equal("MeshPart", CarbonBridgeMod.SerializedPropertyCarrierClass("MeshPart", asset));
    }

    [Fact]
    public async Task StaleGenerationWorkNeverRunsAgainstTheReplacementDataModel()
    {
        var invoked = false;
        var completion = new TaskCompletionSource<object?>(TaskCreationOptions.RunContinuationsAsynchronously);
        var work = new CarbonBridgeMod.EngineWork(7, () =>
        {
            invoked = true;
            return null;
        }, completion);

        work.Run(8);

        var error = await Assert.ThrowsAsync<InvalidOperationException>(async () => await completion.Task);
        Assert.Contains("detached DataModel session", error.Message);
        Assert.False(invoked);
    }

    [Fact]
    public async Task AFullBatchLeavesWorkForTheNextWake()
    {
        var queue = new ConcurrentQueue<CarbonBridgeMod.EngineWork>();
        var completions = new List<TaskCompletionSource<object?>>();
        var invoked = 0;
        for (var index = 0; index < CarbonBridgeMod.EngineWorkBatchSize + 1; index++)
        {
            var completion = new TaskCompletionSource<object?>(TaskCreationOptions.RunContinuationsAsynchronously);
            completions.Add(completion);
            queue.Enqueue(new CarbonBridgeMod.EngineWork(11, () =>
            {
                Interlocked.Increment(ref invoked);
                return null;
            }, completion));
        }

        Assert.Equal(
            CarbonBridgeMod.EngineWorkBatchSize,
            CarbonBridgeMod.DrainEngineWorkBatch(queue, 11));
        Assert.Equal(CarbonBridgeMod.EngineWorkBatchSize, invoked);
        Assert.Single(queue);

        Assert.Equal(1, CarbonBridgeMod.DrainEngineWorkBatch(queue, 11));
        Assert.Empty(queue);
        await Task.WhenAll(completions.Select(completion => completion.Task));
        Assert.Equal(CarbonBridgeMod.EngineWorkBatchSize + 1, invoked);
    }

    [Fact]
    public void RecaptureReplacesTheOwnedMarkerAndPublishesAFreshAddTransition()
    {
        string? marker = "dead-endpoint-marker";
        var lifecycle = new List<string>();

        LiveSessionMarkerLifecycle.Replace(
            ref marker,
            previous => lifecycle.Add($"removed:{previous}"),
            () =>
            {
                lifecycle.Add("added:fresh-endpoint-marker");
                return "fresh-endpoint-marker";
            });

        Assert.Equal("fresh-endpoint-marker", marker);
        Assert.Equal(
            ["removed:dead-endpoint-marker", "added:fresh-endpoint-marker"],
            lifecycle);
    }

    [Fact]
    public void CapabilitiesAdvertiseEveryCanonicalExactRawType()
    {
        Assert.Equal(
            new HashSet<string>
            {
                "Int64", "Float32", "Float64", "CFrame", "Color3", "ColorSequence",
                "OptionalCFrame", "NumberRange", "NumberSequence", "PhysicalProperties",
                "Ray", "Rect", "Region3", "UDim", "UDim2", "Vector2", "Vector3", "Vector3int16",
            },
            CarbonBridgeMod.ExactRawTypes.ToHashSet());
    }

    [Fact]
    public void CapabilitiesAdvertiseSerializedReferenceTransport()
    {
        Assert.True(CarbonBridgeMod.SerializedReferences);
    }

    [Theory]
    [InlineData(SerializedPropertyAttributes.Reference, true)]
    [InlineData(SerializedPropertyAttributes.Reference | SerializedPropertyAttributes.Accessible, true)]
    [InlineData(SerializedPropertyAttributes.Reference | SerializedPropertyAttributes.Excluded, false)]
    [InlineData(SerializedPropertyAttributes.Accessible, false)]
    public void SerializedReferenceTransportRequiresANonExcludedReferenceDescriptor(
        SerializedPropertyAttributes attributes,
        bool expected)
    {
        var descriptor = new SerializedPropertyDescriptor("Fixture", "Object", attributes);
        Assert.Equal(expected, CarbonBridgeMod.CanTransportReference(descriptor));
    }

    [Fact]
    public void OptionalReferenceTargetsKeepNullDistinctFromUnknownDebugIds()
    {
        var resolutions = 0;
        Assert.Null(CarbonBridgeMod.ResolveOptionalReferenceTarget<object>(null, _ =>
        {
            resolutions++;
            return new object();
        }));
        Assert.Equal(0, resolutions);

        var target = new object();
        Assert.Same(target, CarbonBridgeMod.ResolveOptionalReferenceTarget("known", id =>
        {
            resolutions++;
            return id == "known" ? target : throw new KeyNotFoundException(id);
        }));
        Assert.Equal(1, resolutions);
        Assert.Throws<KeyNotFoundException>(() =>
            CarbonBridgeMod.ResolveOptionalReferenceTarget<object>(
                "outside",
                _ => throw new KeyNotFoundException("outside")));
    }

    [Theory]
    [InlineData("PhysicalProperties", SerializedPropertyAttributes.Accessible, true)]
    [InlineData("Color3", SerializedPropertyAttributes.Accessible, false)]
    public void MaterializedWritesAreLimitedToAccessibleModelSerializedTypes(
        string typeName,
        SerializedPropertyAttributes attributes,
        bool expected)
    {
        var descriptor = new SerializedPropertyDescriptor("Fixture", typeName, attributes);
        Assert.Equal(expected, CarbonBridgeMod.CanWriteMaterialized("Fixture", descriptor));
    }

    [Theory]
    [InlineData("Chat", "LoadDefaultChat")]
    [InlineData("HttpService", "HttpEnabled")]
    [InlineData("Lighting", "LightingStyle")]
    [InlineData("Lighting", "PrioritizeLightingQuality")]
    [InlineData("MeshPart", "HasJointOffset")]
    [InlineData("MeshPart", "HasSkinnedMesh")]
    [InlineData("MeshPart", "JointOffset")]
    [InlineData("MeshPart", "MeshContent")]
    [InlineData("PackageLink", "DefaultName")]
    [InlineData("PackageLink", "PackageContent")]
    [InlineData("Players", "MaxPlayers")]
    [InlineData("Players", "PreferredPlayers")]
    [InlineData("StarterPlayer", "AllowCustomAnimations")]
    [InlineData("TextChatService", "ChatVersion")]
    public void PersistedReadOnlyPropertiesUseTheEngineMaterializationPath(
        string className,
        string propertyName)
    {
        var descriptor = new SerializedPropertyDescriptor(
            propertyName,
            "EngineMaterialized",
            SerializedPropertyAttributes.ReadOnly | SerializedPropertyAttributes.XmlRead);

        Assert.True(CarbonBridgeMod.CanWriteMaterialized(className, descriptor));
        Assert.True(CarbonBridgeMod.CanObserve(className, descriptor));
        Assert.False(CarbonBridgeMod.CanWriteMaterialized(
            className,
            descriptor with { Attributes = descriptor.Attributes | SerializedPropertyAttributes.Excluded }));
        Assert.False(CarbonBridgeMod.CanObserve(
            className,
            descriptor with { Attributes = descriptor.Attributes | SerializedPropertyAttributes.Excluded }));
    }

    [Theory]
    [InlineData("Fixture", "LoadDefaultChat")]
    [InlineData("Chat", "HttpEnabled")]
    [InlineData("PartOperation", "TriangleCount")]
    [InlineData("AudioRecorder", "IsRecording")]
    public void MaterializedReadOnlyPolicyDoesNotGrowByNameOrScriptability(
        string className,
        string propertyName)
    {
        var descriptor = new SerializedPropertyDescriptor(
            propertyName,
            "EngineMaterialized",
            SerializedPropertyAttributes.ReadOnly | SerializedPropertyAttributes.XmlRead);

        Assert.False(CarbonBridgeMod.CanWriteMaterialized(className, descriptor));
        Assert.False(CarbonBridgeMod.CanObserve(className, descriptor));
    }

    [Theory]
    [InlineData("Content", SerializedPropertyAttributes.None, true)]
	[InlineData("Content", SerializedPropertyAttributes.ReadOnly | SerializedPropertyAttributes.XmlRead, true)]
	[InlineData("bool", SerializedPropertyAttributes.ReadOnly | SerializedPropertyAttributes.XmlRead, true)]
    [InlineData("Object", SerializedPropertyAttributes.Reference, true)]
    [InlineData("EnginePrivate", SerializedPropertyAttributes.None, true)]
    [InlineData("Content", SerializedPropertyAttributes.Excluded, false)]
    public void EngineMaterializedRootCopiesKeepEveryNonDynamicSerializedType(
        string typeName,
        SerializedPropertyAttributes attributes,
        bool expected)
    {
        var descriptor = new SerializedPropertyDescriptor("Fixture", typeName, attributes);
        Assert.Equal(expected, CarbonBridgeMod.CanCopyFromModel(descriptor));
    }

    private sealed record ManagedStageFixture(
        string ContractId,
        IReadOnlyList<ManagedSourceNode> Source);
}
