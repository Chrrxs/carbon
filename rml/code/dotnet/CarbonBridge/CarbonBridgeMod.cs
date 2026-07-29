using System.Collections.Concurrent;
using System.Buffers.Binary;
using System.Diagnostics;
using System.Globalization;
using System.Net;
using System.Net.NetworkInformation;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

using RML.Core.Api;
using RML.Core.Modding;
using Roblox;

using IOFile = System.IO.File;
using IOPath = System.IO.Path;

namespace Carbon.RmlBridge;

internal readonly record struct ObservationRetentionPlan(
    bool AttestHierarchy,
    bool AttestChange,
    bool RetainDetails);

[Mod(
    "carbon-serialized-property-bridge",
    "1.0.0",
    Author = "Carbon",
    Description = "Authenticated, serialized-property-only bridge for the Carbon Studio plugin"
)]
public sealed class CarbonBridgeMod : ModBase, IDataModelAware
{
    private const int ProtocolVersion = 2;
    private const string BridgeIdEnvironmentVariable = "CARBON_RML_BRIDGE_ID";
    internal const int EngineWorkBatchSize = 256;
    private const string StudioRouteMarker = "__CarbonStudioRoute";
    private const string ManagedBaselineReadyMarker = "__CarbonManagedBaselineReady";
    private const string RootPropertyWrapperPrefix = "__CarbonRootProperty:";
    private static readonly TimeSpan ManagedSnapshotQuietPeriod = TimeSpan.FromMilliseconds(500);
    private static readonly TimeSpan AttestedManagedSnapshotQuietPeriod = TimeSpan.FromMilliseconds(250);
    internal static readonly TimeSpan ManagedSnapshotRetryPeriod = TimeSpan.FromMilliseconds(100);
    internal static readonly TimeSpan ManagedSnapshotReadinessTimeout = TimeSpan.FromSeconds(30);

    [DllImport("roblox_modloader.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern nint carbon_rml_build_version();

    private static string AttestedRmlBuildVersion()
    {
        var buildVersion = Marshal.PtrToStringUTF8(carbon_rml_build_version());
        if (string.IsNullOrWhiteSpace(buildVersion))
        {
            throw new InvalidOperationException("the native RML build did not attest its version");
        }
        return buildVersion;
    }

    internal static bool IsExcludedEditCameraReference(
        string ownerClass,
        string property,
        nuint targetHandle,
        nuint excludedEditCameraHandle) =>
        excludedEditCameraHandle != 0
        && targetHandle == excludedEditCameraHandle
        && string.Equals(ownerClass, "Workspace", StringComparison.Ordinal)
        && string.Equals(property, "CurrentCamera", StringComparison.Ordinal);

    internal static bool IsEngineOwnedScriptNormalizationProperty(
        string className,
        string propertyName) =>
        (className is "Script" or "LocalScript" or "ModuleScript")
        && (propertyName is "Capabilities" or "LinkedSource" or "Sandboxed" or "SourceAssetId");

    internal static bool IsEngineOwnedWorkspaceNormalizationProperty(
        string className,
        string instanceName,
        string propertyName,
        ReadOnlySpan<byte> value) =>
        string.Equals(className, "Workspace", StringComparison.Ordinal)
        && string.Equals(instanceName, "Workspace", StringComparison.Ordinal)
        && string.Equals(propertyName, "PredictiveStreamingMode", StringComparison.Ordinal)
        && value.Length == sizeof(uint)
        && value.IndexOfAnyExcept((byte)0) < 0;

    internal static ObservationRetentionPlan PlanObservationRetention(
        bool isPersistentAuthoredMutation,
        bool isMapped,
        bool isHierarchyMutation,
        bool recordsNativeChange) => new(
            AttestHierarchy: isPersistentAuthoredMutation && isHierarchyMutation,
            AttestChange: isPersistentAuthoredMutation && recordsNativeChange,
            RetainDetails: isPersistentAuthoredMutation && isMapped);

    internal static bool ShouldResolveManagedObservation(
        string sourceId,
        IReadOnlySet<string> ownedSourceIds) => ownedSourceIds.Contains(sourceId);

    internal static bool IsRequestedCaptureShellProperty(
        string ownerClass,
        string property,
        IReadOnlyDictionary<string, string[]> shellSchema) =>
        shellSchema.TryGetValue(ownerClass, out var properties)
        && properties.Contains(property, StringComparer.Ordinal);

    internal static bool ShouldIncludeCaptureExternalReference(
        bool ownerIsShell,
        string ownerClass,
        string property,
        IReadOnlyDictionary<string, string[]> shellSchema) =>
        !ownerIsShell
        || IsRequestedCaptureShellProperty(ownerClass, property, shellSchema);

	internal static bool CrossesCaptureOwnershipBarrier(
		bool ownerIsMapped,
		bool ownerIsManifest,
		bool targetIsMapped,
		bool targetIsManifest) =>
		ownerIsMapped && targetIsManifest;

    internal static nuint[] SelectUnboundCaptureReferenceHandles(
        IEnumerable<nuint> mappedReferenceHandles,
        IReadOnlyDictionary<nuint, string> knownSourceIdsByHandle) =>
        mappedReferenceHandles
            .Where(handle => handle != 0 && !knownSourceIdsByHandle.ContainsKey(handle))
            .Distinct()
            .Order()
            .ToArray();

    internal static object?[] CaptureSerializerArguments(Instance[] roots) =>
        [(IReadOnlyList<Instance>)roots];

    internal static CaptureMappedBinding[] SelectCaptureMappedBindings(
        IReadOnlyList<string> requestedSourceIds,
        IReadOnlyDictionary<string, CaptureMappedBinding> plannedBindings)
    {
        var selected = new List<CaptureMappedBinding>(requestedSourceIds.Count);
        foreach (var sourceId in requestedSourceIds.Order(StringComparer.Ordinal))
        {
            if (!plannedBindings.TryGetValue(sourceId, out var binding))
            {
                throw new InvalidOperationException(
                    $"mapped capture root {sourceId} has no verified graft anchor");
            }
            selected.Add(binding);
        }
        return selected.ToArray();
    }

    internal static async Task<T> AwaitCaptureSerializerWithRestoration<T>(
        Task<T> serialization,
        Func<Task> restore)
    {
        Exception? serializationFailure = null;
        T result = default!;
        try
        {
            result = await serialization.ConfigureAwait(false);
        }
        catch (Exception ex)
        {
            serializationFailure = ex;
        }

        try
        {
            await restore().ConfigureAwait(false);
        }
        catch (Exception restorationFailure)
        {
            var explicitRestorationFailure = new InvalidOperationException(
                "capture cleanup and Archivable restoration failed",
                restorationFailure);
            if (serializationFailure is not null)
            {
                throw new AggregateException(
                    "capture serialization and Archivable restoration both failed",
                    serializationFailure,
                    explicitRestorationFailure);
            }
            throw explicitRestorationFailure;
        }

        if (serializationFailure is not null)
        {
            System.Runtime.ExceptionServices.ExceptionDispatchInfo
                .Capture(serializationFailure)
                .Throw();
        }
        return result;
    }

    internal static void EnsureCaptureLeaseEpochsUnchanged(
        long expectedHierarchySequence,
        long expectedChangeSequence,
        long actualHierarchySequence,
        long actualChangeSequence,
        string phase)
    {
        if (expectedHierarchySequence != actualHierarchySequence
            || expectedChangeSequence != actualChangeSequence)
        {
            throw new InvalidOperationException($"edit DataModel changed {phase}");
        }
    }

    internal static bool IsExpectedClientDisconnect(Exception exception) => exception switch
    {
        HttpListenerException listener => listener.ErrorCode is 64 or 995 or 1229,
        SocketException socket => socket.SocketErrorCode is
            SocketError.ConnectionAborted or
            SocketError.ConnectionReset or
            SocketError.NotConnected or
            SocketError.Shutdown,
        IOException { InnerException: Exception inner } => IsExpectedClientDisconnect(inner),
        _ => false,
    };
    private static readonly TimeSpan ManagedMovePairWindow = TimeSpan.FromMilliseconds(100);
    private static readonly JsonSerializerOptions JsonOptions = new(JsonSerializerDefaults.Web);
    internal static readonly string[] ExactRawTypes =
    [
        "Int64", "Float32", "Float64", "CFrame", "Color3", "ColorSequence",
        "OptionalCFrame", "NumberRange", "NumberSequence", "PhysicalProperties",
        "Ray", "Rect", "Region3", "UDim", "UDim2", "Vector2", "Vector3", "Vector3int16",
    ];
    internal const bool SerializedReferences = true;
    internal const bool ManagedHierarchyAttachment = true;
    internal const bool ManifestIdentityLedgerSupported = true;
    private const string ManagedIdentityMarkerPrefix = "__CarbonIdentity:";

    private readonly ConcurrentDictionary<string, Instance> _instances = new(StringComparer.Ordinal);
    private readonly ConcurrentQueue<EngineWork> _engineWork = new();
    private readonly object _discoveryWriteLock = new();
    private readonly object _engineStateLock = new();
    private readonly object _changesLock = new();
    private readonly List<PropertyChange> _changes = [];
    private readonly List<BridgeDiagnostic> _diagnostics = [];
    private readonly SemaphoreSlim _changesReady = new(0);
    private readonly object _managedHierarchyLock = new();
    private readonly object _manifestIdentityLock = new();
    private readonly ManifestIdentityLedger _manifestIdentities = new();
    private readonly CaptureDirtyPageTable _captureDirtyPages = new();
    private ManifestIdentityRemapSession? _manifestIdentityRemap;
    private readonly CaptureArchivableMaskTracker _captureArchivableMasks = new();
    private readonly object _managedIdentityResolutionLock = new();
    private readonly Dictionary<string, ManagedHierarchyBinding> _managedBySource = new(StringComparer.Ordinal);
    private readonly Dictionary<string, ManagedHierarchyBinding> _managedByRuntime = new(StringComparer.Ordinal);
    private readonly Dictionary<string, ManagedHierarchyBinding> _managedByDebug = new(StringComparer.Ordinal);
    private readonly HashSet<string> _managedOwnershipRoots = new(StringComparer.Ordinal);
    private readonly HashSet<string> _managedOwnedSourceIds = new(StringComparer.Ordinal);
    private readonly Dictionary<nuint, LaunchHydratedServiceDefaults> _launchHydratedRootDefaults = [];
    private readonly HashSet<nuint> _pendingLaunchHydratedRootDefaultRefreshes = [];
    private readonly Dictionary<nuint, LaunchHydratedServiceDefaults> _attachedManagedRootBaselines = [];
    private string[] _launchHydratedDefaultFailures = [];
    private readonly ManagedBindingReleaseGate _managedBindingReleases = new();
    private readonly Dictionary<string, Task<object>> _managedIdentityResolutions = new(StringComparer.Ordinal);
    private readonly Dictionary<nuint, (string StudioSessionId, string InstanceId)> _studioIdentityCandidates = [];
    private (string StudioSessionId, string InstanceId)? _preservedStudioRoute;

    private CancellationTokenSource? _shutdown;
    private HttpListener? _listener;
    private Task? _listenerTask;
    private TcpListener? _wslProxy;
    private Task? _wslProxyTask;
    private Timer? _launchHydratedDefaultsTimer;
    private CaptureLeaseManager? _captureLeases;
    private DataModel? _dataModel;
    private nuint _detachedEditDataModelHandle;
    private SerializedPropertyAccess.EngineThreadPump? _engineThreadPump;
    private Timer? _managedSnapshotTimer;
    private IDisposable? _propertyObservation;
    private Instance? _liveSessionMarker;
    private string _token = string.Empty;
    private string _bridgeId = string.Empty;
    private string _endpoint = string.Empty;
    private string? _wslEndpoint;
    private string _discoveryPath = string.Empty;
    private string _routeDiscoveryPath = string.Empty;
    private long _changeSequence;
    private long _hierarchySequence;
    private nint _excludedEditCameraHandle;
    private long _engineGeneration;
    private long _managedSnapshotLastHierarchyChange;
    private int _engineDrainActive;
    private int _managedSnapshotPending;
    private int _managedStartupBoundaryAttested;
    private StudioIdentity? _studioIdentity;
    private ManagedSourceContract? _stagedManagedSource;
    private ManagedAttachmentReceipt? _attachedManagedContract;
    private ManagedRuntimeSnapshot? _loadedHierarchy;
    private List<ManagedHierarchyChange>? _preVerificationHierarchyChanges;
    private TaskCompletionSource<bool> _managedSnapshotReady = NewManagedSnapshotReady();

    private static TaskCompletionSource<bool> NewManagedSnapshotReady() =>
        new(TaskCreationOptions.RunContinuationsAsynchronously);

    public override int OnLoad()
    {
        _shutdown = new CancellationTokenSource();
        _launchHydratedDefaultsTimer = new Timer(
            OnLaunchHydratedDefaultsTimer,
            null,
            Timeout.InfiniteTimeSpan,
            Timeout.InfiniteTimeSpan);
        _token = Convert.ToHexString(RandomNumberGenerator.GetBytes(32)).ToLowerInvariant();
        _bridgeId = ResolveBridgeId(
            Environment.GetEnvironmentVariable(BridgeIdEnvironmentVariable),
            () => Convert.ToHexString(RandomNumberGenerator.GetBytes(16)).ToLowerInvariant());
        try
        {
            var removed = PruneStaleDiscoveryRecords(DiscoveryRoot(), IsStudioProcessRunning);
            if (removed != 0)
            {
                Logger.Info($"Pruned {removed} stale Carbon bridge discovery record(s)");
            }
        }
        catch (Exception ex)
        {
            Logger.Warn($"Failed to prune stale Carbon bridge discovery records: {ex.Message}");
        }
        _captureLeases = new CaptureLeaseManager(
            IOPath.Combine(
                Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
                "RobloxModLoader",
                "carbon-capture-leases",
                _bridgeId),
            CaptureLeaseSnapshotAsync);

        var port = ReserveLoopbackPort();
        _endpoint = $"http://127.0.0.1:{port}/";
        _listener = new HttpListener();
        _listener.Prefixes.Add(_endpoint);
        _listener.Start();
        _listenerTask = Task.Run(() => ListenAsync(_shutdown.Token));
        _wslEndpoint = StartWslProxy(port, _shutdown.Token);

        _discoveryPath = DiscoveryPath();
        Directory.CreateDirectory(IOPath.GetDirectoryName(_discoveryPath)!);
        WriteDiscovery(null);
        Logger.Info(_wslEndpoint is null
            ? $"Carbon bridge listening on {_endpoint}"
            : $"Carbon bridge listening on {_endpoint} (WSL proxy {_wslEndpoint})");
        return 0;
    }

    public void OnDataModelLoaded(DataModel dataModel, DataModelType dataModelType)
    {
        bool hasAuthenticatedEditDataModel;
        lock (_engineStateLock)
        {
            hasAuthenticatedEditDataModel = _dataModel is not null && _studioIdentity is not null;
        }
        if (!ShouldAttachEditDataModelCandidate(dataModelType, hasAuthenticatedEditDataModel))
        {
            if (IsEditDataModelCandidate(dataModelType))
            {
                Logger.Info(
                    $"Ignoring unknown DataModel type {(int)dataModelType}; " +
                    "an authenticated edit DataModel is already attached");
            }
            return;
        }
        if (dataModelType != DataModelType.Edit)
        {
            // Roblox's private DataModel layout can move before the loader's
            // next offset update. An out-of-domain value is allowed to attach
            // provisionally until the unique CoreGui route authenticates it.
            Logger.Info(
                $"RML reported unknown DataModel type {(int)dataModelType}; " +
                "probing it as an edit candidate until Studio routing is established");
        }

        var dataModelHandle = InstanceHierarchy.RuntimeHandle(dataModel);
        nuint detachedDataModelHandle;
        (string StudioSessionId, string InstanceId)? detachedStudioRoute;
        lock (_engineStateLock)
        {
            detachedDataModelHandle = _detachedEditDataModelHandle;
            detachedStudioRoute = _preservedStudioRoute;
            _detachedEditDataModelHandle = 0;
            _preservedStudioRoute = null;
        }

        _dataModel = dataModel;
        // Exact-property requests identify every snapshot node by Studio's
        // debug identity. DataModel itself is not a descendant, so it never
        // reaches the cache through the hierarchy observation callbacks.
        // Register it explicitly so full-place capture can hydrate the source
        // root without treating the live edit DataModel as unavailable.
        _instances[dataModel.GetDebugId(128)] = dataModel;
        RememberEditCamera(dataModel);
        ResetChanges();
        _captureDirtyPages.Reset();
        Interlocked.Exchange(ref _hierarchySequence, 0);
        // A serve-built Studio place is the expected source. Managed startup
        // attaches the staged source identities directly; it does not snapshot
        // or verify the already-built hierarchy on connection.
        Interlocked.Exchange(ref _managedSnapshotPending, 0);
        Interlocked.Exchange(ref _managedStartupBoundaryAttested, 0);
        Interlocked.Exchange(ref _managedSnapshotLastHierarchyChange, Stopwatch.GetTimestamp());
        TaskCompletionSource<bool> previousManagedSnapshotReady;
        lock (_managedHierarchyLock)
        {
            previousManagedSnapshotReady = _managedSnapshotReady;
            _managedSnapshotReady = NewManagedSnapshotReady();
            _managedBySource.Clear();
            _managedByRuntime.Clear();
            _managedByDebug.Clear();
            _managedOwnershipRoots.Clear();
            _managedOwnedSourceIds.Clear();
            _launchHydratedRootDefaults.Clear();
            _pendingLaunchHydratedRootDefaultRefreshes.Clear();
            _attachedManagedRootBaselines.Clear();
            _launchHydratedDefaultFailures = [];
            _stagedManagedSource = null;
            _attachedManagedContract = null;
            _loadedHierarchy = null;
            _preVerificationHierarchyChanges = null;
        }
        _managedBindingReleases.Clear();
        lock (_managedIdentityResolutionLock)
        {
            _managedIdentityResolutions.Clear();
        }
        previousManagedSnapshotReady.TrySetCanceled();

        // Observe immediately, but do not freeze the hierarchy until Roblox has
        // materialized the standard Workspace Terrain service. The pointer-
        // attachment callback precedes persisted Workspace children, and the
        // DataModel.Loaded reflection signal is not reliable in Studio edit mode.
        dataModel.DescendantAdded += OnDescendantAdded;
        dataModel.DescendantRemoving += OnDescendantRemoving;
        _propertyObservation = SerializedPropertyAccess.Observe(dataModel, OnItemChanged);

        var engineThreadPump = SerializedPropertyAccess.PumpEngineThread(
            dataModel,
            DrainEngineWork);
        var managedSnapshotTimer = new Timer(
            OnManagedSnapshotTimer,
            null,
            Timeout.InfiniteTimeSpan,
            Timeout.InfiniteTimeSpan);
        Timer? previousManagedSnapshotTimer;
        lock (_engineStateLock)
        {
            _engineGeneration++;
            _studioIdentityCandidates.Clear();
            _studioIdentity = null;
            _engineThreadPump = engineThreadPump;
            previousManagedSnapshotTimer = _managedSnapshotTimer;
            _managedSnapshotTimer = managedSnapshotTimer;
        }
        previousManagedSnapshotTimer?.Dispose();
        PublishStudioIdentity(null);

        // Studio can preserve the plugin-owned CoreGui route marker while
        // unloading and reattaching the edit DataModel around a playtest. In
        // that case DescendantAdded has already fired, so seed the candidates
        // from the existing direct children before serving bridge requests.
        foreach (var child in dataModel.GetService<CoreGui>().GetChildren())
        {
            TryCacheStudioIdentity(child);
        }

        (string StudioSessionId, string InstanceId)? activeStudioRoute;
        lock (_engineStateLock)
        {
            activeStudioRoute = UniqueStudioRoute(_studioIdentityCandidates.Values);
            _preservedStudioRoute = activeStudioRoute;
        }
        IReadOnlyList<ManagedRuntimeNode>? activeHierarchy = null;
        if (CanResumeStudioRoute(detachedDataModelHandle, detachedStudioRoute, activeStudioRoute))
        {
            try
            {
                activeHierarchy = ManagedHierarchy.ParseRuntime(InstanceHierarchy.Read(dataModel));
            }
            catch (Exception error)
            {
                Logger.Warn($"Retained manifest identity validation failed: {error.Message}");
            }
        }
        lock (_manifestIdentityLock)
        {
            if (activeHierarchy is not null
                && _manifestIdentities.MatchesRetainedAttachment(
                    activeHierarchy.Select(node => node.Handle),
                    detachedDataModelHandle,
                    dataModelHandle))
            {
                _manifestIdentities.RebindHandle(detachedDataModelHandle, dataModelHandle);
            }
            else
            {
                _manifestIdentities.Reset();
            }
            _manifestIdentityRemap = null;
        }
    }

    public void OnDataModelUnloaded(DataModel dataModel, DataModelType dataModelType)
    {
        DetachDataModel(dataModel);
    }

    public override void OnUnload()
    {
        if (_dataModel is not null)
        {
            DetachDataModel(_dataModel);
        }

        _shutdown?.Cancel();
        _launchHydratedDefaultsTimer?.Dispose();
        _launchHydratedDefaultsTimer = null;
        _captureLeases?.CancelActive();
        _wslProxy?.Stop();
        _listener?.Stop();
        _listener?.Close();
        try
        {
            _listenerTask?.Wait(TimeSpan.FromSeconds(2));
        }
        catch
        {
        }
        try
        {
            _wslProxyTask?.Wait(TimeSpan.FromSeconds(2));
        }
        catch
        {
        }
        try
        {
            _captureLeases?.DisposeAsync().AsTask().Wait(TimeSpan.FromSeconds(2));
        }
        catch
        {
        }

        if (!string.IsNullOrEmpty(_discoveryPath))
        {
            try
            {
                IOFile.Delete(_discoveryPath);
                if (_routeDiscoveryPath.Length != 0)
                {
                    IOFile.Delete(_routeDiscoveryPath);
                }
            }
            catch
            {
            }
        }
        _captureDirtyPages.Dispose();
    }

    private void DetachDataModel(DataModel dataModel)
    {
        if (_dataModel is not { } currentDataModel || !currentDataModel.Equals(dataModel))
        {
            return;
        }

        // A cancelled serializer remains the exclusive capture owner until its
        // engine task settles. Do not admit a capture against the next DataModel
        // generation while the old serializer is still returning.
        _captureLeases?.CancelActive();

        dataModel.DescendantAdded -= OnDescendantAdded;
        dataModel.DescendantRemoving -= OnDescendantRemoving;
        ArmLaunchHydratedDefaultsTimer(Timeout.InfiniteTimeSpan);
        Interlocked.Exchange(ref _managedSnapshotPending, 0);
        DestroyLiveSessionMarker();
        _propertyObservation?.Dispose();
        _propertyObservation = null;
        _captureDirtyPages.Reset();

        SerializedPropertyAccess.EngineThreadPump? engineThreadPump;
        Timer? managedSnapshotTimer;
        lock (_engineStateLock)
        {
            _engineGeneration++;
            engineThreadPump = _engineThreadPump;
            _engineThreadPump = null;
            managedSnapshotTimer = _managedSnapshotTimer;
            _managedSnapshotTimer = null;
            _preservedStudioRoute = UniqueStudioRoute(_studioIdentityCandidates.Values);
            _studioIdentityCandidates.Clear();
            _studioIdentity = null;
            _detachedEditDataModelHandle = InstanceHierarchy.RuntimeHandle(dataModel);
            _dataModel = null;
            Interlocked.Exchange(ref _excludedEditCameraHandle, 0);
        }

        PublishStudioIdentity(null);
        managedSnapshotTimer?.Dispose();
        engineThreadPump?.Dispose();
        FailEngineWork(new InvalidOperationException("edit DataModel was detached"));
        ResetChanges();
        _instances.Clear();
        _managedBindingReleases.Clear();
        lock (_manifestIdentityLock)
        {
            _manifestIdentityRemap = null;
        }
        TaskCompletionSource<bool> managedSnapshotReady;
        lock (_managedHierarchyLock)
        {
            managedSnapshotReady = _managedSnapshotReady;
            _managedBySource.Clear();
            _managedByRuntime.Clear();
            _managedByDebug.Clear();
            _managedOwnershipRoots.Clear();
            _managedOwnedSourceIds.Clear();
            _launchHydratedRootDefaults.Clear();
            _pendingLaunchHydratedRootDefaultRefreshes.Clear();
            _attachedManagedRootBaselines.Clear();
            _launchHydratedDefaultFailures = [];
            _stagedManagedSource = null;
            _attachedManagedContract = null;
            _loadedHierarchy = null;
            _preVerificationHierarchyChanges = null;
        }
        lock (_managedIdentityResolutionLock)
        {
            _managedIdentityResolutions.Clear();
        }
        managedSnapshotReady.TrySetCanceled();
    }

    private void TryCaptureManagedRuntimeSnapshot(bool startupBoundaryAttested = false)
    {
        if (Volatile.Read(ref _managedSnapshotPending) == 0)
        {
            return;
        }

        var dataModel = _dataModel;
        if (dataModel is null)
        {
            return;
        }

        lock (_engineStateLock)
        {
            // The DataModel attachment callback precedes both persisted-place
            // materialization and Studio's deterministic service construction.
            // The exact, unique Carbon route marker is the first trustworthy
            // signal that this edit session's plugin has started. Capturing
            // before it would enlarge the unsupported pre-verification window.
            if (_studioIdentity is null)
            {
                ArmManagedSnapshotTimer(ManagedSnapshotRetryPeriod);
                return;
            }
        }

        try
        {
            if (startupBoundaryAttested)
            {
                Interlocked.Exchange(ref _managedStartupBoundaryAttested, 1);
            }
            var quietPeriod = ManagedSnapshotQuietPeriodFor(
                Volatile.Read(ref _managedStartupBoundaryAttested) != 0);
            var quietFor = Stopwatch.GetElapsedTime(
                Volatile.Read(ref _managedSnapshotLastHierarchyChange));
            if (quietFor < quietPeriod)
            {
                ArmManagedSnapshotTimer(quietPeriod - quietFor);
                return;
            }

            var workspace = dataModel.Workspace;
            var terrain = workspace.Terrain;
            if (terrain is null
                || terrain.Parent is not { } terrainParent
                || !terrainParent.Equals(workspace))
            {
                ArmManagedSnapshotTimer(ManagedSnapshotRetryPeriod);
                return;
            }

            CaptureManagedRuntimeSnapshot(dataModel);
            lock (_managedHierarchyLock)
            {
                if (_loadedHierarchy is not null || _attachedManagedContract is not null)
                {
                    Interlocked.Exchange(ref _managedSnapshotPending, 0);
                }
            }
            if (Volatile.Read(ref _managedSnapshotPending) == 0)
            {
                ArmManagedSnapshotTimer(Timeout.InfiniteTimeSpan);
            }
        }
        catch (Exception error)
        {
            // Edit-mode materialization is not gated by DataModel.Loaded.
            // Retry transient hierarchy/reflection gaps on the engine thread.
            Logger.Info($"Managed hierarchy snapshot deferred: {error.Message}");
            ArmManagedSnapshotTimer(ManagedSnapshotRetryPeriod);
        }
    }

    private void CaptureManagedRuntimeSnapshot(DataModel dataModel)
    {
        long changeSequence;
        long hierarchySequence;
        lock (_changesLock)
        {
            lock (_managedHierarchyLock)
            {
                if (_loadedHierarchy is not null
                    || _attachedManagedContract is not null
                    || _preVerificationHierarchyChanges is null)
                {
                    return;
                }

                // Observer attachment precedes Roblox's persisted-property and
                // internal-service materialization. Those callbacks describe
                // the state this authoritative snapshot is about to capture;
                // they are not edits relative to it. Start the verification
                // journal and receipt sequence at this native snapshot boundary.
                changeSequence = Interlocked.Read(ref _changeSequence);
                hierarchySequence = Interlocked.Read(ref _hierarchySequence);
                _changes.Clear();
                while (_changesReady.Wait(0))
                {
                }
                _preVerificationHierarchyChanges = [];
            }
        }

        var runtime = ReadManagedRuntimeHierarchy(dataModel);
        // This callback is running on Studio's engine thread, so hierarchy
        // notifications cannot interleave with this pure snapshot work. Finish
        // the structural index before the source/runtime match; the journal
        // remains open until the verification receipt is committed.
        var shapeTimer = Stopwatch.StartNew();
        var runtimeShapes = ManagedHierarchy.PrecomputeRuntimeShapes(runtime.Nodes);
        Logger.Info(
            $"Managed runtime hierarchy shapes precomputed before verification boundary " +
            $"in {shapeTimer.ElapsedMilliseconds} ms");

        ManagedSourceContract? preverifiedSource;
        lock (_managedHierarchyLock)
        {
            preverifiedSource = _stagedManagedSource;
        }
        IReadOnlyList<ManagedHierarchyMatch>? preverifiedMatches = null;
        if (preverifiedSource is not null)
        {
            try
            {
                var verificationTimer = Stopwatch.StartNew();
                preverifiedMatches = ManagedHierarchy.Match(
                    preverifiedSource.Source,
                    runtime.Nodes,
                    runtime.RootDebugId,
                    strategy => Logger.Info(
                        $"Managed hierarchy preverification strategy: {strategy}"),
                    runtimeShapes);
                Logger.Info(
                    $"Managed hierarchy preverified {preverifiedMatches.Count} nodes " +
                    $"before verification boundary in {verificationTimer.ElapsedMilliseconds} ms");
            }
            catch (Exception ex)
            {
                // Preserve the raw verification route's diagnostic behavior. A
                // mismatch is reported by the request that owns the contract,
                // rather than preventing the baseline snapshot from publishing.
                Logger.Info($"Managed hierarchy preverification deferred: {ex.Message}");
                preverifiedMatches = null;
            }
        }
        Dictionary<nuint, LaunchHydratedServiceDefaults> attachedManagedRootBaselines = [];
        if (preverifiedSource is not null && preverifiedMatches is not null)
        {
            var authoredRootSourceIds = preverifiedSource.Source
                .Where(node => node.ParentIndex == 0)
                .Select(node => node.SourceId)
                .ToHashSet(StringComparer.Ordinal);
            var runtimeHandleByDebugId = runtime.Nodes.ToDictionary(
                node => node.DebugId,
                node => node.Handle,
                StringComparer.Ordinal);
            foreach (var match in preverifiedMatches)
            {
                if (authoredRootSourceIds.Contains(match.SourceId)
                    && runtimeHandleByDebugId.TryGetValue(match.DebugId, out var handle))
                {
                    if (runtime.LaunchHydratedRootDefaults.Remove(handle, out var baseline))
                    {
                        attachedManagedRootBaselines.Add(handle, baseline);
                    }
                }
            }
        }

        lock (_changesLock)
        {
            lock (_managedHierarchyLock)
            {
                if (_dataModel is null || !_dataModel.Equals(dataModel)
                    || _loadedHierarchy is not null
                    || _attachedManagedContract is not null
                    || _preVerificationHierarchyChanges is null)
                {
                    return;
                }
                _launchHydratedRootDefaults.Clear();
                foreach (var (handle, defaults) in runtime.LaunchHydratedRootDefaults)
                {
                    _launchHydratedRootDefaults.Add(handle, defaults);
                }
                _pendingLaunchHydratedRootDefaultRefreshes.Clear();
                _attachedManagedRootBaselines.Clear();
                foreach (var (handle, baseline) in attachedManagedRootBaselines)
                {
                    _attachedManagedRootBaselines.Add(handle, baseline);
                }
                _launchHydratedDefaultFailures = runtime.LaunchHydratedDefaultFailures;
                if (preverifiedMatches is not null
                    && ReferenceEquals(_stagedManagedSource, preverifiedSource))
                {
                    try
                    {
                        ManagedHierarchy.ValidatePreVerificationChanges(
                            _preVerificationHierarchyChanges,
                            runtime.RuntimeOnlyRootDebugIds);
                        if (hierarchySequence != Interlocked.Read(ref _hierarchySequence)
                            || changeSequence != Interlocked.Read(ref _changeSequence))
                        {
                            throw new InvalidOperationException(
                                "persistent edit DataModel mutation crossed the managed verification boundary");
                        }
                    }
                    catch (Exception ex)
                    {
                        // Preserve the exact journal for the staged verification
                        // request. It will fail closed with the offending edit
                        // instead of publishing a receipt that hides the change.
                        _loadedHierarchy = new(
                            runtime.Nodes,
                            runtime.RootDebugId,
                            hierarchySequence,
                            changeSequence,
                            runtime.RuntimeOnlyRootDebugIds,
                            runtime.RootStudioDebugIds,
                            runtimeShapes);
                        _managedSnapshotReady.TrySetResult(true);
                        Logger.Info($"Managed hierarchy atomic commit deferred: {ex.Message}");
                        return;
                    }

                    // Snapshot, structural verification, and publication all run
                    // in one engine-thread transaction. No hierarchy callback can
                    // interleave, so the first observable post-verification edit
                    // receives a sequence strictly after this receipt.
                    _managedBySource.Clear();
                    _managedByRuntime.Clear();
                    _managedByDebug.Clear();
                    foreach (var match in preverifiedMatches)
                    {
                        var binding = new ManagedHierarchyBinding(
                            match.SourceId,
                            match.DebugId,
                            match.RootSourceId,
                            match.RootDebugId);
                        _managedBySource.Add(binding.SourceId, binding);
                        _managedByRuntime.Add(binding.DebugId, binding);
                    }
                    var sourceRootDebugIds = SourceRootDebugIds(
                        preverifiedSource!.Source,
                        preverifiedMatches,
                        runtime.RootStudioDebugIds);
                    _attachedManagedContract = new(
                        preverifiedSource.ContractId,
                        preverifiedMatches.Count,
                        hierarchySequence,
                        changeSequence,
                        sourceRootDebugIds);
                    _preVerificationHierarchyChanges = null;
                    _managedSnapshotReady.TrySetResult(true);
                    Logger.Info(
                        $"Managed hierarchy contract {preverifiedSource.ContractId} " +
                        $"committed atomically before Studio resumed");
                    return;
                }

                // The delayed-staging route cannot preverify until its request
                // supplies a source tree. Preserve every change after the native
                // snapshot boundary through that request.
                _loadedHierarchy = new(
                    runtime.Nodes,
                    runtime.RootDebugId,
                    hierarchySequence,
                    changeSequence,
                    runtime.RuntimeOnlyRootDebugIds,
                    runtime.RootStudioDebugIds,
                    runtimeShapes);
                _managedSnapshotReady.TrySetResult(true);
            }
        }
    }

    private ManagedRuntimeHierarchy ReadManagedRuntimeHierarchy(DataModel dataModel)
    {
        Instance? editCamera = null;
        try
        {
            editCamera = dataModel.Workspace.CurrentCamera;
            RememberEditCamera(editCamera);
        }
        catch
        {
        }

        var snapshotTimer = Stopwatch.StartNew();
        var payload = InstanceHierarchy.Read(dataModel, editCamera);
        var nativeMilliseconds = snapshotTimer.ElapsedMilliseconds;
        var runtimeNodes = ManagedHierarchy.NormalizeRuntime(
            ManagedHierarchy.ParseRuntime(payload),
            node =>
            {
                try
                {
                    var weld = Instance.FromHandle(node.Handle) as Weld;
                    return weld?.Part1 is { } part1
                        ? InstanceHierarchy.RuntimeHandle(part1)
                        : 0;
                }
                catch
                {
                    // Preserve the node when the exact normalization predicate
                    // cannot be established. Verification will then fail closed
                    // instead of silently ignoring an authored HeadWeld.
                    return 0;
                }
            });
        Logger.Info(
            $"Managed hierarchy baseline captured {runtimeNodes.Count} nodes " +
            $"(native {nativeMilliseconds} ms, native+parse {snapshotTimer.ElapsedMilliseconds} ms)");
        var runtimeRootDebugId = runtimeNodes[0].DebugId;
        var rootStudioDebugIds = runtimeNodes
            .Where(node => node.ParentDebugId == runtimeRootDebugId)
            .ToDictionary(
                node => node.DebugId,
                node => (Instance.FromHandle(node.Handle)
                    ?? throw new InvalidDataException("managed runtime root handle is unavailable"))
                    .GetDebugId(128),
                StringComparer.Ordinal);
        var runtimeOnlyRootDebugIds = ManagedHierarchy.RuntimeOnlyRoots(runtimeNodes, runtimeRootDebugId)
            .Select(node => Instance.FromHandle(node.Handle)
                ?? throw new InvalidDataException("managed runtime-only root handle is unavailable"))
            .Select(instance => instance.GetDebugId(128))
            .ToHashSet(StringComparer.Ordinal);
        var (launchHydratedRootDefaults, launchHydratedDefaultFailures) = CaptureLaunchHydratedRootDefaults(
            runtimeNodes
                .Where(node => node.ParentDebugId == runtimeRootDebugId)
                .Select(node => Instance.FromHandle(node.Handle)
                    ?? throw new InvalidDataException("managed runtime root handle is unavailable")));
        return new(
            runtimeNodes,
            runtimeRootDebugId,
            runtimeOnlyRootDebugIds,
            rootStudioDebugIds,
            launchHydratedRootDefaults,
            launchHydratedDefaultFailures);
    }

    private (Dictionary<nuint, LaunchHydratedServiceDefaults> Defaults, string[] Failures)
        CaptureLaunchHydratedRootDefaults(
        IEnumerable<Instance> roots,
        IReadOnlySet<nuint>? excludedHandles = null)
    {
        var defaults = new Dictionary<nuint, LaunchHydratedServiceDefaults>();
        var failures = new List<string>();
        foreach (var instance in roots)
        {
            var handle = InstanceHierarchy.RuntimeHandle(instance);
            if (excludedHandles?.Contains(handle) == true)
            {
                continue;
            }
            try
            {
                defaults.Add(
                    handle,
                    new(
                        instance.ClassName,
                        instance.Name,
                        SerializedPropertyAccess.Snapshot(instance)));
            }
            catch (Exception error)
            {
                // An incomplete launch baseline must retain this service later.
                // It is never safe to infer defaults from a different class or
                // Studio build when a singleton cannot be instantiated directly.
                Logger.Info(
                    $"Managed launch defaults unavailable for {instance.ClassName} {instance.Name}: " +
                    error.Message);
                failures.Add($"{instance.ClassName} {instance.Name}: {error.Message}");
            }
        }
        return (defaults, failures.ToArray());
    }

    private ManifestIdentityBootstrapResponse BootstrapManifestIdentities(
        ManifestIdentityBootstrapRequest request)
    {
        lock (_manifestIdentityLock)
        {
            if (_manifestIdentities.IsAuthoritative)
            {
                _manifestIdentities.Bootstrap(
                    [],
                    request.RootSourceId,
                    request.ExpectedSourceInstances,
                    request.ExpectedDigest);
                return new(true, _manifestIdentities.Count, _manifestIdentities.ActiveDigest());
            }
        }
        var dataModel = _dataModel
            ?? throw new InvalidOperationException("edit DataModel is unavailable");
        Instance? editCamera = null;
        try
        {
            editCamera = dataModel.Workspace.CurrentCamera;
        }
        catch
        {
        }
        var runtime = ManagedHierarchy.ParseCaptureRuntimePayload(
            InstanceHierarchy.Read(dataModel, editCamera, includeCaptureMetadata: true));
        var bindings = new List<ManifestIdentityBinding>(request.ExpectedSourceInstances)
        {
            new(runtime.Nodes[0].Handle, request.RootSourceId),
        };
        foreach (var node in runtime.Nodes.Skip(1))
        {
            var instance = Instance.FromHandle(node.Handle)
                ?? throw new InvalidDataException("manifest identity bootstrap handle is unavailable");
            var sourceId = ManifestIdentityAttributeCodec.Decode(
                SerializedPropertyAccess.Read(
                    instance,
                    ManifestIdentityAttributeCodec.SerializedPropertyName),
                node.ClassName,
                node.Name);
            if (sourceId is null)
            {
                continue;
            }
            bindings.Add(new(node.Handle, sourceId));
        }
        var resolvedBindings = ManifestIdentityBootstrapResolver.Resolve(
            runtime,
            bindings,
            request.Rebindings);

        // Validate without changing the active ledger or consuming the markers.
        new ManifestIdentityLedger().Bootstrap(
            resolvedBindings,
            request.RootSourceId,
            request.ExpectedSourceInstances,
            request.ExpectedDigest);
        lock (_manifestIdentityLock)
        {
            _manifestIdentities.Bootstrap(
                resolvedBindings,
                request.RootSourceId,
                request.ExpectedSourceInstances,
                request.ExpectedDigest);
        }
        return new(true, resolvedBindings.Count, request.ExpectedDigest);
    }

    private ManifestIdentityRemapResponse ApplyManifestIdentityRemapChunk(byte[] payload)
    {
        if (payload.Length < 64 || (payload.Length - 32) % 32 != 0)
        {
            throw new InvalidDataException("manifest identity remap chunk is malformed");
        }
        var captureId = ManifestIdentity.FromBytes(payload.AsSpan(0, 16));
        var totalRaw = BinaryPrimitives.ReadUInt64LittleEndian(payload.AsSpan(16, 8));
        var offsetRaw = BinaryPrimitives.ReadUInt64LittleEndian(payload.AsSpan(24, 8));
        var count = (payload.Length - 32) / 32;
        if (totalRaw is 0 or > 20_000_000
            || offsetRaw > totalRaw
            || offsetRaw + (ulong)count > totalRaw)
        {
            throw new InvalidDataException("manifest identity remap chunk range is invalid");
        }
        var total = checked((int)totalRaw);
        var offset = checked((int)offsetRaw);
        var captureIdText = captureId.ToString();
        (_captureLeases ?? throw new InvalidOperationException("capture lease manager is unavailable"))
            .EnsureReadyCapture(captureIdText);

        lock (_manifestIdentityLock)
        {
            if (offset == 0)
            {
                _manifestIdentityRemap = new(captureId, total);
            }
            var session = _manifestIdentityRemap;
            if (session is null
                || session.CaptureId != captureId
                || session.Total != total
                || session.Next != offset)
            {
                throw new InvalidOperationException("manifest identity remap chunks are missing or out of order");
            }
            for (var index = 0; index < count; index++)
            {
                var start = 32 + index * 32;
                var observed = ManifestIdentity.FromBytes(payload.AsSpan(start, 16));
                var stable = ManifestIdentity.FromBytes(payload.AsSpan(start + 16, 16));
                if (!session.Mappings.TryAdd(observed, stable))
                {
                    throw new InvalidDataException("manifest identity remap repeats a captured identity");
                }
            }
            session.Next += count;
            var complete = session.Next == session.Total;
            if (!complete)
            {
                return new(false, false, session.Next, string.Empty, captureIdText);
            }
            _manifestIdentities.ApplyRemap(captureId, session.Mappings);
            _manifestIdentityRemap = null;
            return new(true, true, _manifestIdentities.Count, _manifestIdentities.ActiveDigest(), captureIdText);
        }
    }

    private void OnDescendantAdded(Instance instance)
    {
        CancelManagedBindingReleaseIfRetained(instance);
        TryCacheStudioIdentity(instance);
        TryCaptureManagedRuntimeSnapshotFromReadyMarker(instance);
        if (TryCaptureLaunchHydratedRootDefault(instance))
        {
            ArmLaunchHydratedDefaultsTimer(ManagedSnapshotQuietPeriod);
        }
        RecordHierarchyChange(instance, "Add", "__CarbonHierarchy", "Add");
    }

    private void OnDescendantRemoving(Instance instance)
    {
        lock (_managedHierarchyLock)
        {
            var handle = InstanceHierarchy.RuntimeHandle(instance);
            _launchHydratedRootDefaults.Remove(handle);
            _pendingLaunchHydratedRootDefaultRefreshes.Remove(handle);
            _attachedManagedRootBaselines.Remove(handle);
        }
        ClearStudioIdentity(instance);
        RecordHierarchyChange(instance, "Remove", "__CarbonHierarchy", "Remove");
        ScheduleManagedBindingReleaseIfRetained(instance);
    }

    private bool TryCaptureLaunchHydratedRootDefault(Instance instance)
    {
        var dataModel = _dataModel;
        if (dataModel is null
            || instance.Parent is not { } parent
            || !parent.Equals(dataModel))
        {
            return false;
        }

        var handle = InstanceHierarchy.RuntimeHandle(instance);
        var runtimeId = ManagedHierarchy.RuntimeIdentity(handle);
        lock (_managedHierarchyLock)
        {
            if (_managedByRuntime.ContainsKey(runtimeId)
                || _launchHydratedRootDefaults.ContainsKey(handle)
                || _pendingLaunchHydratedRootDefaultRefreshes.Contains(handle))
            {
                return false;
            }
        }

        var (defaults, failures) = CaptureLaunchHydratedRootDefaults([instance]);
        lock (_managedHierarchyLock)
        {
            if (_managedByRuntime.ContainsKey(runtimeId))
            {
                return false;
            }
            if (defaults.TryGetValue(handle, out var baseline))
            {
                _launchHydratedRootDefaults.TryAdd(handle, baseline);
            }
            _pendingLaunchHydratedRootDefaultRefreshes.Add(handle);
            if (failures.Length != 0)
            {
                _launchHydratedDefaultFailures = [.. _launchHydratedDefaultFailures, .. failures];
            }
        }
        return true;
    }

    private void ArmLaunchHydratedDefaultsTimer(TimeSpan dueTime)
    {
        try
        {
            _launchHydratedDefaultsTimer?.Change(dueTime, Timeout.InfiniteTimeSpan);
        }
        catch (ObjectDisposedException)
        {
        }
    }

    private void OnLaunchHydratedDefaultsTimer(object? state)
    {
        _ = RefreshLaunchHydratedRootDefaultsAsync();
    }

    private async Task RefreshLaunchHydratedRootDefaultsAsync()
    {
        var cancellationToken = _shutdown?.Token ?? CancellationToken.None;
        try
        {
            await OnEngineThread(() =>
            {
                var dataModel = _dataModel;
                if (dataModel is null)
                {
                    return true;
                }

                HashSet<nuint> pendingHandles;
                lock (_managedHierarchyLock)
                {
                    pendingHandles = new(_pendingLaunchHydratedRootDefaultRefreshes);
                    _pendingLaunchHydratedRootDefaultRefreshes.Clear();
                }
                var roots = dataModel.GetChildren()
                    .Where(instance => pendingHandles.Contains(
                        InstanceHierarchy.RuntimeHandle(instance)))
                    .ToArray();
                var (defaults, failures) = CaptureLaunchHydratedRootDefaults(roots);
                lock (_managedHierarchyLock)
                {
                    RefreshPendingLaunchHydratedRootDefaults(
                        _launchHydratedRootDefaults,
                        defaults,
                        pendingHandles);
                    _launchHydratedDefaultFailures = failures;
                }
                return true;
            }, cancellationToken);
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
        }
        catch (Exception error)
        {
            Logger.Info($"Managed launch defaults refresh deferred: {error.Message}");
        }
    }

    private void OnItemChanged(Instance instance, string propertyName)
    {
        if (IsCaptureArchivableMaskChange(instance, propertyName))
        {
            return;
        }
        if (propertyName is "Value" or "Name" or "Archivable")
        {
            TryCacheStudioIdentity(instance);
        }
        if (IsLaunchBaselineEcho(instance, propertyName))
        {
            return;
        }
        if (!IsPersistentAuthoredMutation(instance, propertyName))
        {
            return;
        }
        if (string.Equals(propertyName, "Name", StringComparison.Ordinal))
        {
            var recordNativeChange = false;
            try
            {
                var nameDescriptor = SerializedPropertyAccess.Describe(instance, propertyName);
                recordNativeChange = nameDescriptor is { } nameInfo && CanObserve(instance.ClassName, nameInfo);
            }
            catch (Exception ex)
            {
                ReportWarning($"Failed to describe serialized name change: {ex.Message}");
            }
            RecordHierarchyChange(
                instance,
                "Rename",
                propertyName,
                "Property",
                recordNativeChange,
                isPersistentAuthoredMutation: true);
            return;
        }

        try
        {
            var descriptor = SerializedPropertyAccess.Describe(instance, propertyName);
            if (descriptor is not { } info || !CanObserve(instance.ClassName, info))
            {
                _captureDirtyPages.Poison();
                Interlocked.Increment(ref _changeSequence);
                return;
            }

            RecordChange(instance, propertyName, "Property", isPersistentAuthoredMutation: true);
        }
        catch (Exception ex)
        {
            _captureDirtyPages.Poison();
            ReportWarning($"Failed to observe serialized property change: {ex.Message}");
        }
    }

    private bool IsCaptureArchivableMaskChange(Instance instance, string propertyName)
    {
        return _captureArchivableMasks.TryConsume(
            propertyName,
            InstanceHierarchy.RuntimeHandle(instance));
    }

    private CaptureArchivableMaskEntry[] ApplyCaptureArchivableMask(
        string captureId,
        IEnumerable<Instance> mappedRoots)
    {
        var roots = mappedRoots.Where(root => root.Archivable).ToArray();
        var handles = roots.Select(InstanceHierarchy.RuntimeHandle).ToArray();
        if (handles.Distinct().Count() != handles.Length)
        {
            throw new InvalidOperationException(
                "capture Archivable mask contains duplicate runtime roots");
        }
        _captureArchivableMasks.Register(captureId, handles);
        try
        {
            for (var index = 0; index < roots.Length; index++)
            {
                SetCaptureArchivable(
                    captureId,
                    roots[index],
                    handles[index],
                    value: false);
            }
            return roots
                .Zip(handles, (root, handle) => new CaptureArchivableMaskEntry(root, handle))
                .ToArray();
        }
        catch (Exception applyFailure)
        {
            var failures = new List<Exception> { applyFailure };
            for (var index = 0; index < roots.Length; index++)
            {
                try
                {
                    SetCaptureArchivable(
                        captureId,
                        roots[index],
                        handles[index],
                        value: true);
                }
                catch (Exception ex)
                {
                    failures.Add(ex);
                }
            }
            try
            {
                _captureArchivableMasks.CompleteRestoration(captureId, handles);
            }
            catch (Exception ex)
            {
                failures.Add(ex);
            }
            if (failures.Count > 1)
            {
                throw new AggregateException(
                    $"capture {captureId} failed while rolling back its Archivable mask",
                    failures);
            }
            System.Runtime.ExceptionServices.ExceptionDispatchInfo
                .Capture(applyFailure)
                .Throw();
            throw new UnreachableException();
        }
    }

    private void RestoreCaptureArchivableMask(
        string captureId,
        IReadOnlyList<CaptureArchivableMaskEntry> roots)
    {
        List<Exception>? failures = null;
        foreach (var entry in roots)
        {
            try
            {
                SetCaptureArchivable(
                    captureId,
                    entry.Root,
                    entry.Handle,
                    value: true);
            }
            catch (Exception ex)
            {
                (failures ??= []).Add(ex);
            }
        }
        try
        {
            _captureArchivableMasks.CompleteRestoration(
                captureId,
                roots.Select(entry => entry.Handle).ToArray());
        }
        catch (Exception ex)
        {
            (failures ??= []).Add(ex);
        }
        if (failures is not null)
        {
            throw new AggregateException(
                $"capture {captureId} could not restore {failures.Count} Archivable value(s)",
                failures);
        }
    }

    private void SetCaptureArchivable(
        string captureId,
        Instance root,
        nuint handle,
        bool value)
    {
        if (root.Archivable == value)
        {
            return;
        }

        _captureArchivableMasks.ExpectNotification(captureId, handle);
        try
        {
            root.Archivable = value;
        }
        catch
        {
            // A setter that rejected the write cannot produce the notification
            // this ticket represents. If it did take effect before throwing,
            // retain the ticket so its delayed callback remains quarantined.
            try
            {
                if (root.Archivable != value)
                {
                    _captureArchivableMasks.CancelExpectedNotification(captureId, handle);
                }
            }
            catch
            {
            }
            throw;
        }
    }

    private void TryCaptureManagedRuntimeSnapshotFromReadyMarker(Instance instance)
    {
        if (Volatile.Read(ref _managedSnapshotPending) == 0)
        {
            return;
        }

        try
        {
            var dataModel = _dataModel;
            var parent = instance.Parent;
            if (dataModel is not null
                && IsManagedBaselineReadyMarker(
                    instance.ClassName,
                    instance.Name,
                    instance.Archivable,
                    parent?.ClassName,
                    parent?.Parent is { } grandparent && grandparent.Equals(dataModel)))
            {
                // DescendantAdded runs on Studio's engine thread. Attestation
                // switches the native timer to a short quiet window so the
                // snapshot follows same-startup plugin cleanup without paying
                // the materialization fallback's 500 ms delay.
                TryCaptureManagedRuntimeSnapshot(startupBoundaryAttested: true);
            }
        }
        catch
        {
            // The marker can disappear while Studio unloads. The native timer
            // wake remains armed as the fallback capture path.
        }
    }

    internal static bool IsEditDataModelCandidate(DataModelType dataModelType) =>
        dataModelType == DataModelType.Edit
        || !System.Enum.IsDefined(typeof(DataModelType), dataModelType);

    internal static bool ShouldAttachEditDataModelCandidate(
        DataModelType dataModelType,
        bool hasAuthenticatedEditDataModel) =>
        IsEditDataModelCandidate(dataModelType)
        && !hasAuthenticatedEditDataModel;

    internal static bool IsManagedBaselineReadyMarker(
        string className,
        string name,
        bool archivable,
        string? parentClassName,
        bool parentIsDirectDataModelChild) =>
        string.Equals(className, "BoolValue", StringComparison.Ordinal)
        && string.Equals(name, ManagedBaselineReadyMarker, StringComparison.Ordinal)
        && !archivable
        && string.Equals(parentClassName, "CoreGui", StringComparison.Ordinal)
        && parentIsDirectDataModelChild;

    internal static TimeSpan ManagedSnapshotQuietPeriodFor(bool startupBoundaryAttested) =>
        startupBoundaryAttested
            ? AttestedManagedSnapshotQuietPeriod
            : ManagedSnapshotQuietPeriod;

    private void RecordChange(
        Instance instance,
        string propertyName,
        string kind,
        bool isPersistentAuthoredMutation = false)
    {
        try
        {
            if (!isPersistentAuthoredMutation)
            {
                isPersistentAuthoredMutation = IsPersistentAuthoredMutation(instance, propertyName);
            }
            if (!isPersistentAuthoredMutation)
            {
                return;
            }

            var sequence = Interlocked.Increment(ref _changeSequence);
            _captureDirtyPages.MarkDirty(
                InstanceHierarchy.RuntimeHandle(instance),
                sequence);
            if (!TryRetainManagedObservation(instance, out var debugId, out var sourceId))
            {
                return;
            }

            var root = DataModelRoot(instance);
            var rootDebugId = root?.GetDebugId(128);
            var className = instance.ClassName;
            var name = instance.Name;
            var rootClassName = root?.ClassName ?? "unknown";
            var rootName = root?.Name ?? "unknown";
            var notify = false;
            lock (_changesLock)
            {
                lock (_managedHierarchyLock)
                {
                    _preVerificationHierarchyChanges?.Add(new(
                        kind,
                        debugId,
                        rootDebugId,
                        className,
                        name,
                        rootClassName,
                        rootName,
                        propertyName));
                }
                var change = new PropertyChange(
                    sequence,
                    debugId,
                    propertyName,
                    kind,
                    rootDebugId,
                    sourceId);
                notify = _changes.Count == 0;
                _changes.Add(change);
            }
            if (notify)
            {
                _changesReady.Release();
            }
        }
        catch (Exception ex)
        {
            _captureDirtyPages.Poison();
            ReportWarning($"Failed to observe serialized property change: {ex.Message}");
        }
    }

    private void RecordHierarchyChange(
        Instance instance,
        string hierarchyKind,
        string propertyName,
        string nativeKind,
        bool recordNativeChange = true,
        bool isPersistentAuthoredMutation = false)
    {
        if (!isPersistentAuthoredMutation)
        {
            isPersistentAuthoredMutation = IsPersistentAuthoredMutation(instance, propertyName);
        }
        var unmappedPlan = PlanObservationRetention(
            isPersistentAuthoredMutation,
            isMapped: false,
            isHierarchyMutation: true,
            recordsNativeChange: recordNativeChange);
        if (!unmappedPlan.AttestHierarchy)
        {
            return;
        }

        _captureDirtyPages.InvalidateStructure();
        Interlocked.Increment(ref _hierarchySequence);
        var sequence = recordNativeChange
            ? Interlocked.Increment(ref _changeSequence)
            : 0;
        if (!TryRetainManagedObservation(instance, out var debugId, out var sourceId))
        {
            return;
        }

        string? rootDebugId = null;
        var className = "unknown";
        var name = "unknown";
        var rootClassName = "unknown";
        var rootName = "unknown";
        try
        {
            className = instance.ClassName;
            name = instance.Name;
            if (DataModelRoot(instance) is { } root)
            {
                rootDebugId = root.GetDebugId(128);
                rootClassName = root.ClassName;
                rootName = root.Name;
            }
        }
        catch (Exception ex)
        {
            ReportWarning($"Failed to classify hierarchy change: {ex.Message}");
        }

        var notify = false;
        lock (_changesLock)
        {
            lock (_managedHierarchyLock)
            {
                if (recordNativeChange)
                {
                    var change = new PropertyChange(
                        sequence,
                        debugId,
                        propertyName,
                        nativeKind,
                        rootDebugId,
                        sourceId);
                    notify = _changes.Count == 0;
                    _changes.Add(change);
                }
                _preVerificationHierarchyChanges?.Add(new(
                    hierarchyKind,
                    debugId,
                    rootDebugId,
                    className,
                    name,
                    rootClassName,
                    rootName,
                    propertyName));
            }
        }
        if (notify)
        {
            _changesReady.Release();
        }
    }

    private string? AssociateManagedBinding(Instance instance, string debugId)
    {
        var runtimeId = ManagedHierarchy.RuntimeIdentity(InstanceHierarchy.RuntimeHandle(instance));
        lock (_managedHierarchyLock)
        {
            if (_managedByRuntime.TryGetValue(runtimeId, out var known))
            {
                CacheManagedDebugBinding(debugId, known);
                _instances[debugId] = instance;
                _instances[runtimeId] = instance;
                return known.SourceId;
            }
        }
        ManagedHierarchyBinding binding;
        try
        {
            if (!TryResolveManagedBinding(instance, out binding)
                && !TryResolveDisplacedManagedBinding(instance, out binding))
            {
                return null;
            }
        }
        catch (ManagedSourceReplacementPendingException replacement)
        {
            // Filesystem-authoritative replacement can add the new runtime
            // instance before DescendantRemoving retires the old binding. The
            // source identity is still known, so annotate the causal event but
            // do not cache the replacement until the old runtime is gone.
            _instances[debugId] = instance;
            _instances[runtimeId] = instance;
            return replacement.SourceId;
        }
        lock (_managedHierarchyLock)
        {
            CacheManagedDebugBinding(debugId, binding);
        }
        _instances[debugId] = instance;
        _instances[runtimeId] = instance;
        return binding.SourceId;
    }

    private bool TryRetainManagedObservation(
        Instance instance,
        out string debugId,
        out string sourceId)
    {
        debugId = string.Empty;
        sourceId = string.Empty;
        if (!MayBelongToManagedContract(instance))
        {
            return false;
        }

        debugId = instance.GetDebugId(128);
        sourceId = AssociateManagedBinding(instance, debugId) ?? string.Empty;
        return sourceId.Length != 0;
    }

    private bool MayBelongToManagedContract(Instance instance)
    {
        var runtimeId = ManagedHierarchy.RuntimeIdentity(InstanceHierarchy.RuntimeHandle(instance));
        ManagedSourceContract staged;
        lock (_managedHierarchyLock)
        {
            if (_managedByRuntime.TryGetValue(runtimeId, out var known)
                && ShouldResolveManagedObservation(known.SourceId, _managedOwnedSourceIds))
            {
                return true;
            }
            if (_attachedManagedContract is not { } attached
                || _stagedManagedSource is not { } stagedContract
                || !string.Equals(attached.ContractId, stagedContract.ContractId, StringComparison.Ordinal))
            {
                return false;
            }
            staged = stagedContract;
        }

        var dataModel = _dataModel;
        var current = instance.Parent;
        while (current is not null && (dataModel is null || !current.Equals(dataModel)))
        {
            var ancestorRuntimeId = ManagedHierarchy.RuntimeIdentity(
                InstanceHierarchy.RuntimeHandle(current));
            lock (_managedHierarchyLock)
            {
                if (_managedByRuntime.TryGetValue(ancestorRuntimeId, out var ancestor)
                    && ShouldResolveManagedObservation(
                        ancestor.SourceId,
                        _managedOwnedSourceIds))
                {
                    return true;
                }
            }
            current = current.Parent;
        }

        // A newly-created replacement root has no managed ancestor yet. Only a
        // globally unique source class/name can identify it without retaining
        // every unrelated runtime instance as a candidate.
        var candidateIndex = ManagedHierarchy.UniqueClassNameIndex(
            staged.Source,
            instance.ClassName,
            instance.Name);
        if (candidateIndex <= 0)
        {
            return false;
        }
        lock (_managedHierarchyLock)
        {
            return ShouldResolveManagedObservation(
                staged.Source[candidateIndex].SourceId,
                _managedOwnedSourceIds);
        }
    }

    private void SetManagedObservationOwnership(
        IEnumerable<string> sourceIds,
        bool replace)
    {
        lock (_managedHierarchyLock)
        {
            if (UpdateManagedObservationRoots(_managedOwnershipRoots, sourceIds, replace)
                && _stagedManagedSource is { } staged)
            {
                RebuildManagedObservationOwnership(staged);
            }
        }
    }

    internal static bool UpdateManagedObservationRoots(
        HashSet<string> current,
        IEnumerable<string> sourceIds,
        bool replace)
    {
        var requested = sourceIds.ToHashSet(StringComparer.Ordinal);
        if (replace)
        {
            if (current.SetEquals(requested))
            {
                return false;
            }
            current.Clear();
            current.UnionWith(requested);
            return true;
        }

        var changed = false;
        foreach (var sourceId in requested)
        {
            changed |= current.Add(sourceId);
        }
        return changed;
    }

    private void RebuildManagedObservationOwnership(ManagedSourceContract staged)
    {
        _managedOwnedSourceIds.Clear();
        _managedOwnedSourceIds.UnionWith(ManagedHierarchy.ExpandOwnedSourceIds(
            staged.Source,
            _managedOwnershipRoots));
    }

    private void ReleaseManagedBinding(Instance instance)
    {
        var handle = InstanceHierarchy.RuntimeHandle(instance);
        var runtimeId = ManagedHierarchy.RuntimeIdentity(handle);
        lock (_managedHierarchyLock)
        {
            if (_managedByRuntime.Remove(runtimeId, out var binding))
            {
                if (_managedBySource.TryGetValue(binding.SourceId, out var current)
                    && ReferenceEquals(current, binding))
                {
                    _managedBySource.Remove(binding.SourceId);
                }
                foreach (var debugId in _managedByDebug
                    .Where(pair => ReferenceEquals(pair.Value, binding))
                    .Select(pair => pair.Key)
                    .ToArray())
                {
                    _managedByDebug.Remove(debugId);
                    _instances.TryRemove(debugId, out _);
                }
            }
        }
        lock (_manifestIdentityLock)
        {
            _manifestIdentities.Release(handle);
        }
        _instances.TryRemove(runtimeId, out _);
    }

    private void CancelManagedBindingReleaseIfRetained(Instance instance)
    {
        try
        {
            var runtimeId = ManagedHierarchy.RuntimeIdentity(
                InstanceHierarchy.RuntimeHandle(instance));
            var debugId = instance.GetDebugId(128);
            if (_managedBindingReleases.Cancel(runtimeId, debugId))
            {
                return;
            }
            if (_managedBindingReleases.HasPending(runtimeId))
            {
                // The numeric native handle was recycled before the move window
                // elapsed. Retire the previous lifetime before observing the new one.
                _managedBindingReleases.Cancel(runtimeId);
                ReleaseManagedBinding(instance);
            }
        }
        catch
        {
        }
    }

    private void CancelManagedBindingRelease(Instance instance)
    {
        try
        {
            var runtimeId = ManagedHierarchy.RuntimeIdentity(InstanceHierarchy.RuntimeHandle(instance));
            _managedBindingReleases.Cancel(runtimeId);
        }
        catch
        {
        }
    }

    private void ScheduleManagedBindingRelease(Instance instance)
    {
        string runtimeId;
        string debugId;
        try
        {
            runtimeId = ManagedHierarchy.RuntimeIdentity(InstanceHierarchy.RuntimeHandle(instance));
            debugId = instance.GetDebugId(128);
        }
        catch
        {
            return;
        }
        var token = _managedBindingReleases.Schedule(runtimeId, debugId);
        var cancellationToken = _shutdown?.Token ?? CancellationToken.None;
        _ = ReleaseManagedBindingAfterMoveWindow(
            instance,
            runtimeId,
            debugId,
            token,
            cancellationToken);
    }

    private void ScheduleManagedBindingReleaseIfRetained(Instance instance)
    {
        try
        {
            var runtimeId = ManagedHierarchy.RuntimeIdentity(InstanceHierarchy.RuntimeHandle(instance));
            var retained = false;
            lock (_managedHierarchyLock)
            {
                retained = _managedByRuntime.ContainsKey(runtimeId)
                    || _instances.ContainsKey(runtimeId);
            }
            lock (_manifestIdentityLock)
            {
                retained = retained || _manifestIdentities.Contains(InstanceHierarchy.RuntimeHandle(instance));
            }
            if (!retained)
            {
                return;
            }
        }
        catch
        {
            return;
        }
        ScheduleManagedBindingRelease(instance);
    }

    private async Task ReleaseManagedBindingAfterMoveWindow(
        Instance instance,
        string runtimeId,
        string debugId,
        long token,
        CancellationToken cancellationToken)
    {
        try
        {
            await Task.Delay(ManagedMovePairWindow, cancellationToken);
            await OnEngineThread(() =>
            {
                if (!_managedBindingReleases.Complete(runtimeId, token))
                {
                    return false;
                }
                ReleaseManagedBinding(instance);
                _instances.TryRemove(debugId, out _);
                return true;
            }, cancellationToken);
        }
        catch (OperationCanceledException)
        {
        }
        catch (Exception ex)
        {
            if (!cancellationToken.IsCancellationRequested)
            {
                ReportWarning($"Failed to release managed binding after hierarchy settle: {ex.Message}");
            }
        }
    }

    private void CacheManagedDebugBinding(string debugId, ManagedHierarchyBinding binding)
    {
        if (_managedByDebug.TryGetValue(debugId, out var existing)
            && !string.Equals(existing.SourceId, binding.SourceId, StringComparison.Ordinal))
        {
            throw new InvalidOperationException(
                $"managed runtime identity {debugId} is duplicated");
        }
        _managedByDebug[debugId] = binding;
    }

    private Instance? DataModelRoot(Instance instance)
    {
        var dataModel = _dataModel;
        if (dataModel is null || instance.Equals(dataModel))
        {
            return null;
        }
        var current = instance;
        while (true)
        {
            var parent = current.Parent;
            if (parent is null)
            {
                return null;
            }
            if (parent.Equals(dataModel))
            {
                return current;
            }
            current = parent;
        }
    }

    private void RememberEditCamera(DataModel dataModel)
    {
        try
        {
            RememberEditCamera(dataModel.Workspace.CurrentCamera);
        }
        catch
        {
        }
    }

    private void RememberEditCamera(Instance? editCamera)
    {
        if (editCamera is null)
        {
            return;
        }
        Interlocked.Exchange(
            ref _excludedEditCameraHandle,
            checked((nint)InstanceHierarchy.RuntimeHandle(editCamera)));
    }

    private bool IsPersistentAuthoredMutation(Instance instance, string propertyName)
    {
        try
        {
            var handle = InstanceHierarchy.RuntimeHandle(instance);
            var excludedEditCameraHandle = unchecked((nuint)Volatile.Read(
                ref _excludedEditCameraHandle));
            if (excludedEditCameraHandle != 0 && handle == excludedEditCameraHandle)
            {
                return false;
            }

            if (IsEngineOwnedScriptNormalizationProperty(instance.ClassName, propertyName))
            {
                return false;
            }

            if (string.Equals(instance.ClassName, "Workspace", StringComparison.Ordinal)
                && string.Equals(propertyName, "CurrentCamera", StringComparison.Ordinal))
            {
                var currentCamera = _dataModel?.Workspace.CurrentCamera;
                var targetHandle = currentCamera is null
                    ? 0
                    : InstanceHierarchy.RuntimeHandle(currentCamera);
                if (IsExcludedEditCameraReference(
                    instance.ClassName,
                    propertyName,
                    targetHandle,
                    excludedEditCameraHandle))
                {
                    return false;
                }
            }

            var dataModel = _dataModel;
            if (dataModel is null)
            {
                return false;
            }
            var root = DataModelRoot(instance);
            if (root is null)
            {
                return false;
            }
            if (ManagedHierarchy.IsKnownRuntimeOnlyRoot(root.ClassName, root.Name))
            {
                return false;
            }

            var ignoreSelfArchivable = string.Equals(
                propertyName,
                "Archivable",
                StringComparison.Ordinal);
            var current = instance;
            var isSelf = true;
            while (!current.Equals(dataModel))
            {
                if (!Reflection.IsSerializable(current)
                    || (!ignoreSelfArchivable || !isSelf) && !current.Archivable)
                {
                    return false;
                }
                current = current.Parent
                    ?? throw new InvalidOperationException(
                        "observed instance detached before persistence classification");
                isSelf = false;
            }
            return true;
        }
        catch
        {
            // Observation is an attestation boundary. If Roblox exposes a
            // transient state that cannot be classified exactly, fail closed by
            // advancing the compact epoch without retaining an instance graph.
            return true;
        }
    }

    private bool IsLaunchBaselineEcho(Instance instance, string propertyName)
    {
        IReadOnlyDictionary<string, SerializedPropertySnapshot>? baseline = null;
        lock (_managedHierarchyLock)
        {
            if (_launchHydratedRootDefaults.TryGetValue(
                InstanceHierarchy.RuntimeHandle(instance),
                out var defaults))
            {
                baseline = defaults.Properties;
            }
            else if (_attachedManagedRootBaselines.TryGetValue(
                InstanceHierarchy.RuntimeHandle(instance),
                out defaults))
            {
                baseline = defaults.Properties;
            }
        }
        if (baseline is null)
        {
            return false;
        }
        try
        {
            var serializedName = LaunchBaselinePropertyName(propertyName);
            var descriptor = SerializedPropertyAccess.Describe(instance, serializedName);
            var value = descriptor is null
                ? []
                : SerializedPropertyAccess.Read(instance, serializedName);
            var matches = MatchesLaunchBaselineProperty(propertyName, descriptor, value, baseline);
            if (!matches
                && string.Equals(instance.ClassName, "ServerStorage", StringComparison.Ordinal)
                && string.Equals(instance.Name, "ServerStorage", StringComparison.Ordinal)
                && string.Equals(propertyName, "Attributes", StringComparison.Ordinal)
                && baseline.TryGetValue(serializedName, out var transportBaseline))
            {
                matches = ManifestIdentityAttributeCodec.MatchesIgnoringTransportMcpPlaceId(
                    transportBaseline.Value,
                    value);
            }
            if (!matches
                && IsEngineOwnedWorkspaceNormalizationProperty(
                    instance.ClassName,
                    instance.Name,
                    propertyName,
                    value))
            {
                matches = true;
            }
            if (!matches
                && string.Equals(instance.ClassName, "ReplicatedStorage", StringComparison.Ordinal)
                && string.Equals(instance.Name, "ReplicatedStorage", StringComparison.Ordinal)
                && string.Equals(propertyName, "Attributes", StringComparison.Ordinal)
                && baseline.TryGetValue(serializedName, out var emitterBaseline))
            {
                matches = ManifestIdentityAttributeCodec.MatchesIgnoringEmitterVersion(
                    emitterBaseline.Value,
                    value);
            }
            return matches;
        }
        catch
        {
            // A baseline read failure must advance the epoch rather than hide a
            // potentially authored change.
            return false;
        }
    }

    internal static string LaunchBaselinePropertyName(string propertyName) =>
        string.Equals(propertyName, "Attributes", StringComparison.Ordinal)
            ? "AttributesSerialize"
            : propertyName;

    internal static bool MatchesLaunchBaselineProperty(
        string observedPropertyName,
        SerializedPropertyDescriptor? liveDescriptor,
        ReadOnlySpan<byte> liveValue,
        IReadOnlyDictionary<string, SerializedPropertySnapshot> baseline)
    {
        var serializedName = LaunchBaselinePropertyName(observedPropertyName);
        return liveDescriptor is { } descriptor
            && baseline.TryGetValue(serializedName, out var expected)
            && descriptor == expected.Descriptor
            && liveValue.SequenceEqual(expected.Value);
    }

    private string Index(Instance instance)
    {
        try
        {
            var debugId = instance.GetDebugId(128);
            _instances[debugId] = instance;
            return debugId;
        }
        catch
        {
            return string.Empty;
        }
    }

    private void DrainEngineWork()
    {
        if (Interlocked.Exchange(ref _engineDrainActive, 1) != 0)
        {
            return;
        }

        try
        {
            var generation = Volatile.Read(ref _engineGeneration);
            DrainEngineWorkBatch(_engineWork, generation);
        }
        finally
        {
            Volatile.Write(ref _engineDrainActive, 0);
        }

        if (_engineWork.IsEmpty)
        {
            return;
        }

        SerializedPropertyAccess.EngineThreadPump? pump;
        lock (_engineStateLock)
        {
            pump = _engineThreadPump;
        }
        try
        {
            (pump ?? throw new InvalidOperationException("engine-thread pump is unavailable")).Wake();
        }
        catch (Exception ex)
        {
            FailEngineWork(ex);
        }
    }

    private void WakeEngineThreadPump()
    {
        SerializedPropertyAccess.EngineThreadPump? pump;
        lock (_engineStateLock)
        {
            pump = _engineThreadPump;
        }
        try
        {
            pump?.Wake();
        }
        catch
        {
        }
    }

    private void ScheduleManagedSnapshotAfterHierarchyChange()
    {
        if (Volatile.Read(ref _managedSnapshotPending) == 0)
        {
            return;
        }
        Interlocked.Exchange(ref _managedSnapshotLastHierarchyChange, Stopwatch.GetTimestamp());
        ArmManagedSnapshotTimer(ManagedSnapshotQuietPeriodFor(
            Volatile.Read(ref _managedStartupBoundaryAttested) != 0));
    }

    private void ArmManagedSnapshotTimer(TimeSpan dueTime)
    {
        Timer? timer;
        lock (_engineStateLock)
        {
            timer = _managedSnapshotTimer;
        }
        try
        {
            timer?.Change(dueTime, Timeout.InfiniteTimeSpan);
        }
        catch (ObjectDisposedException)
        {
        }
    }

    private void OnManagedSnapshotTimer(object? _)
    {
        var cancellationToken = _shutdown?.Token ?? CancellationToken.None;
        if (cancellationToken.IsCancellationRequested
            || Volatile.Read(ref _managedSnapshotPending) == 0)
        {
            return;
        }
        _ = RetryManagedSnapshotOnEngineThreadAsync(cancellationToken);
    }

    private async Task RetryManagedSnapshotOnEngineThreadAsync(CancellationToken cancellationToken)
    {
        try
        {
            await OnEngineThread(() =>
            {
                TryCaptureManagedRuntimeSnapshot();
                return true;
            }, cancellationToken);
        }
        catch (OperationCanceledException)
        {
        }
        catch (Exception error)
        {
            Logger.Info($"Managed hierarchy snapshot retry deferred: {error.Message}");
            ArmManagedSnapshotTimer(ManagedSnapshotRetryPeriod);
        }
    }

    private async Task<T> OnEngineThread<T>(Func<T> callback, CancellationToken cancellationToken)
    {
        var completion = new TaskCompletionSource<object?>(TaskCreationOptions.RunContinuationsAsynchronously);
        SerializedPropertyAccess.EngineThreadPump pump;
        lock (_engineStateLock)
        {
            pump = _engineThreadPump
                ?? throw new InvalidOperationException("engine-thread pump is unavailable");
            _engineWork.Enqueue(new EngineWork(
                _engineGeneration,
                () => callback(),
                completion));
        }
        try
        {
            pump.Wake();
        }
        catch (Exception ex)
        {
            // A failed wake must poison its queued work. DrainEngineWork skips
            // completed items, so a request that already failed at the HTTP
            // boundary can never execute later.
            completion.TrySetException(ex);
        }
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeout.CancelAfter(TimeSpan.FromSeconds(10));
        using var registration = timeout.Token.Register(() => completion.TrySetCanceled(timeout.Token));
        return (T)(await completion.Task)!;
    }

    private async Task<T> OnEngineThreadUninterruptibleOnceStarted<T>(
        Func<T> callback,
        CancellationToken cancellationToken,
        bool timeoutBeforeStart = true)
    {
        var completion = new TaskCompletionSource<object?>(TaskCreationOptions.RunContinuationsAsynchronously);
        var launchGate = new CaptureLeaseLaunchGate();
        SerializedPropertyAccess.EngineThreadPump pump;
        lock (_engineStateLock)
        {
            pump = _engineThreadPump
                ?? throw new InvalidOperationException("engine-thread pump is unavailable");
            _engineWork.Enqueue(new EngineWork(
                _engineGeneration,
                () =>
                {
                    launchGate.Start(cancellationToken);
                    return callback();
                },
                completion));
        }
        try
        {
            pump.Wake();
        }
        catch (Exception ex)
        {
            launchGate.CancelBeforeStart(() => completion.TrySetException(ex));
        }
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        if (timeoutBeforeStart)
        {
            timeout.CancelAfter(TimeSpan.FromSeconds(10));
        }
        using var registration = timeout.Token.Register(() =>
            launchGate.CancelBeforeStart(() => completion.TrySetCanceled(timeout.Token)));
        return (T)(await completion.Task)!;
    }

    private void FailEngineWork(Exception error)
    {
        while (_engineWork.TryDequeue(out var work))
        {
            work.Fail(error);
        }
    }

    private void ResetChanges()
    {
        lock (_changesLock)
        {
            _changes.Clear();
            while (_changesReady.Wait(0))
            {
            }
        }
    }

    private void ReportWarning(string message)
    {
        _captureDirtyPages.Poison();
        Logger.Warn(message);
        var notify = false;
        lock (_changesLock)
        {
            notify = _changes.Count == 0 && _diagnostics.Count == 0;
            _diagnostics.Add(new(
                Interlocked.Increment(ref _changeSequence),
                "Warning",
                message));
        }
        if (notify)
        {
            _changesReady.Release();
        }
    }

    internal static int DrainEngineWorkBatch(ConcurrentQueue<EngineWork> queue, long generation)
    {
        var count = 0;
        while (count < EngineWorkBatchSize && queue.TryDequeue(out var work))
        {
            work.Run(generation);
            count++;
        }
        return count;
    }

    internal static bool CanReadForCapture(SerializedPropertyDescriptor descriptor)
    {
        return !descriptor.IsExcluded
            && (descriptor.IsAccessible || IsSerializedThroughModel(descriptor.TypeName));
    }

    internal static bool UsesSerializedPropertyCarrier(SerializedPropertyDescriptor descriptor) =>
        CanReadForCapture(descriptor) && IsCapturedThroughModel(descriptor.TypeName);

    internal static string SerializedPropertyCarrierClass(
        string sourceClassName,
        SerializedPropertyDescriptor descriptor) =>
        descriptor.TypeName == "PhysicalProperties" ? "Part" : sourceClassName;

    private static bool IsSerializedThroughModel(string typeName) =>
        typeName is "NetAssetRef" or "PhysicalProperties" or "Region3" or "ColorSequence" or "NumberSequence";

    private static bool IsCapturedThroughModel(string typeName) =>
        typeName is "NetAssetRef" or "PhysicalProperties" or "Region3" or "ColorSequence" or "NumberSequence";

    private static bool IsPersistentReadOnlyProperty(string className, string propertyName) =>
        (className, propertyName) is
            ("Chat", "LoadDefaultChat") or
            ("HttpService", "HttpEnabled") or
            ("Lighting", "LightingStyle") or
            ("Lighting", "PrioritizeLightingQuality") or
            ("MeshPart", "HasJointOffset") or
            ("MeshPart", "HasSkinnedMesh") or
            ("MeshPart", "JointOffset") or
            ("MeshPart", "MeshContent") or
            ("PackageLink", "DefaultName") or
            ("PackageLink", "PackageContent") or
            ("Players", "MaxPlayers") or
            ("Players", "PreferredPlayers") or
            ("StarterPlayer", "AllowCustomAnimations") or
            ("TextChatService", "ChatVersion");

    internal static bool CanWriteMaterialized(
        string className,
        SerializedPropertyDescriptor descriptor) =>
        !descriptor.IsExcluded
        && ((descriptor.IsAccessible && IsSerializedThroughModel(descriptor.TypeName))
            || IsPersistentReadOnlyProperty(className, descriptor.Name));

    internal static bool CanCopyFromModel(SerializedPropertyDescriptor descriptor) => !descriptor.IsExcluded;

    internal static bool CanObserve(
        string className,
        SerializedPropertyDescriptor descriptor)
    {
        return CanReadForCapture(descriptor)
            || (!descriptor.IsExcluded
                && (descriptor.IsReference || IsPersistentReadOnlyProperty(className, descriptor.Name)));
    }

    internal static bool CanTransportReference(SerializedPropertyDescriptor descriptor) =>
        descriptor.IsReference && !descriptor.IsExcluded;

    internal static T? ResolveOptionalReferenceTarget<T>(string? targetDebugId, Func<string, T> resolve)
        where T : class =>
        targetDebugId is null ? null : resolve(targetDebugId);

    private async Task ListenAsync(CancellationToken cancellationToken)
    {
        while (!cancellationToken.IsCancellationRequested && _listener is { IsListening: true } listener)
        {
            HttpListenerContext context;
            try
            {
                context = await listener.GetContextAsync().WaitAsync(cancellationToken);
            }
            catch (OperationCanceledException)
            {
                break;
            }
            catch (HttpListenerException) when (cancellationToken.IsCancellationRequested)
            {
                break;
            }

            _ = Task.Run(() => HandleAsync(context, cancellationToken), cancellationToken);
        }
    }

    private string? StartWslProxy(int loopbackPort, CancellationToken cancellationToken)
    {
        var address = FindWslAddress();
        if (address is null)
        {
            return null;
        }

        try
        {
            _wslProxy = new TcpListener(address, 0);
            _wslProxy.Start();
            var proxyPort = ((IPEndPoint)_wslProxy.LocalEndpoint).Port;
            _wslProxyTask = Task.Run(() => ProxyWslAsync(loopbackPort, cancellationToken));
            return $"http://{address}:{proxyPort}/";
        }
        catch (Exception ex)
        {
            _wslProxy?.Stop();
            _wslProxy = null;
            ReportWarning($"Carbon bridge could not start its WSL proxy: {ex.Message}");
            return null;
        }
    }

    private async Task ProxyWslAsync(int loopbackPort, CancellationToken cancellationToken)
    {
        while (!cancellationToken.IsCancellationRequested && _wslProxy is { } listener)
        {
            TcpClient incoming;
            try
            {
                incoming = await listener.AcceptTcpClientAsync(cancellationToken);
            }
            catch (OperationCanceledException)
            {
                break;
            }
            catch (SocketException) when (cancellationToken.IsCancellationRequested)
            {
                break;
            }

            _ = Task.Run(() => ProxyConnectionAsync(incoming, loopbackPort, cancellationToken), cancellationToken);
        }
    }

    private static async Task ProxyConnectionAsync(
        TcpClient incoming,
        int loopbackPort,
        CancellationToken cancellationToken)
    {
        using (incoming)
        using (var outgoing = new TcpClient(AddressFamily.InterNetwork))
        using (var connectionShutdown = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken))
        {
            try
            {
                incoming.NoDelay = true;
                outgoing.NoDelay = true;
                await outgoing.ConnectAsync(IPAddress.Loopback, loopbackPort, connectionShutdown.Token);
                var inbound = incoming.GetStream();
                var outbound = outgoing.GetStream();
                var upload = inbound.CopyToAsync(outbound, connectionShutdown.Token);
                var download = outbound.CopyToAsync(inbound, connectionShutdown.Token);
                await Task.WhenAny(upload, download);
                connectionShutdown.Cancel();
                try
                {
                    await Task.WhenAll(upload, download);
                }
                catch (OperationCanceledException)
                {
                }
            }
            catch (OperationCanceledException)
            {
            }
            catch (SocketException)
            {
            }
            catch (IOException)
            {
            }
        }
    }

    private async Task HandleAsync(HttpListenerContext context, CancellationToken cancellationToken)
    {
        try
        {
            if (!Authorized(context.Request))
            {
                await ReplyAsync(context.Response, HttpStatusCode.Unauthorized, new { error = "unauthorized" });
                return;
            }

            var path = context.Request.Url?.AbsolutePath.TrimEnd('/') ?? string.Empty;
            if (path.StartsWith("/v1/diagnostics/", StringComparison.Ordinal)
                && !IsDiagnosticRouteSupported(path))
            {
                await ReplyAsync(context.Response, HttpStatusCode.NotFound, new { error = "not_found" });
                return;
            }
            const string managedStagePrefix = "/v1/managed/stage/";
            if (path.StartsWith(managedStagePrefix, StringComparison.Ordinal))
            {
                var contractId = path[managedStagePrefix.Length..];
                if (contractId.Length != 32 || contractId.Any(character => !Uri.IsHexDigit(character)))
                {
                    throw new InvalidOperationException("managed hierarchy contract identity is invalid");
                }
                var payload = await ReadBytesAsync(
                    context.Request,
                    512 * 1024 * 1024,
                    cancellationToken);
                var sourceParseTimer = Stopwatch.StartNew();
                var source = ManagedHierarchy.Parse(payload);
                var staged = ManagedSourceContract.Create(contractId, source);
                lock (_managedHierarchyLock)
                {
                    _stagedManagedSource = staged;
                    RebuildManagedObservationOwnership(staged);
                }
                Interlocked.Exchange(ref _managedSnapshotPending, 1);
                ArmManagedSnapshotTimer(ManagedSnapshotQuietPeriodFor(
                    Volatile.Read(ref _managedStartupBoundaryAttested) != 0));
                Logger.Info(
                    $"Managed hierarchy source contract staged {source.Count} nodes " +
                    $"in {sourceParseTimer.ElapsedMilliseconds} ms");
                await ReplyAsync(context.Response, HttpStatusCode.OK, new
                {
                    contractId,
                    sourceInstances = source.Count,
                });
                return;
            }
            if (path.Equals("/v2/capture-leases", StringComparison.Ordinal)
                || path.StartsWith("/v2/capture-leases/", StringComparison.Ordinal))
            {
                await HandleCaptureLeaseAsync(context, path, cancellationToken);
                return;
            }
            switch (path)
            {
                case "/v1/identity":
                    {
                        var result = GetStudioIdentity();
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/capabilities":
                    bool engineReady;
                    long engineGeneration;
                    StudioIdentity? studioIdentity;
                    lock (_engineStateLock)
                    {
                        engineReady = _dataModel is not null && _engineThreadPump is not null;
                        engineGeneration = _engineGeneration;
                        studioIdentity = _studioIdentity;
                    }
                    bool manifestIdentitiesAuthoritative;
                    lock (_manifestIdentityLock)
                    {
                        manifestIdentitiesAuthoritative = _manifestIdentities.IsAuthoritative;
                    }
                    int launchHydratedDefaultRoots;
                    int launchHydratedDefaultProperties;
                    string[] launchHydratedDefaultFailures;
                    string managedContractId;
                    int managedContractSourceInstances;
                    lock (_managedHierarchyLock)
                    {
                        launchHydratedDefaultRoots = _launchHydratedRootDefaults.Count;
                        launchHydratedDefaultProperties = _launchHydratedRootDefaults.Values
                            .Sum(defaults => defaults.Properties.Count);
                        launchHydratedDefaultFailures = _launchHydratedDefaultFailures;
                        managedContractId = _attachedManagedContract?.ContractId ?? string.Empty;
                        managedContractSourceInstances = _attachedManagedContract?.SourceInstances ?? 0;
                    }
                    await ReplyAsync(context.Response, HttpStatusCode.OK, new
                    {
                        protocolVersion = ProtocolVersion,
                        bridgeId = _bridgeId,
                        processId = Environment.ProcessId,
                        engineReady,
                        engineGeneration,
                        studioSessionId = studioIdentity?.StudioSessionId ?? string.Empty,
                        instanceId = studioIdentity?.InstanceId ?? string.Empty,
                        hierarchySequence = Interlocked.Read(ref _hierarchySequence),
                        changeSequence = Interlocked.Read(ref _changeSequence),
                        binaryTypes = new[] { "BinaryString", "SharedString", "ContentId", "NetAssetRef", "OptionalCFrame", "UniqueId" },
                        scalarTypes = new[] { "Bool", "Int32", "Int64", "Float32", "Float64", "String", "Enum", "SecurityCapabilities" },
                        blittableTypes = new[] { "CFrame", "NumberRange", "Vector2", "Vector3", "Vector3int16" },
                        rawTypes = ExactRawTypes,
                        nativeObservation = true,
                        engineCreation = true,
                        perRootAvailability = true,
                        serializedReferences = SerializedReferences,
                        managedHierarchyAttachment = ManagedHierarchyAttachment,
                        managedContractId,
                        managedContractSourceInstances,
                        manifestIdentityLedger = ManifestIdentityLedgerSupported,
                        manifestIdentitiesAuthoritative,
                        launchHydratedDefaultRoots,
                        launchHydratedDefaultProperties,
                        launchHydratedDefaultFailures,
                        captureLeaseProtocol = CaptureEnvelope.Version,
                        captureLeaseChunkArtifact = "CARBONCM2",
                        captureLeaseRanges = true,
                        captureLeaseDigest = CaptureEnvelope.DigestAlgorithm,
                        localPlaceSaveDiagnostic = true,
                    });
                    break;

                case "/v1/managed/attach-staged":
                    {
                        var request = await ReadAsync<ManagedHierarchyAttachmentRequest>(context.Request);
                        ManagedSourceContract staged;
                        lock (_managedHierarchyLock)
                        {
                            staged = _stagedManagedSource is { } candidate
                                && string.Equals(
                                    candidate.ContractId,
                                    request.ContractId,
                                    StringComparison.Ordinal)
                                    ? candidate
                                    : throw new InvalidOperationException(
                                        "the authoritative managed hierarchy contract is not staged");
                        }
                        var result = await VerifyManagedHierarchyAsync(
                            staged.Source,
                            request.ContractId,
                            cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/manifest-identities/bootstrap":
                    {
                        var request = await ReadAsync<ManifestIdentityBootstrapRequest>(context.Request);
                        var result = await OnEngineThread(
                            () => BootstrapManifestIdentities(request),
                            cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/manifest-identities/remap-chunk":
                    {
                        var payload = await ReadBytesAsync(
                            context.Request,
                            16 + 8 + 8 + 4096 * 32,
                            cancellationToken);
                        var result = ApplyManifestIdentityRemapChunk(payload);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/managed/resolve/start":
                    {
                        var request = await ReadAsync<ManagedIdentityRequest>(context.Request);
                        if (request.SourceIds.Length + request.DebugIds.Length > 4096)
                        {
                            throw new InvalidOperationException("managed identity batch exceeds the 4096 item limit");
                        }
                        var result = StartManagedIdentityResolution(request);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/managed/resolve/poll":
                    {
                        var request = await ReadAsync<ManagedIdentityPollRequest>(context.Request);
                        var result = await PollManagedIdentityResolution(request.RequestId);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/live-session":
                    {
                        object result;
                        if (context.Request.HttpMethod == "DELETE")
                        {
                            result = await OnEngineThread(() =>
                            {
                                DestroyLiveSessionMarker();
                                return new { installed = false, engineGeneration = _engineGeneration };
                            }, cancellationToken);
                        }
                        else if (context.Request.HttpMethod == "POST")
                        {
                            var request = await ReadAsync<LiveSessionRequest>(context.Request);
                            var payload = LiveSessionContract.ValidateAndSerialize(request, JsonOptions);
                            result = await OnEngineThread(
                                () => InstallLiveSessionMarker(payload),
                                cancellationToken);
                        }
                        else
                        {
                            throw new InvalidOperationException("live session requires POST or DELETE");
                        }
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/property/read":
                    {
                        var request = await ReadAsync<PropertyRequest>(context.Request);
                        var result = await ReadPropertyAsync(request, cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/properties/read":
                    {
                        var request = await ReadAsync<PropertyBatchRequest>(context.Request);
                        if (request.Requests.Length > 4096)
                        {
                            throw new InvalidOperationException("property batch exceeds the 4096 item limit");
                        }
                        var result = await ReadPropertiesAsync(request, cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/defaults/read":
                    {
                        var request = await ReadAsync<DefaultPropertiesRequest>(context.Request);
                        if (request.Properties.Length > 4096)
                        {
                            throw new InvalidOperationException("default property batch exceeds the 4096 item limit");
                        }
                        var result = await ReadDefaultPropertiesAsync(request, cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/references/read":
                    {
                        var request = await ReadAsync<PropertyBatchRequest>(context.Request);
                        if (request.Requests.Length > 4096)
                        {
                            throw new InvalidOperationException("reference batch exceeds the 4096 item limit");
                        }
                        var result = await OnEngineThread(() => ReadReferences(request), cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/reference/write":
                    {
                        var request = await ReadAsync<ReferenceWriteRequest>(context.Request);
                        var result = await OnEngineThread(() => WriteReference(request), cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/property/write":
                    {
                        var request = await ReadAsync<PropertyWriteRequest>(context.Request);
                        var result = await OnEngineThread(() => WriteProperty(request), cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/property/copy":
                    {
                        var request = await ReadAsync<PropertyCopyRequest>(context.Request);
                        var result = await OnEngineThread(() => CopyProperty(request), cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/property/materialized-write":
                    {
                        var request = await ReadAsync<MaterializedPropertyWriteRequest>(context.Request);
                        var result = await WriteMaterializedPropertyAsync(request, cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/instance/create":
                    {
                        var request = await ReadAsync<CreateRequest>(context.Request);
                        var result = await OnEngineThread(() => CreateInstance(request), cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/roots":
                    {
                        var result = await OnEngineThread(GetRoots, cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/roots/model":
                    {
                        var request = await ReadAsync<RootModelRequest>(context.Request);
                        var result = await SerializeRootModelAsync(request, cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/roots/apply-model":
                    {
                        var request = await ReadAsync<RootApplyModelRequest>(context.Request);
                        var result = await ApplyRootModelAsync(request, cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/roots/apply-bundle":
                    {
                        var request = await ReadAsync<RootApplyBundleRequest>(context.Request);
                        var result = await ApplyRootBundleAsync(request, cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/roots/validate-model":
                    {
                        var request = await ReadAsync<RootApplyModelRequest>(context.Request);
                        var result = await ValidateRootModelAsync(request, cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/roots/validate-bundle":
                    {
                        var request = await ReadAsync<RootApplyBundleRequest>(context.Request);
                        var result = await ValidateRootBundleAsync(request, cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/diagnostics/rejected-yield":
                    {
                        var result = await TriggerRejectedYieldAsync(cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, result);
                        break;
                    }

                case "/v1/diagnostics/save-local-place":
                    {
                        if (!string.Equals(context.Request.HttpMethod, "POST", StringComparison.Ordinal))
                        {
                            throw new InvalidOperationException("local place save diagnostic requires POST");
                        }
                        if (!StudioDiagnostics.QueueLocalPlaceSaveForTesting())
                        {
                            throw new InvalidOperationException(
                                "Studio's local place save action is unavailable");
                        }
                        await ReplyAsync(context.Response, HttpStatusCode.Accepted, new
                        {
                            queued = true,
                            engineGeneration = Interlocked.Read(ref _engineGeneration),
                        });
                        break;
                    }

                case "/v1/changes":
                    {
                        var after = long.TryParse(context.Request.QueryString["after"], out var parsed) ? parsed : 0;
                        var batch = await ChangesAfterAsync(after, cancellationToken);
                        await ReplyAsync(context.Response, HttpStatusCode.OK, batch);
                        break;
                    }

                default:
                    await ReplyAsync(context.Response, HttpStatusCode.NotFound, new { error = "not_found" });
                    break;
            }
        }
        catch (KeyNotFoundException ex)
        {
            await ReplyAsync(context.Response, HttpStatusCode.NotFound, new { error = ex.Message });
        }
        catch (CaptureLeaseConflictException ex)
        {
            await ReplyAsync(context.Response, HttpStatusCode.Conflict, new { error = ex.Message });
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
            Logger.Info("Bridge request cancelled during shutdown");
            context.Response.Abort();
        }
        catch (Exception ex) when (IsExpectedClientDisconnect(ex))
        {
            Logger.Info($"Bridge client disconnected before the response completed: {ex.Message}");
            context.Response.Abort();
        }
        catch (Exception ex)
        {
            Logger.Warn($"Bridge request failed: {ex.Message}");
            await ReplyAsync(context.Response, HttpStatusCode.BadRequest, new { error = ex.Message });
        }
    }

    private async Task HandleCaptureLeaseAsync(
        HttpListenerContext context,
        string path,
        CancellationToken cancellationToken)
    {
        var manager = _captureLeases
            ?? throw new InvalidOperationException("capture lease manager is unavailable");
        const string collectionPath = "/v2/capture-leases";
        if (string.Equals(path, collectionPath, StringComparison.Ordinal))
        {
            if (!string.Equals(context.Request.HttpMethod, "POST", StringComparison.Ordinal))
            {
                await ReplyAsync(
                    context.Response,
                    HttpStatusCode.MethodNotAllowed,
                    new { error = "capture lease collection requires POST" });
                return;
            }
            var request = await ReadAsync<CaptureLeaseRequest>(context.Request);
            ValidateCaptureLeaseRequest(request);
            var status = manager.Start(request);
            await ReplyAsync(context.Response, HttpStatusCode.Accepted, status);
            return;
        }

        var segments = path[(collectionPath.Length + 1)..]
            .Split('/', StringSplitOptions.RemoveEmptyEntries);
        if (segments.Length is < 1 or > 2)
        {
            throw new KeyNotFoundException("capture lease route is unavailable");
        }
        var leaseId = segments[0];
        if (segments.Length == 1)
        {
            if (string.Equals(context.Request.HttpMethod, "GET", StringComparison.Ordinal))
            {
                await ReplyAsync(context.Response, HttpStatusCode.OK, manager.Get(leaseId));
                return;
            }
            if (string.Equals(context.Request.HttpMethod, "DELETE", StringComparison.Ordinal))
            {
                var result = manager.Delete(leaseId);
                _captureDirtyPages.Discard(result.Status.CaptureId);
                await ReplyAsync(
                    context.Response,
                    result.Released ? HttpStatusCode.OK : HttpStatusCode.Accepted,
                    result);
                return;
            }
            await ReplyAsync(
                context.Response,
                HttpStatusCode.MethodNotAllowed,
                new { error = "capture lease requires GET or DELETE" });
            return;
        }

        if (string.Equals(segments[1], "commit", StringComparison.Ordinal))
        {
            if (!string.Equals(context.Request.HttpMethod, "POST", StringComparison.Ordinal))
            {
                await ReplyAsync(
                    context.Response,
                    HttpStatusCode.MethodNotAllowed,
                    new { error = "capture page-table acknowledgement requires POST" });
                return;
            }
            var captureId = manager.EnsureReadyLease(leaseId);
            _captureDirtyPages.Acknowledge(captureId);
            await ReplyAsync(context.Response, HttpStatusCode.OK, new
            {
                captureId,
                acknowledged = true,
            });
            return;
        }

        if (!string.Equals(context.Request.HttpMethod, "GET", StringComparison.Ordinal))
        {
            await ReplyAsync(
                context.Response,
                HttpStatusCode.MethodNotAllowed,
                new { error = "capture lease artifact requires GET" });
            return;
        }
        var envelope = string.Equals(segments[1], "envelope", StringComparison.Ordinal);
        if (!envelope && !string.Equals(segments[1], "payload", StringComparison.Ordinal))
        {
            throw new KeyNotFoundException("capture lease artifact is unavailable");
        }
        var file = manager.OpenFile(leaseId, envelope, context.Request.Headers["Range"]);
        await ReplyFileAsync(
            context.Response,
            file,
            envelope ? "application/vnd.carbon.capture-envelope" : "application/vnd.roblox.rbxm",
            cancellationToken);
    }

    private void ValidateCaptureLeaseRequest(CaptureLeaseRequest request)
    {
        var identity = GetStudioIdentity();
        if (!string.Equals(identity.StudioSessionId, request.StudioSessionId, StringComparison.Ordinal)
            || !string.Equals(identity.InstanceId, request.InstanceId, StringComparison.Ordinal))
        {
            throw new InvalidOperationException("capture lease Studio route does not match this bridge");
        }
        lock (_engineStateLock)
        {
            if (_dataModel is null || _engineThreadPump is null)
            {
                throw new InvalidOperationException("edit DataModel is unavailable");
            }
            if (_engineGeneration != request.EngineGeneration)
            {
                throw new InvalidOperationException("capture lease engine generation is stale");
            }
        }
        lock (_managedHierarchyLock)
        {
            var attachedContractId = _attachedManagedContract?.ContractId ?? string.Empty;
            if (!string.Equals(
                    attachedContractId,
                    request.ManagedContractId,
                    StringComparison.OrdinalIgnoreCase))
            {
                throw new InvalidOperationException("capture lease managed hierarchy contract is stale");
            }
        }
        SetManagedObservationOwnership(request.MappedRootSourceIds, replace: true);
    }

    private static async Task ReplyFileAsync(
        HttpListenerResponse response,
        CaptureLeaseFile file,
        string contentType,
        CancellationToken cancellationToken)
    {
        response.StatusCode = file.IsPartial
            ? (int)HttpStatusCode.PartialContent
            : (int)HttpStatusCode.OK;
        response.ContentType = contentType;
        response.Headers["Accept-Ranges"] = "bytes";
        response.ContentLength64 = file.Length;
        if (file.IsPartial)
        {
            response.Headers["Content-Range"] =
                $"bytes {file.Offset}-{file.Offset + file.Length - 1}/{file.TotalLength}";
        }
        await using var input = new FileStream(
            file.Path,
            FileMode.Open,
            FileAccess.Read,
            FileShare.ReadWrite | FileShare.Delete,
            64 * 1024,
            FileOptions.Asynchronous | FileOptions.SequentialScan);
        input.Position = file.Offset;
        var remaining = file.Length;
        var buffer = new byte[64 * 1024];
        while (remaining > 0)
        {
            var read = await input.ReadAsync(
                buffer.AsMemory(0, checked((int)Math.Min(buffer.Length, remaining))),
                cancellationToken);
            if (read == 0)
            {
                throw new EndOfStreamException("capture lease artifact ended before its declared length");
            }
            await response.OutputStream.WriteAsync(buffer.AsMemory(0, read), cancellationToken);
            remaining -= read;
        }
        response.Close();
    }

    private PropertyReadResponse ReadProperty(PropertyRequest request)
    {
        var instance = Resolve(request.DebugId);
        var descriptor = SerializedPropertyAccess.Describe(instance, request.Property)
            ?? throw new InvalidOperationException("property descriptor is unavailable");
        if (!CanReadForCapture(descriptor))
        {
            throw new InvalidOperationException(
                $"property is outside Carbon's serialized-property policy ({descriptor.TypeName}; {descriptor.Attributes})");
        }
        return new PropertyReadResponse(descriptor.TypeName, Convert.ToBase64String(
            SerializedPropertyAccess.Read(instance, request.Property)), null, null);
    }

    private async Task<object> VerifyManagedHierarchyAsync(
        byte[] payload,
        CancellationToken cancellationToken)
    {
        var sourceParseTimer = Stopwatch.StartNew();
        var source = ManagedHierarchy.Parse(payload);
        Logger.Info(
            $"Managed hierarchy source contract parsed {source.Count} nodes " +
            $"in {sourceParseTimer.ElapsedMilliseconds} ms");
        return await VerifyManagedHierarchyAsync(source, null, cancellationToken);
    }

    private async Task<object> VerifyManagedHierarchyAsync(
        IReadOnlyList<ManagedSourceNode> source,
        string? contractId,
        CancellationToken cancellationToken)
    {
        if (contractId is not null)
        {
            try
            {
                return await OnEngineThread(
                    () => AttachStagedManagedHierarchy(contractId),
                    cancellationToken);
            }
            catch (InvalidDataException ex)
            {
                Logger.Info(
                    $"Managed hierarchy attachment is waiting for complete edit-mode materialization: {ex.Message}");
            }
        }

        Interlocked.Exchange(ref _managedSnapshotPending, 1);
        ArmManagedSnapshotTimer(ManagedSnapshotQuietPeriodFor(startupBoundaryAttested: true));
        await OnEngineThread(() =>
        {
            TryCaptureManagedRuntimeSnapshot(startupBoundaryAttested: true);
            return true;
        }, cancellationToken);

        Task snapshotReady;
        lock (_managedHierarchyLock)
        {
            if (AttachedManagedContractResponse(contractId) is { } response)
            {
                return response;
            }
            snapshotReady = _loadedHierarchy is null
                ? _managedSnapshotReady.Task
                : Task.CompletedTask;
        }
        var snapshotWait = snapshotReady.IsCompleted
            ? Task.CompletedTask
            : snapshotReady.WaitAsync(ManagedSnapshotReadinessTimeout, cancellationToken);
        try
        {
            await snapshotWait;
        }
        catch (TimeoutException)
        {
            throw new InvalidOperationException(
                "the edit DataModel hierarchy did not materialize before the managed attachment deadline");
        }
        lock (_managedHierarchyLock)
        {
            if (AttachedManagedContractResponse(contractId) is { } response)
            {
                return response;
            }
        }
        return VerifyManagedHierarchy(source);
    }

    private object AttachStagedManagedHierarchy(string contractId)
    {
        var dataModel = _dataModel
            ?? throw new InvalidOperationException("edit DataModel is unavailable");
        ManagedSourceContract staged;
        lock (_managedHierarchyLock)
        {
            if (AttachedManagedContractResponse(contractId) is { } response)
            {
                return response;
            }
            staged = _stagedManagedSource is { } candidate
                && string.Equals(candidate.ContractId, contractId, StringComparison.Ordinal)
                    ? candidate
                    : throw new InvalidOperationException(
                        "the authoritative managed hierarchy contract is not staged");
        }

        var source = staged.Source;
        if (source.Count == 0
            || !string.Equals(source[0].ClassName, dataModel.ClassName, StringComparison.Ordinal))
        {
            throw new InvalidDataException("managed source DataModel root is inconsistent");
        }

        var runtimeChildren = dataModel.GetChildren();
        var rootBindings = new List<(int SourceIndex, Instance Instance, string RuntimeId, string DebugId)>();
        foreach (var sourceIndex in staged.ChildrenByParent[0])
        {
            var sourceRoot = source[sourceIndex];
            if (staged.ChildrenByParent[0].Count(candidateIndex =>
                    string.Equals(source[candidateIndex].ClassName, sourceRoot.ClassName, StringComparison.Ordinal)
                    && string.Equals(source[candidateIndex].Name, sourceRoot.Name, StringComparison.Ordinal)) != 1)
            {
                throw new InvalidDataException(
                    $"managed source root {sourceRoot.ClassName} {sourceRoot.Name} is ambiguous");
            }
            var candidates = runtimeChildren.Where(candidate =>
                    string.Equals(candidate.ClassName, sourceRoot.ClassName, StringComparison.Ordinal)
                    && string.Equals(candidate.Name, sourceRoot.Name, StringComparison.Ordinal))
                .ToArray();
            if (candidates.Length != 1)
            {
                throw new InvalidDataException(
                    $"managed source root {sourceRoot.ClassName} {sourceRoot.Name} has " +
                    $"{candidates.Length} runtime matches");
            }
            var instance = candidates[0];
            rootBindings.Add((
                sourceIndex,
                instance,
                ManagedHierarchy.RuntimeIdentity(InstanceHierarchy.RuntimeHandle(instance)),
                instance.GetDebugId(128)));
        }

        var dataModelRuntimeId = ManagedHierarchy.RuntimeIdentity(
            InstanceHierarchy.RuntimeHandle(dataModel));
        var changeSequence = Interlocked.Read(ref _changeSequence);
        var hierarchySequence = Interlocked.Read(ref _hierarchySequence);
        var authoredRootHandles = rootBindings
            .Select(binding => InstanceHierarchy.RuntimeHandle(binding.Instance))
            .ToHashSet();
        var (launchHydratedRootDefaults, launchHydratedDefaultFailures) = CaptureLaunchHydratedRootDefaults(
            runtimeChildren,
            authoredRootHandles);
        var (attachedManagedRootBaselines, _) = CaptureLaunchHydratedRootDefaults(
            rootBindings.Select(binding => binding.Instance));
        lock (_changesLock)
        {
            lock (_managedHierarchyLock)
            {
                if (!ReferenceEquals(_stagedManagedSource, staged))
                {
                    throw new InvalidOperationException(
                        "the authoritative managed hierarchy contract changed during attachment");
                }
                if (AttachedManagedContractResponse(contractId) is { } response)
                {
                    return response;
                }

                _loadedHierarchy = null;
                _preVerificationHierarchyChanges = null;
                _managedBySource.Clear();
                _managedByRuntime.Clear();
                _managedByDebug.Clear();
                ReconcileLaunchHydratedRootDefaults(
                    _launchHydratedRootDefaults,
                    launchHydratedRootDefaults);
                _pendingLaunchHydratedRootDefaultRefreshes.ExceptWith(authoredRootHandles);
                _attachedManagedRootBaselines.Clear();
                foreach (var (handle, baseline) in attachedManagedRootBaselines)
                {
                    _attachedManagedRootBaselines.Add(handle, baseline);
                }
                _launchHydratedDefaultFailures = launchHydratedDefaultFailures;
                _changes.Clear();
                while (_changesReady.Wait(0))
                {
                }

                var sourceRootId = source[0].SourceId;
                var dataModelBinding = new ManagedHierarchyBinding(
                    sourceRootId,
                    dataModelRuntimeId,
                    sourceRootId,
                    dataModelRuntimeId);
                _managedBySource.Add(sourceRootId, dataModelBinding);
                _managedByRuntime.Add(dataModelRuntimeId, dataModelBinding);
                foreach (var root in rootBindings)
                {
                    var binding = new ManagedHierarchyBinding(
                        source[root.SourceIndex].SourceId,
                        root.RuntimeId,
                        source[root.SourceIndex].SourceId,
                        root.RuntimeId);
                    if (!_managedByRuntime.TryAdd(binding.DebugId, binding))
                    {
                        throw new InvalidDataException(
                            $"managed runtime root identity {binding.DebugId} is duplicated");
                    }
                    _managedBySource.Add(binding.SourceId, binding);
                    _instances[root.DebugId] = root.Instance;
                    _instances[root.RuntimeId] = root.Instance;
                }
                _instances[dataModelRuntimeId] = dataModel;
                _attachedManagedContract = new(
                    contractId,
                    source.Count,
                    hierarchySequence,
                    changeSequence,
                    rootBindings.Select(root => root.DebugId).ToArray());
            }
        }
        Logger.Info(
            $"Managed hierarchy contract {contractId} attached at " +
            $"{rootBindings.Count} DataModel roots; descendant identities are lazy");
        return AttachedManagedContractResponse(contractId)
            ?? throw new InvalidOperationException("managed attachment receipt was not committed");
    }

    private object? AttachedManagedContractResponse(string? contractId)
    {
        if (contractId is null
            || _attachedManagedContract is not { } attached
            || !string.Equals(attached.ContractId, contractId, StringComparison.Ordinal))
        {
            return null;
        }
        return new
        {
            attached = true,
            sourceInstances = attached.SourceInstances,
            hierarchySequence = attached.HierarchySequence,
            changeSequence = attached.ChangeSequence,
            sourceRootDebugIds = attached.SourceRootDebugIds,
        };
    }

    private static string[] SourceRootDebugIds(
        IReadOnlyList<ManagedSourceNode> source,
        IReadOnlyList<ManagedHierarchyMatch> matches,
        IReadOnlyDictionary<string, string> rootStudioDebugIds) => ManagedHierarchy
            .SourceRootMatches(source, matches)
            .Select(match => rootStudioDebugIds.TryGetValue(match.DebugId, out var debugId)
                ? debugId
                : throw new InvalidDataException("managed source root debug identity is unavailable"))
            .ToArray();

    private object VerifyManagedHierarchy(IReadOnlyList<ManagedSourceNode> source)
    {
        ManagedRuntimeSnapshot snapshot;
        lock (_managedHierarchyLock)
        {
            snapshot = _loadedHierarchy
                ?? throw new InvalidOperationException("the edit DataModel hierarchy snapshot is unavailable");
        }
        var runtimeOnlyRootDebugIds = snapshot.RuntimeOnlyRootDebugIds;
        IReadOnlyList<ManagedHierarchyMatch> matches;
        try
        {
            var verificationTimer = Stopwatch.StartNew();
            matches = ManagedHierarchy.Match(
                source,
                snapshot.Nodes,
                snapshot.RootDebugId,
                strategy => Logger.Info(
                    $"Managed hierarchy verification strategy: {strategy}"),
                snapshot.RuntimeShapes);
            Logger.Info(
                $"Managed hierarchy verification matched {matches.Count} nodes " +
                $"in {verificationTimer.ElapsedMilliseconds} ms");
        }
        catch
        {
            lock (_managedHierarchyLock)
            {
                if (ReferenceEquals(_loadedHierarchy, snapshot))
                {
                    _loadedHierarchy = null;
                    _preVerificationHierarchyChanges = null;
                }
            }
            throw;
        }

        long verifiedHierarchySequence;
        lock (_changesLock)
        {
            lock (_managedHierarchyLock)
            {
                if (!ReferenceEquals(_loadedHierarchy, snapshot)
                    || _preVerificationHierarchyChanges is not { } hierarchyChanges)
                {
                    throw new InvalidOperationException("the edit DataModel hierarchy snapshot is no longer current");
                }

                _loadedHierarchy = null;
                _preVerificationHierarchyChanges = null;
                ManagedHierarchy.ValidatePreVerificationChanges(
                    hierarchyChanges,
                    runtimeOnlyRootDebugIds);
                if (snapshot.HierarchySequence != Interlocked.Read(ref _hierarchySequence)
                    || snapshot.ChangeSequence != Interlocked.Read(ref _changeSequence))
                {
                    throw new InvalidOperationException(
                        "persistent edit DataModel mutation crossed the managed verification boundary");
                }

                // Runtime-only Studio construction is intentionally absent from
                // the managed source tree. Do not replay its native journal rows
                // as hidden-root changes after readiness.
                _changes.RemoveAll(change => change.Sequence > snapshot.ChangeSequence
                    && change.RootDebugId is { } rootDebugId
                    && runtimeOnlyRootDebugIds.Contains(rootDebugId));

                _managedBySource.Clear();
                _managedByRuntime.Clear();
                _managedByDebug.Clear();
                foreach (var match in matches)
                {
                    var binding = new ManagedHierarchyBinding(
                        match.SourceId,
                        match.DebugId,
                        match.RootSourceId,
                        match.RootDebugId);
                    _managedBySource.Add(binding.SourceId, binding);
                    _managedByRuntime.Add(binding.DebugId, binding);
                }
                verifiedHierarchySequence = Interlocked.Read(ref _hierarchySequence);
            }
        }
        return new
        {
            verified = true,
            sourceInstances = matches.Count,
            hierarchySequence = verifiedHierarchySequence,
            changeSequence = snapshot.ChangeSequence,
            sourceRootDebugIds = SourceRootDebugIds(source, matches, snapshot.RootStudioDebugIds),
        };
    }

    private object ResolveManagedIdentities(ManagedIdentityRequest request)
    {
        if (request.RequestId.Length == 0 || request.RequestId.Length > 128)
        {
            throw new InvalidOperationException("managed identity request id is invalid");
        }
        var bindings = new List<(ManagedHierarchyBinding Binding, bool CreateMarker)>(
            request.SourceIds.Length + request.DebugIds.Length);
        var requested = new HashSet<string>(StringComparer.Ordinal);
        foreach (var sourceId in request.SourceIds)
        {
            ManagedHierarchyBinding binding;
            lock (_managedHierarchyLock)
            {
                _managedBySource.TryGetValue(sourceId, out binding!);
            }
            binding ??= ResolveManagedSourceBinding(sourceId);
            SetManagedObservationOwnership([binding.SourceId], replace: false);
            if (!requested.Add(binding.SourceId))
            {
                continue;
            }
            bindings.Add((binding, true));
        }
        foreach (var debugId in request.DebugIds)
        {
            if (!TryFindManagedBinding(debugId, out var binding))
            {
                continue;
            }
            if (!requested.Add(binding.SourceId))
            {
                continue;
            }
            bindings.Add((binding, false));
        }

        // Engine operations can synchronously emit hierarchy callbacks. Those
        // callbacks acquire _changesLock before _managedHierarchyLock, so never
        // hold the managed lock while resolving instances or creating markers.
        var resolved = new List<object>(bindings.Count);
        foreach (var (binding, createMarker) in bindings)
        {
            var identity = HydrateManagedBinding(binding);
            var markerName = createMarker
                ? CreateManagedIdentityMarker(request.RequestId, resolved.Count, identity.Instance)
                : string.Empty;
            resolved.Add(ManagedIdentityResponse(
                binding,
                identity.DebugId,
                identity.RootDebugId,
                markerName));
        }
        return new { identities = resolved };
    }

    private object StartManagedIdentityResolution(ManagedIdentityRequest request)
    {
        if (request.RequestId.Length == 0 || request.RequestId.Length > 128)
        {
            throw new InvalidOperationException("managed identity request id is invalid");
        }
        lock (_managedIdentityResolutionLock)
        {
            if (_managedIdentityResolutions.ContainsKey(request.RequestId))
            {
                throw new InvalidOperationException("managed identity request id is already pending");
            }
            var shutdown = _shutdown?.Token ?? CancellationToken.None;
            _managedIdentityResolutions.Add(
                request.RequestId,
                OnEngineThread(() => ResolveManagedIdentities(request), shutdown));
        }
        return new { pending = true, requestId = request.RequestId };
    }

    private async Task<object> PollManagedIdentityResolution(string requestId)
    {
        Task<object> pending;
        lock (_managedIdentityResolutionLock)
        {
            if (!_managedIdentityResolutions.TryGetValue(requestId, out pending!))
            {
                throw new KeyNotFoundException("managed identity request is unavailable");
            }
            if (!pending.IsCompleted)
            {
                return new { pending = true, requestId };
            }
            _managedIdentityResolutions.Remove(requestId);
        }
        return await pending;
    }

    private bool TryFindManagedBinding(string debugId, out ManagedHierarchyBinding binding)
    {
        lock (_managedHierarchyLock)
        {
            if (_managedByDebug.TryGetValue(debugId, out binding!))
            {
                return true;
            }
        }
        if (!_instances.TryGetValue(debugId, out var instance))
        {
            binding = null!;
            return false;
        }
        if (TryResolveManagedBinding(instance, out binding)
            || TryResolveDisplacedManagedBinding(instance, out binding))
        {
            lock (_managedHierarchyLock)
            {
                CacheManagedDebugBinding(debugId, binding);
            }
            return true;
        }
        binding = null!;
        return false;
    }

    private ManagedSourceContract CurrentManagedSourceContract()
    {
        lock (_managedHierarchyLock)
        {
            var staged = _stagedManagedSource
                ?? throw new InvalidOperationException(
                    "the authoritative managed hierarchy contract is unavailable");
            if (_attachedManagedContract is not { } attached
                || !string.Equals(attached.ContractId, staged.ContractId, StringComparison.Ordinal))
            {
                throw new InvalidOperationException(
                    "the authoritative managed hierarchy contract is not attached");
            }
            return staged;
        }
    }

    private bool TryResolveManagedBinding(
        Instance requested,
        out ManagedHierarchyBinding binding)
    {
        var runtimeId = ManagedHierarchy.RuntimeIdentity(InstanceHierarchy.RuntimeHandle(requested));
        lock (_managedHierarchyLock)
        {
            if (_managedByRuntime.TryGetValue(runtimeId, out binding!))
            {
                return true;
            }
            if (_attachedManagedContract is not { } attached
                || _stagedManagedSource is not { } stagedContract
                || !string.Equals(attached.ContractId, stagedContract.ContractId, StringComparison.Ordinal))
            {
                binding = null!;
                return false;
            }
        }

        var staged = CurrentManagedSourceContract();
        var dataModel = _dataModel
            ?? throw new InvalidOperationException("edit DataModel is unavailable");
        var path = new List<Instance>();
        var current = requested;
        while (!current.Equals(dataModel))
        {
            path.Add(current);
            var parent = current.Parent;
            if (parent is null)
            {
                binding = null!;
                return false;
            }
            current = parent;
        }
        path.Reverse();

        lock (_managedHierarchyLock)
        {
            if (!_managedBySource.TryGetValue(staged.Source[0].SourceId, out binding!))
            {
                throw new InvalidOperationException("managed DataModel identity is unavailable");
            }
        }
        var parentInstance = (Instance)dataModel;
        var parentIndex = 0;
        foreach (var runtimeChild in path)
        {
            var childRuntimeId = ManagedHierarchy.RuntimeIdentity(
                InstanceHierarchy.RuntimeHandle(runtimeChild));
            lock (_managedHierarchyLock)
            {
                if (_managedByRuntime.TryGetValue(childRuntimeId, out var known))
                {
                    if (!staged.IndexBySourceId.TryGetValue(known.SourceId, out parentIndex))
                    {
                        throw new InvalidOperationException(
                            $"managed source identity {known.SourceId} is stale");
                    }
                    binding = known;
                    parentInstance = runtimeChild;
                    continue;
                }
            }
            var childIndex = FindManagedSourceChild(staged, parentIndex, parentInstance, runtimeChild);
            if (childIndex < 0)
            {
                binding = null!;
                return false;
            }
            binding = BindManagedIdentity(staged, childIndex, runtimeChild, binding);
            parentIndex = childIndex;
            parentInstance = runtimeChild;
        }
        return true;
    }

    private bool TryResolveDisplacedManagedBinding(
        Instance requested,
        out ManagedHierarchyBinding binding)
    {
        ManagedSourceContract staged;
        lock (_managedHierarchyLock)
        {
            if (_attachedManagedContract is not { } attached
                || _stagedManagedSource is not { } stagedContract
                || !string.Equals(attached.ContractId, stagedContract.ContractId, StringComparison.Ordinal))
            {
                binding = null!;
                return false;
            }
            staged = stagedContract;
        }
        var sourceIndex = ManagedHierarchy.UniqueClassNameIndex(
            staged.Source,
            requested.ClassName,
            requested.Name);
        if (sourceIndex <= 0)
        {
            binding = null!;
            return false;
        }
        return TryResolveDisplacedManagedBinding(
            staged,
            sourceIndex,
            requested,
            out binding);
    }

    private bool TryResolveDisplacedManagedBinding(
        ManagedSourceContract staged,
        int sourceIndex,
        Instance? requested,
        out ManagedHierarchyBinding binding)
    {
        var sourceNode = staged.Source[sourceIndex];
        if (ManagedHierarchy.UniqueClassNameIndex(
                staged.Source,
                sourceNode.ClassName,
                sourceNode.Name) != sourceIndex)
        {
            binding = null!;
            return false;
        }

        var candidates = new Dictionary<string, Instance>(StringComparer.Ordinal);
        foreach (var observed in _instances.Values)
        {
            if (!string.Equals(observed.ClassName, sourceNode.ClassName, StringComparison.Ordinal)
                || !string.Equals(observed.Name, sourceNode.Name, StringComparison.Ordinal)
                || DataModelRoot(observed) is null)
            {
                continue;
            }
            var runtimeId = ManagedHierarchy.RuntimeIdentity(
                InstanceHierarchy.RuntimeHandle(observed));
            candidates.TryAdd(runtimeId, observed);
        }
        if (candidates.Count != 1)
        {
            binding = null!;
            return false;
        }
        var candidate = candidates.Values.Single();
        if (requested is not null && !candidate.Equals(requested))
        {
            binding = null!;
            return false;
        }

        var rootIndex = sourceIndex;
        while (staged.Source[rootIndex].ParentIndex > 0)
        {
            rootIndex = staged.Source[rootIndex].ParentIndex;
        }
        ManagedHierarchyBinding rootBinding;
        lock (_managedHierarchyLock)
        {
            if (!ReferenceEquals(_stagedManagedSource, staged)
                || _attachedManagedContract is not { } verified
                || !string.Equals(verified.ContractId, staged.ContractId, StringComparison.Ordinal)
                || !_managedBySource.TryGetValue(
                    staged.Source[rootIndex].SourceId,
                    out rootBinding!))
            {
                binding = null!;
                return false;
            }
        }
        binding = BindManagedIdentity(staged, sourceIndex, candidate, rootBinding);
        return true;
    }

    private ManagedHierarchyBinding ResolveManagedSourceBinding(string sourceId)
    {
        var staged = CurrentManagedSourceContract();
        if (!staged.IndexBySourceId.TryGetValue(sourceId, out var requestedIndex))
        {
            throw new KeyNotFoundException($"managed source identity {sourceId} is unavailable");
        }
        lock (_managedHierarchyLock)
        {
            if (_managedBySource.TryGetValue(sourceId, out var known))
            {
                return known;
            }
        }
        if (TryResolveDisplacedManagedBinding(
                staged,
                requestedIndex,
                null,
                out var displaced))
        {
            return displaced;
        }

        var path = new Stack<int>();
        for (var index = requestedIndex; index > 0; index = staged.Source[index].ParentIndex)
        {
            path.Push(index);
        }
        var parentInstance = (Instance)(_dataModel
            ?? throw new InvalidOperationException("edit DataModel is unavailable"));
        var parentIndex = 0;
        ManagedHierarchyBinding binding;
        lock (_managedHierarchyLock)
        {
            binding = _managedBySource[staged.Source[0].SourceId];
        }
        while (path.TryPop(out var childIndex))
        {
            var childSourceId = staged.Source[childIndex].SourceId;
            lock (_managedHierarchyLock)
            {
                if (_managedBySource.TryGetValue(childSourceId, out var known))
                {
                    binding = known;
                    parentInstance = Resolve(known.DebugId);
                    parentIndex = childIndex;
                    continue;
                }
            }
            var runtimeChild = FindManagedRuntimeChild(
                staged,
                parentIndex,
                parentInstance,
                childIndex);
            binding = BindManagedIdentity(staged, childIndex, runtimeChild, binding);
            parentInstance = runtimeChild;
            parentIndex = childIndex;
        }
        return binding;
    }

    private static int FindManagedSourceChild(
        ManagedSourceContract staged,
        int parentIndex,
        Instance runtimeParent,
        Instance runtimeChild)
    {
        var source = staged.Source;
        var sourceChildren = staged.ChildrenByParent[parentIndex];
        var exact = sourceChildren.Where(index =>
                string.Equals(source[index].ClassName, runtimeChild.ClassName, StringComparison.Ordinal)
                && string.Equals(source[index].Name, runtimeChild.Name, StringComparison.Ordinal))
            .ToArray();
        if (exact.Length == 1)
        {
            return exact[0];
        }
        if (exact.Length > 1)
        {
            throw new InvalidOperationException(
                $"managed runtime identity {runtimeChild.ClassName} {runtimeChild.Name} is ambiguous");
        }

        var runtimeChildren = runtimeParent.GetChildren();
        var unmatchedSource = UnmatchedManagedSourceChildren(staged, parentIndex, runtimeChildren)
            .Where(index => string.Equals(
                source[index].ClassName,
                runtimeChild.ClassName,
                StringComparison.Ordinal))
            .ToArray();
        var unmatchedRuntime = UnmatchedManagedRuntimeChildren(staged, parentIndex, runtimeChildren)
            .Where(candidate => string.Equals(
                candidate.ClassName,
                runtimeChild.ClassName,
                StringComparison.Ordinal))
            .ToArray();
        return unmatchedSource.Length == 1
            && unmatchedRuntime.Length == 1
            && unmatchedRuntime[0].Equals(runtimeChild)
                ? unmatchedSource[0]
                : -1;
    }

    private static Instance FindManagedRuntimeChild(
        ManagedSourceContract staged,
        int parentIndex,
        Instance runtimeParent,
        int childIndex)
    {
        var sourceChild = staged.Source[childIndex];
        var runtimeChildren = runtimeParent.GetChildren();
        var exact = runtimeChildren.Where(candidate =>
                string.Equals(candidate.ClassName, sourceChild.ClassName, StringComparison.Ordinal)
                && string.Equals(candidate.Name, sourceChild.Name, StringComparison.Ordinal))
            .ToArray();
        if (exact.Length == 1)
        {
            return exact[0];
        }
        if (exact.Length > 1)
        {
            throw new InvalidOperationException(
                $"managed source identity {sourceChild.SourceId} has an ambiguous runtime match");
        }

        var unmatchedSource = UnmatchedManagedSourceChildren(staged, parentIndex, runtimeChildren)
            .Where(index => string.Equals(
                staged.Source[index].ClassName,
                sourceChild.ClassName,
                StringComparison.Ordinal))
            .ToArray();
        var unmatchedRuntime = UnmatchedManagedRuntimeChildren(staged, parentIndex, runtimeChildren)
            .Where(candidate => string.Equals(
                candidate.ClassName,
                sourceChild.ClassName,
                StringComparison.Ordinal))
            .ToArray();
        if (unmatchedSource.Length == 1
            && unmatchedSource[0] == childIndex
            && unmatchedRuntime.Length == 1)
        {
            return unmatchedRuntime[0];
        }
        throw new KeyNotFoundException(
            $"managed source identity {sourceChild.SourceId} is unavailable or ambiguous");
    }

    private static IEnumerable<int> UnmatchedManagedSourceChildren(
        ManagedSourceContract staged,
        int parentIndex,
        IReadOnlyList<Instance> runtimeChildren)
    {
        foreach (var sourceIndex in staged.ChildrenByParent[parentIndex])
        {
            var sourceChild = staged.Source[sourceIndex];
            var sourceDuplicates = staged.ChildrenByParent[parentIndex].Count(candidateIndex =>
                string.Equals(staged.Source[candidateIndex].ClassName, sourceChild.ClassName, StringComparison.Ordinal)
                && string.Equals(staged.Source[candidateIndex].Name, sourceChild.Name, StringComparison.Ordinal));
            var runtimeMatches = runtimeChildren.Count(candidate =>
                string.Equals(candidate.ClassName, sourceChild.ClassName, StringComparison.Ordinal)
                && string.Equals(candidate.Name, sourceChild.Name, StringComparison.Ordinal));
            if (sourceDuplicates > 1 || runtimeMatches > 1)
            {
                throw new InvalidOperationException(
                    $"managed sibling identity {sourceChild.ClassName} {sourceChild.Name} is ambiguous");
            }
            if (runtimeMatches == 0)
            {
                yield return sourceIndex;
            }
        }
    }

    private static IEnumerable<Instance> UnmatchedManagedRuntimeChildren(
        ManagedSourceContract staged,
        int parentIndex,
        IReadOnlyList<Instance> runtimeChildren)
    {
        foreach (var runtimeChild in runtimeChildren)
        {
            var sourceMatches = staged.ChildrenByParent[parentIndex].Count(sourceIndex =>
                string.Equals(staged.Source[sourceIndex].ClassName, runtimeChild.ClassName, StringComparison.Ordinal)
                && string.Equals(staged.Source[sourceIndex].Name, runtimeChild.Name, StringComparison.Ordinal));
            var runtimeDuplicates = runtimeChildren.Count(candidate =>
                string.Equals(candidate.ClassName, runtimeChild.ClassName, StringComparison.Ordinal)
                && string.Equals(candidate.Name, runtimeChild.Name, StringComparison.Ordinal));
            if (sourceMatches > 1 || (sourceMatches == 1 && runtimeDuplicates > 1))
            {
                throw new InvalidOperationException(
                    $"managed sibling identity {runtimeChild.ClassName} {runtimeChild.Name} is ambiguous");
            }
            if (sourceMatches == 0)
            {
                yield return runtimeChild;
            }
        }
    }

    private ManagedHierarchyBinding BindManagedIdentity(
        ManagedSourceContract staged,
        int sourceIndex,
        Instance instance,
        ManagedHierarchyBinding parent)
    {
        var sourceNode = staged.Source[sourceIndex];
        var runtimeId = ManagedHierarchy.RuntimeIdentity(InstanceHierarchy.RuntimeHandle(instance));
        var rootSourceId = sourceNode.ParentIndex == 0
            ? sourceNode.SourceId
            : parent.RootSourceId;
        var rootRuntimeId = sourceNode.ParentIndex == 0
            ? runtimeId
            : parent.RootDebugId;
        lock (_managedHierarchyLock)
        {
            if (!ReferenceEquals(_stagedManagedSource, staged)
                || _attachedManagedContract is not { } verified
                || !string.Equals(verified.ContractId, staged.ContractId, StringComparison.Ordinal))
            {
                throw new InvalidOperationException(
                    "the authoritative managed hierarchy contract changed during identity resolution");
            }
            if (_managedBySource.TryGetValue(sourceNode.SourceId, out var existingSource))
            {
                if (!string.Equals(existingSource.DebugId, runtimeId, StringComparison.Ordinal))
                {
                    throw new ManagedSourceReplacementPendingException(sourceNode.SourceId);
                }
                return existingSource;
            }
            if (_managedByRuntime.TryGetValue(runtimeId, out var existingRuntime))
            {
                throw new InvalidOperationException(
                    $"managed runtime identity {runtimeId} is already bound to " +
                    existingRuntime.SourceId);
            }
            var binding = new ManagedHierarchyBinding(
                sourceNode.SourceId,
                runtimeId,
                rootSourceId,
                rootRuntimeId);
            _managedBySource.Add(binding.SourceId, binding);
            _managedByRuntime.Add(binding.DebugId, binding);
            _instances[runtimeId] = instance;
            return binding;
        }
    }

    private (Instance Instance, string DebugId, string RootDebugId) HydrateManagedBinding(
        ManagedHierarchyBinding binding)
    {
        lock (_managedHierarchyLock)
        {
            EnsureManagedBindingCurrent(binding);
        }
        EnsureManagedHierarchyLockReleasedForEngineWork();

        var instance = Resolve(binding.DebugId);
        var root = Resolve(binding.RootDebugId);
        var debugId = instance.GetDebugId(128);
        var rootDebugId = root.GetDebugId(128);
        lock (_managedHierarchyLock)
        {
            EnsureManagedBindingCurrent(binding);
            if (_managedByDebug.TryGetValue(debugId, out var existing)
                && !string.Equals(existing.SourceId, binding.SourceId, StringComparison.Ordinal))
            {
                throw new InvalidOperationException(
                    $"managed runtime identity {debugId} is duplicated");
            }
            _managedByDebug[debugId] = binding;
        }
        _instances[debugId] = instance;
        _instances[rootDebugId] = root;
        return (instance, debugId, rootDebugId);
    }

    private void EnsureManagedBindingCurrent(ManagedHierarchyBinding binding)
    {
        if (!_managedBySource.TryGetValue(binding.SourceId, out var current)
            || !ReferenceEquals(current, binding)
            || !_managedByRuntime.TryGetValue(binding.DebugId, out current)
            || !ReferenceEquals(current, binding))
        {
            throw new InvalidOperationException(
                $"managed source identity {binding.SourceId} is stale");
        }
    }

    private void EnsureManagedHierarchyLockReleasedForEngineWork()
    {
        if (Monitor.IsEntered(_managedHierarchyLock))
        {
            throw new InvalidOperationException(
                "managed hierarchy lock cannot be held during engine work");
        }
    }

    private static object ManagedIdentityResponse(
        ManagedHierarchyBinding binding,
        string debugId,
        string rootDebugId,
        string markerName) => new
        {
            sourceId = binding.SourceId,
            debugId,
            markerName,
            rootDebugId,
            rootSourceId = binding.RootSourceId,
        };

    private string CreateManagedIdentityMarker(string requestId, int index, Instance value)
    {
        EnsureManagedHierarchyLockReleasedForEngineWork();
        var dataModel = _dataModel ?? throw new InvalidOperationException("edit DataModel is unavailable");
        var coreGui = dataModel.GetService<CoreGui>();
        var handle = Reflection.CreateInstance("ObjectValue", CreatorRole.Engine);
        var marker = Instance.FromHandle(handle)
            ?? throw new InvalidOperationException("engine could not create a managed identity marker");
        marker.Name = $"{ManagedIdentityMarkerPrefix}{requestId}:{index}";
        marker.Archivable = false;
        Reflection.SetProperty<Instance?>(marker, "Value", value);
        marker.Parent = coreGui;
        return marker.Name;
    }

    private object InstallLiveSessionMarker(string payload)
    {
        var dataModel = _dataModel ?? throw new InvalidOperationException("edit DataModel is unavailable");
        var coreGui = dataModel.GetService<CoreGui>();
        LiveSessionMarkerLifecycle.Replace(
            ref _liveSessionMarker,
            marker => marker.Destroy(),
            () =>
        {
            var handle = Reflection.CreateInstance("StringValue", CreatorRole.Engine);
            var marker = Instance.FromHandle(handle)
                ?? throw new InvalidOperationException("engine could not create the Carbon live-session marker");
            marker.Name = LiveSessionContract.MarkerName;
            marker.Archivable = false;
            Reflection.SetProperty(marker, "Value", payload);
            marker.Parent = coreGui;
            return marker;
        });
        return new { installed = true, engineGeneration = _engineGeneration };
    }

    private void DestroyLiveSessionMarker()
    {
        var marker = _liveSessionMarker;
        _liveSessionMarker = null;
        if (marker is null)
        {
            return;
        }
        try
        {
            marker.Destroy();
        }
        catch
        {
        }
    }

    private async Task<PropertyReadResponse> ReadPropertyAsync(
        PropertyRequest request,
        CancellationToken cancellationToken)
    {
        var response = await ReadPropertiesAsync(
            new PropertyBatchRequest([request]),
            cancellationToken);
        var value = response.Values.Single();
        if (value.Error is not null)
        {
            throw new InvalidOperationException(value.Error);
        }
        return new PropertyReadResponse(
            value.TypeName ?? throw new InvalidOperationException("property read returned no type"),
            value.Value ?? string.Empty,
            response.Model,
            value.ModelRootDebugId);
    }

    private async Task<PropertyBatchResponse> ReadPropertiesAsync(
        PropertyBatchRequest request,
        CancellationToken cancellationToken)
    {
        var capture = await OnEngineThread(() =>
        {
            var values = new PropertyBatchRead[request.Requests.Length];
            var serializationRoots = new List<Instance>();
            var carrierByKey = new Dictionary<string, Instance>(StringComparer.Ordinal);
            for (var index = 0; index < request.Requests.Length; index++)
            {
                try
                {
                    var item = request.Requests[index];
                    var instance = Resolve(item.DebugId);
                    var descriptor = SerializedPropertyAccess.Describe(instance, item.Property)
                        ?? throw new InvalidOperationException("property descriptor is unavailable");
                    var serializedThroughModel = UsesSerializedPropertyCarrier(descriptor);
                    if (!CanReadForCapture(descriptor))
                    {
                        throw new InvalidOperationException(
                            $"property is outside Carbon's serialized-property policy ({descriptor.TypeName}; {descriptor.Attributes})");
                    }

                    if (serializedThroughModel)
                    {
                        var carrierClass = SerializedPropertyCarrierClass(instance.ClassName, descriptor);
                        var carrierKey = $"{item.DebugId}|{carrierClass}";
                        if (!carrierByKey.TryGetValue(carrierKey, out var carrier))
                        {
                            Instance? wrapper = null;
                            try
                            {
                                var carrierHandle = Reflection.CreateInstance(carrierClass, CreatorRole.Engine);
                                carrier = Instance.FromHandle(carrierHandle)
                                    ?? throw new InvalidOperationException(
                                        "engine could not create a serialized-property carrier");
                                carrier.Name = instance.Name;
                                var handle = Reflection.CreateInstance("Folder", CreatorRole.Engine);
                                wrapper = Instance.FromHandle(handle)
                                    ?? throw new InvalidOperationException("engine could not create a serialized-property wrapper");
                                wrapper.Name = carrierKey;
                                carrier.Parent = wrapper;
                                carrierByKey.Add(carrierKey, carrier);
                                serializationRoots.Add(wrapper);
                            }
                            catch
                            {
                                if (wrapper is not null)
                                {
                                    wrapper.Destroy();
                                }
                                else
                                {
                                    carrier?.Destroy();
                                }
                                throw;
                            }
                        }
                        if (!SerializedPropertyAccess.Copy(instance, carrier, item.Property))
                        {
                            throw new InvalidOperationException(
                                $"engine rejected the serialized-property carrier copy for " +
                                $"{instance.ClassName}.{item.Property} ({descriptor.TypeName}; {descriptor.Attributes})");
                        }
                        values[index] = new PropertyBatchRead(descriptor.TypeName, string.Empty, null, carrierKey);
                    }
                    else
                    {
                        var value = ReadProperty(item);
                        values[index] = new PropertyBatchRead(value.TypeName, value.Value, null, null);
                    }
                }
                catch (Exception ex)
                {
                    values[index] = new PropertyBatchRead(null, null, ex.Message, null);
                }
            }

            if (serializationRoots.Count == 0)
            {
                return new PropertyBatchCapture(values, null, []);
            }
            var dataModel = _dataModel ?? throw new InvalidOperationException("edit DataModel is unavailable");
            var service = dataModel.GetService<SerializationService>();
            IReadOnlyList<Instance> serializationInput = serializationRoots;
            return new PropertyBatchCapture(
                values,
                Reflection.InvokeAsync<byte[]>(service, "SerializeInstancesAsync", serializationInput),
                serializationRoots.ToArray());
        }, cancellationToken);

        byte[]? model = null;
        try
        {
            model = capture.Serialization is null
                ? null
                : await capture.Serialization.WaitAsync(TimeSpan.FromSeconds(30), cancellationToken)
                    ?? throw new InvalidOperationException("engine returned no serialized property model");
        }
        finally
        {
            if (capture.SerializationRoots.Length > 0)
            {
                await OnEngineThread(() =>
                {
                    foreach (var root in capture.SerializationRoots)
                    {
                        root.Destroy();
                    }
                    return (object?)null;
                }, cancellationToken);
            }
        }
        return new PropertyBatchResponse(
            capture.Values,
            model is null ? null : Convert.ToBase64String(model));
    }

    private async Task<PropertyBatchResponse> ReadDefaultPropertiesAsync(
        DefaultPropertiesRequest request,
        CancellationToken cancellationToken)
    {
        var instance = await OnEngineThread(() =>
        {
            var handle = Reflection.CreateInstance(request.ClassName, CreatorRole.Engine);
            var created = Instance.FromHandle(handle)
                ?? throw new InvalidOperationException($"engine could not create default class '{request.ClassName}'");
            created.Name = "__CarbonExactDefault";
            Index(created);
            return created;
        }, cancellationToken);
        var debugId = instance.GetDebugId(128);
        try
        {
            return await ReadPropertiesAsync(
                new PropertyBatchRequest(
                    request.Properties.Select(property => new PropertyRequest(debugId, property)).ToArray()),
                cancellationToken);
        }
        finally
        {
            await OnEngineThread(() =>
            {
                _instances.TryRemove(debugId, out _);
                instance.Destroy();
                return (object?)null;
            }, CancellationToken.None);
        }
    }

    private ReferenceBatchResponse ReadReferences(PropertyBatchRequest request)
    {
        var values = new ReferenceBatchRead[request.Requests.Length];
        for (var index = 0; index < request.Requests.Length; index++)
        {
            try
            {
                var item = request.Requests[index];
                var instance = Resolve(item.DebugId);
                var descriptor = SerializedPropertyAccess.Describe(instance, item.Property)
                    ?? throw new InvalidOperationException("property descriptor is unavailable");
                if (!CanTransportReference(descriptor))
                {
                    throw new InvalidOperationException(
                        $"property is outside Carbon's serialized-reference policy ({descriptor.TypeName}; {descriptor.Attributes})");
                }
                var target = Reflection.GetProperty<Instance>(instance, item.Property);
                var targetDebugId = target?.GetDebugId(128);
                var sourceId = target is null || targetDebugId is null
                    ? null
                    : AssociateManagedBinding(target, targetDebugId);
                values[index] = new ReferenceBatchRead(targetDebugId, sourceId, null);
            }
            catch (Exception ex)
            {
                values[index] = new ReferenceBatchRead(null, null, ex.Message);
            }
        }
        return new ReferenceBatchResponse(values);
    }

    private object WriteReference(ReferenceWriteRequest request)
    {
        var instance = Resolve(request.DebugId);
        var descriptor = SerializedPropertyAccess.Describe(instance, request.Property)
            ?? throw new InvalidOperationException("property descriptor is unavailable");
        if (!CanTransportReference(descriptor))
        {
            throw new InvalidOperationException(
                $"property is outside Carbon's serialized-reference policy ({descriptor.TypeName}; {descriptor.Attributes})");
        }
        var target = ResolveOptionalReferenceTarget(request.TargetDebugId, Resolve);
        Reflection.SetProperty<Instance?>(instance, request.Property, target);
        return new { written = true };
    }

    private object WriteProperty(PropertyWriteRequest request)
    {
        var instance = Resolve(request.DebugId);
        var value = Convert.FromBase64String(request.Value);
        var descriptor = SerializedPropertyAccess.Describe(instance, request.Property)
            ?? throw new InvalidOperationException("property descriptor is unavailable");
        if (!descriptor.IsAccessible)
        {
            throw new InvalidOperationException(
                $"property is outside Carbon's serialized-property policy ({descriptor.TypeName}; {descriptor.Attributes})");
        }
        if (!SerializedPropertyAccess.Write(instance, request.Property, value))
        {
            throw new InvalidOperationException("engine rejected the serialized property write");
        }
        return new { written = true };
    }

    private object CopyProperty(PropertyCopyRequest request)
    {
        var source = Resolve(request.SourceDebugId);
        var target = Resolve(request.TargetDebugId);
        if (source.ClassName != target.ClassName)
        {
            throw new InvalidOperationException("serialized-property copy requires matching instance classes");
        }
        var descriptor = SerializedPropertyAccess.Describe(target, request.Property)
            ?? throw new InvalidOperationException("property descriptor is unavailable");
        if (!descriptor.IsAccessible)
        {
            throw new InvalidOperationException(
                $"property is outside Carbon's serialized-property policy ({descriptor.TypeName}; {descriptor.Attributes})");
        }
        if (!SerializedPropertyAccess.Copy(source, target, request.Property))
        {
            throw new InvalidOperationException("engine rejected the serialized property copy");
        }
        return new { copied = true };
    }

    private async Task<object> WriteMaterializedPropertyAsync(
        MaterializedPropertyWriteRequest request,
        CancellationToken cancellationToken)
    {
        var bytes = Convert.FromBase64String(request.Model);
        var deserialization = await OnEngineThread(() =>
        {
            var dataModel = _dataModel ?? throw new InvalidOperationException("edit DataModel is unavailable");
            return dataModel.GetService<SerializationService>().DeserializeInstancesAsync(bytes);
        }, cancellationToken);
        var instances = await deserialization.WaitAsync(TimeSpan.FromSeconds(30), cancellationToken)
            ?? throw new InvalidOperationException("engine returned no materialized property instances");
        try
        {
            return await OnEngineThread(() =>
            {
                if (instances.Count != 1)
                {
                    throw new InvalidOperationException("materialized property model must contain exactly one instance");
                }
                var source = instances[0];
                var target = Resolve(request.DebugId);
                if (source.ClassName != target.ClassName)
                {
                    throw new InvalidOperationException("materialized property owner class does not match target");
                }
                var descriptor = SerializedPropertyAccess.Describe(target, request.Property)
                    ?? throw new InvalidOperationException("property descriptor is unavailable");
                if (!CanWriteMaterialized(target.ClassName, descriptor))
                {
                    throw new InvalidOperationException(
                        $"property is outside Carbon's materialized-property policy ({descriptor.TypeName}; {descriptor.Attributes})");
                }
                if (!SerializedPropertyAccess.Copy(source, target, request.Property))
                {
                    throw new InvalidOperationException("engine rejected the materialized property copy");
                }
                return (object)new { written = true };
            }, cancellationToken);
        }
        finally
        {
            await OnEngineThread(() =>
            {
                foreach (var instance in instances)
                {
                    instance.Destroy();
                }
                return (object?)null;
            }, CancellationToken.None);
        }
    }

    private object CreateInstance(CreateRequest request)
    {
        var parent = Resolve(request.ParentDebugId);
        var handle = Reflection.CreateInstance(request.ClassName, CreatorRole.Engine);
        var instance = Instance.FromHandle(handle)
            ?? throw new InvalidOperationException($"engine could not create class '{request.ClassName}'");
        instance.Name = request.Name;
        instance.Parent = parent;
        Index(instance);
        return new { debugId = instance.GetDebugId(128) };
    }

    private object GetRoots()
    {
        var dataModel = _dataModel ?? throw new InvalidOperationException("edit DataModel is unavailable");
        return new
        {
            roots = GetSerializableRoots(dataModel).Select(root => root.Identity).ToArray()
        };
    }

    private SerializableRoot[] GetSerializableRoots(DataModel dataModel)
    {
        var roots = new List<SerializableRoot>();
        foreach (var instance in dataModel.GetChildren())
        {
            if (!instance.Archivable || !Reflection.IsSerializable(instance))
            {
                continue;
            }

            var className = Reflection.GetProperty<string>(instance, "ClassName");
            if (ManagedHierarchy.IsInternalDataModelRoot(className))
            {
                // Studio's internal FilteredSelection objects expose no valid
                // reflection class and are not authored place roots.
                continue;
            }

            var debugId = instance.GetDebugId(128);
            roots.Add(new SerializableRoot(
                instance,
                new RootIdentity(
                    className,
                    instance.Name,
                    debugId,
                    // OnDataModelLoaded can precede late serialized root parenting.
                    // A root returned by this endpoint is present at the handshake
                    // baseline; the plugin intersects it with its frozen hierarchy.
                    true)));
        }
        return roots.ToArray();
    }

    private async Task<CaptureEnvelopeData> CaptureLeaseSnapshotAsync(
        CaptureLeaseRequest request,
        Action<CaptureLeasePhase> reportPhase,
        CaptureModelArtifactWriter modelWriter,
        CancellationToken cancellationToken)
    {
        var captureStarted = Stopwatch.StartNew();
        var managedBytesBefore = GC.GetTotalMemory(forceFullCollection: false);
        var allocatedBytesBefore = GC.GetTotalAllocatedBytes(precise: false);
        using var process = Process.GetCurrentProcess();
        var workingSetBefore = process.WorkingSet64;
        ValidateCaptureLeaseRequest(request);
        var acquisitionStarted = Stopwatch.StartNew();
        var acquisition = await OnEngineThread(
            () => AcquireCaptureLeaseSnapshot(request),
            cancellationToken);
        var acquisitionElapsed = acquisitionStarted.Elapsed;
        cancellationToken.ThrowIfCancellationRequested();
        var planningStarted = Stopwatch.StartNew();
        var capture = await Task.Run(
            () => PlanCaptureLeaseSerializationAsync(
                request,
                acquisition,
                reportPhase,
                cancellationToken),
            cancellationToken);
        var planningElapsed = planningStarted.Elapsed;
        var acquisitionHierarchySequence = acquisition.HierarchySequence;
        var acquisitionChangeSequence = acquisition.ChangeSequence;
        // Planning owns parsed arrays after this point. Do not let the outer
        // capture coroutine retain the dense native wire payload across every
        // serializer chunk.
        acquisition = acquisition with { NativePayload = [] };

        modelWriter.Begin(capture.Chunks.Length);
        CaptureArchivableMaskEntry[] mappedMask = [];
        Exception? chunkFailure = null;
        var launchElapsed = TimeSpan.Zero;
        var settlementElapsed = TimeSpan.Zero;
        var writeElapsed = TimeSpan.Zero;
        var serializedBytes = 0L;
        var reusedPageCount = 0;
        var reusedPayloadBytes = 0L;
        var chunkMilliseconds = new List<double>(capture.Chunks.Length);
        Task<(TimeSpan Elapsed, long Bytes)>? pendingWrite = null;
        async Task DrainPendingWriteAsync()
        {
            if (pendingWrite is null)
            {
                return;
            }
            var write = pendingWrite;
            pendingWrite = null;
            var result = await write.ConfigureAwait(false);
            writeElapsed += result.Elapsed;
            serializedBytes = checked(serializedBytes + result.Bytes);
        }
        try
        {
            cancellationToken.ThrowIfCancellationRequested();
            if (capture.MappedRoots.Length != 0)
            {
                mappedMask = await OnEngineThreadUninterruptibleOnceStarted(
                    () => ApplyCaptureArchivableMask(
                        request.CaptureId,
                        capture.MappedRoots),
                    CancellationToken.None,
                    timeoutBeforeStart: false);
            }
            foreach (var chunk in capture.Chunks)
            {
                var chunkStarted = Stopwatch.StartNew();
                cancellationToken.ThrowIfCancellationRequested();
                if (chunk.ReusedPayload is { } reusedPayload)
                {
                    if (chunk.PageIndex < 0
                        || chunk.PageIndex >= capture.PagePlan.Pages.Length)
                    {
                        throw new InvalidDataException(
                            "capture reused page index is invalid");
                    }
                    await DrainPendingWriteAsync();
                    pendingWrite = Task.Run(() =>
                    {
                        try
                        {
                            var writeStarted = Stopwatch.StartNew();
                            using var input = reusedPayload.OpenRead();
                            modelWriter.WriteChunk(
                                chunk.RootOrdinals,
                                input,
                                reusedPayload.Length,
                                reusedPayload.Digest);
                            return (writeStarted.Elapsed, reusedPayload.Length);
                        }
                        catch
                        {
                            _captureDirtyPages.Poison();
                            throw;
                        }
                    }, CancellationToken.None);
                    reusedPageCount++;
                    reusedPayloadBytes = checked(
                        reusedPayloadBytes + reusedPayload.Length);
                    chunkMilliseconds.Add(chunkStarted.Elapsed.TotalMilliseconds);
                    continue;
                }
                var launchStarted = Stopwatch.StartNew();
                var launched = await OnEngineThreadUninterruptibleOnceStarted(() =>
                {
                    ValidateCaptureLeaseRequest(request);
                    EnsureCaptureLeaseEpochsUnchanged(
						acquisitionHierarchySequence,
						acquisitionChangeSequence,
                        Interlocked.Read(ref _hierarchySequence),
                        Interlocked.Read(ref _changeSequence),
                        "between capture chunks");
                    var dataModel = _dataModel
                        ?? throw new InvalidOperationException("edit DataModel is unavailable");
                    var roots = chunk.RootHandles
                        .Select(handle => Instance.FromHandle(handle)
                            ?? throw new InvalidDataException(
                                "capture component root handle is unavailable"))
                        .ToArray();
                    var maskedRoots = chunk.MaskedRootHandles
                        .Select(handle => Instance.FromHandle(handle)
                            ?? throw new InvalidDataException(
                                "capture frontier root handle is unavailable"))
                        .ToArray();
                    var mask = ApplyCaptureArchivableMask(
                        request.CaptureId,
                        maskedRoots);
                    try
                    {
                        var serialization = Reflection.InvokeAsync<byte[]>(
                            dataModel.GetService<SerializationService>(),
                            "SerializeInstancesAsync",
                            CaptureSerializerArguments(roots));
                        return new CaptureLeaseEngineChunk(serialization, mask);
                    }
                    catch (Exception serializationFailure)
                    {
                        try
                        {
                            RestoreCaptureArchivableMask(request.CaptureId, mask);
                        }
                        catch (Exception restorationFailure)
                        {
                            throw new AggregateException(
                                "capture chunk launch and Archivable restoration both failed",
                                serializationFailure,
                                new InvalidOperationException(
                                    "capture cleanup and Archivable restoration failed",
                                    restorationFailure));
                        }
                        System.Runtime.ExceptionServices.ExceptionDispatchInfo
                            .Capture(serializationFailure)
                            .Throw();
                        throw new UnreachableException();
                    }
                }, CancellationToken.None, timeoutBeforeStart: false);
                launchElapsed += launchStarted.Elapsed;

                // SerializeInstancesAsync has no engine cancellation primitive.
                // Once one bounded chunk starts, await settlement and restore its
                // masks before observing cancellation or launching the next chunk.
                var settlementStarted = Stopwatch.StartNew();
                var payload = await AwaitCaptureSerializerWithRestoration(
                    launched.Serialization,
                    async () => await OnEngineThreadUninterruptibleOnceStarted(() =>
                    {
                        RestoreCaptureArchivableMask(
                            request.CaptureId,
                            launched.TemporarilyNonArchivableRoots);
                        return (object?)null;
                    }, CancellationToken.None, timeoutBeforeStart: false))
                    ?? throw new InvalidOperationException(
                        "engine returned no serialized capture chunk");
                settlementElapsed += settlementStarted.Elapsed;
                await DrainPendingWriteAsync();
                var ownedPayload = payload;
                var rootOrdinals = chunk.RootOrdinals;
                var pageIndex = chunk.PageIndex;
                pendingWrite = Task.Run(() =>
                {
                    var writeStarted = Stopwatch.StartNew();
                    if (pageIndex >= 0)
                    {
                        _captureDirtyPages.StoreSerializedPage(
                            capture.PagePlan,
                            pageIndex,
                            ownedPayload);
                    }
                    modelWriter.WriteChunk(rootOrdinals, ownedPayload);
                    return (writeStarted.Elapsed, ownedPayload.LongLength);
                }, CancellationToken.None);
                chunkMilliseconds.Add(chunkStarted.Elapsed.TotalMilliseconds);
            }
            await DrainPendingWriteAsync();
        }
        catch (Exception ex)
        {
            try
            {
                await DrainPendingWriteAsync();
                chunkFailure = ex;
            }
            catch (Exception writeFailure)
            {
                chunkFailure = new AggregateException(
                    "capture serialization and payload spooling failed",
                    ex,
                    writeFailure);
            }
        }

        Exception? cleanupFailure = null;
        if (mappedMask.Length != 0 || capture.TemporaryRoots.Length != 0)
        {
            try
            {
                await OnEngineThreadUninterruptibleOnceStarted(() =>
                {
                    List<Exception>? failures = null;
                    if (mappedMask.Length != 0)
                    {
                        try
                        {
                            RestoreCaptureArchivableMask(request.CaptureId, mappedMask);
                        }
                        catch (Exception ex)
                        {
                            (failures ??= []).Add(ex);
                        }
                    }
                    try
                    {
                        DestroyCaptureTemporaryRoots(capture.TemporaryRoots);
                    }
                    catch (Exception ex)
                    {
                        (failures ??= []).Add(ex);
                    }
                    if (failures is not null)
                    {
                        throw new AggregateException(
                            "capture plan cleanup failed",
                            failures);
                    }
                    return (object?)null;
                }, CancellationToken.None, timeoutBeforeStart: false);
            }
            catch (Exception ex)
            {
                cleanupFailure = ex;
            }
        }
        if (chunkFailure is not null && cleanupFailure is not null)
        {
            throw new AggregateException(
                "capture chunk execution and cleanup both failed",
                chunkFailure,
                cleanupFailure);
        }
        if (chunkFailure is not null)
        {
            System.Runtime.ExceptionServices.ExceptionDispatchInfo
                .Capture(chunkFailure)
                .Throw();
        }
        if (cleanupFailure is not null)
        {
            System.Runtime.ExceptionServices.ExceptionDispatchInfo
                .Capture(cleanupFailure)
                .Throw();
        }
        cancellationToken.ThrowIfCancellationRequested();
        var completion = await OnEngineThread(() =>
        {
            ValidateCaptureLeaseRequest(request);
            return new CaptureLeaseCompletion(
                Interlocked.Read(ref _hierarchySequence),
                Interlocked.Read(ref _changeSequence));
        }, CancellationToken.None);

        EnsureCaptureLeaseEpochsUnchanged(
            capture.Envelope.HierarchySequenceBefore,
            capture.Envelope.ChangeSequenceBefore,
            completion.HierarchySequence,
            completion.ChangeSequence,
            "while the capture serializer was running");
        _captureDirtyPages.Stage(
            capture.PagePlan,
            completion.HierarchySequence,
            completion.ChangeSequence);
        cancellationToken.ThrowIfCancellationRequested();
        process.Refresh();
        var orderedChunkMilliseconds = chunkMilliseconds
            .Order()
            .ToArray();
        static double Percentile(double[] ordered, double percentile)
        {
            if (ordered.Length == 0)
            {
                return 0;
            }
            var index = Math.Clamp(
                (int)Math.Ceiling(ordered.Length * percentile) - 1,
                0,
                ordered.Length - 1);
            return ordered[index];
        }
        Logger.Info(
            "Capture phase telemetry: "
            + $"nodes={capture.Envelope.Nodes.Count}, "
            + $"chunks={capture.Chunks.Length}, "
            + $"reused-pages={reusedPageCount}, "
            + $"reused-payload={reusedPayloadBytes} bytes, "
            + $"node-budget={CaptureChunkPlanner.DefaultNodeBudget}, "
            + $"payload={serializedBytes} bytes, "
            + $"acquisition={acquisitionElapsed.TotalMilliseconds:F1}ms, "
            + $"planning={planningElapsed.TotalMilliseconds:F1}ms, "
            + $"launch={launchElapsed.TotalMilliseconds:F1}ms, "
            + $"settlement-and-restore={settlementElapsed.TotalMilliseconds:F1}ms, "
            + $"write-and-hash={writeElapsed.TotalMilliseconds:F1}ms, "
            + $"chunk-p50={Percentile(orderedChunkMilliseconds, 0.50):F1}ms, "
            + $"chunk-p95={Percentile(orderedChunkMilliseconds, 0.95):F1}ms, "
            + $"chunk-max={Percentile(orderedChunkMilliseconds, 1.00):F1}ms, "
            + $"total={captureStarted.Elapsed.TotalMilliseconds:F1}ms");
        Logger.Info(
            "Capture memory telemetry: "
            + $"managed={GC.GetTotalMemory(forceFullCollection: false) - managedBytesBefore:+#;-#;0} bytes, "
            + $"allocated={GC.GetTotalAllocatedBytes(precise: false) - allocatedBytesBefore} bytes, "
            + $"working-set={process.WorkingSet64 - workingSetBefore:+#;-#;0} bytes");
        return capture.Envelope with
        {
            HierarchySequenceAfter = completion.HierarchySequence,
            ChangeSequenceAfter = completion.ChangeSequence,
        };
    }

    private CaptureLeaseAcquisition AcquireCaptureLeaseSnapshot(CaptureLeaseRequest request)
    {
        ValidateCaptureLeaseRequest(request);
        var dataModel = _dataModel
            ?? throw new InvalidOperationException("edit DataModel is unavailable");
        Instance? editCamera = null;
        try
        {
            editCamera = dataModel.Workspace.CurrentCamera;
            RememberEditCamera(editCamera);
        }
        catch
        {
        }
        var hierarchySequence = Interlocked.Read(ref _hierarchySequence);
        var changeSequence = Interlocked.Read(ref _changeSequence);
        var excludedEditCameraHandle = editCamera is null
            ? 0
            : InstanceHierarchy.RuntimeHandle(editCamera);
        var nativePayload = InstanceHierarchy.Read(
            dataModel,
            editCamera,
            includeCaptureMetadata: true);
        var publicRootClasses = new Dictionary<nuint, CapturePublicRootClass>();
        foreach (var root in dataModel.GetChildren())
        {
            var handle = InstanceHierarchy.RuntimeHandle(root);
            CapturePublicRootClass publicClass;
            try
            {
                publicClass = new(
                    Reflection.GetProperty<string>(root, "ClassName"),
                    null);
            }
            catch (Exception error)
            {
                // The native hierarchy decides whether this root is persistent.
                // Defer an inaccessible public identity until the off-thread
                // planner knows that the root is actually in capture scope.
                publicClass = new(null, error);
            }
            if (!publicRootClasses.TryAdd(handle, publicClass))
            {
                throw new InvalidDataException("capture DataModel root handle is duplicated");
            }
        }
        var stagedManaged = CurrentManagedSourceContract();
        var mappedRootHandles = new Dictionary<string, nuint>(StringComparer.Ordinal);
        foreach (var sourceId in request.MappedRootSourceIds)
        {
            var binding = ResolveManagedSourceBinding(sourceId);
            if (!ManagedHierarchy.TryParseRuntimeIdentity(binding.DebugId, out var handle))
            {
                throw new InvalidOperationException(
                    $"mapped capture root {sourceId} runtime identity is unavailable");
            }
            mappedRootHandles.Add(sourceId, handle);
        }
        var mappedSourceIdsByHandle = new Dictionary<nuint, string>();
        lock (_managedHierarchyLock)
        {
            foreach (var binding in _managedByRuntime.Values)
            {
                if (ManagedHierarchy.TryParseRuntimeIdentity(binding.DebugId, out var handle)
                    && !mappedSourceIdsByHandle.TryAdd(handle, binding.SourceId))
                {
                    throw new InvalidDataException(
                        "managed capture runtime identity is duplicated");
                }
            }
        }
        Dictionary<nuint, LaunchHydratedServiceDefaults> launchHydratedRootDefaults;
        lock (_managedHierarchyLock)
        {
            launchHydratedRootDefaults = new(_launchHydratedRootDefaults);
        }
        return new(
            nativePayload,
            stagedManaged,
            mappedRootHandles,
            mappedSourceIdsByHandle,
            publicRootClasses,
            excludedEditCameraHandle,
            launchHydratedRootDefaults,
            hierarchySequence,
            changeSequence);
    }

    private async Task<CaptureLeaseEnginePlan> PlanCaptureLeaseSerializationAsync(
        CaptureLeaseRequest request,
        CaptureLeaseAcquisition acquisition,
        Action<CaptureLeasePhase> reportPhase,
        CancellationToken cancellationToken)
    {
        var nativePayload = ManagedHierarchy.ParseCaptureRuntimePayload(
            acquisition.NativePayload,
            cancellationToken);
        // The parsed arrays own every value needed below. Drop the dense wire
        // buffer before allocating planner indexes so large captures do not
        // retain both representations for the complete planning phase.
        acquisition = acquisition with { NativePayload = [] };
        var nativeHierarchy = nativePayload.Nodes;
        var hasPersistenceMetadata = false;
        for (var runtimeIndex = 1; runtimeIndex < nativeHierarchy.Length; runtimeIndex++)
        {
            if ((runtimeIndex & 0xfff) == 0)
            {
                cancellationToken.ThrowIfCancellationRequested();
            }
            hasPersistenceMetadata |= nativeHierarchy[runtimeIndex].PersistenceFlags != 0;
        }
        if (nativeHierarchy.Length < 2 || !hasPersistenceMetadata)
        {
            throw new InvalidDataException(
                "capture requires the RMLHIER5 native persistence flags");
        }

        var stagedManaged = acquisition.StagedManaged;
        var requestedSourceIndexes = new Dictionary<string, int>(StringComparer.Ordinal);
        foreach (var sourceId in request.MappedRootSourceIds)
        {
            if (!stagedManaged.IndexBySourceId.TryGetValue(sourceId, out var sourceIndex)
                || sourceIndex == 0)
            {
                throw new InvalidOperationException(
                    $"mapped capture root {sourceId} is absent from the staged managed contract");
            }
            requestedSourceIndexes.Add(sourceId, sourceIndex);
        }
        var requestedIndexSet = requestedSourceIndexes.Values.ToHashSet();
        var mappedAncestor = new int[stagedManaged.Source.Count];
        Array.Fill(mappedAncestor, -1);
        for (var sourceIndex = 1; sourceIndex < stagedManaged.Source.Count; sourceIndex++)
        {
            if ((sourceIndex & 0xfff) == 0)
            {
                cancellationToken.ThrowIfCancellationRequested();
            }
            var parentIndex = stagedManaged.Source[sourceIndex].ParentIndex;
            var ancestor = requestedIndexSet.Contains(parentIndex)
                ? parentIndex
                : mappedAncestor[parentIndex];
            if (requestedIndexSet.Contains(sourceIndex) && ancestor >= 0)
            {
                throw new InvalidOperationException(
                    $"mapped capture roots {stagedManaged.Source[ancestor].SourceId} and " +
                    $"{stagedManaged.Source[sourceIndex].SourceId} are nested barriers");
            }
            mappedAncestor[sourceIndex] = requestedIndexSet.Contains(sourceIndex)
                ? sourceIndex
                : ancestor;
        }

		var wantedRuntimeHandles = acquisition.MappedRootHandles.Values.ToHashSet();
		foreach (var reference in nativePayload.References)
		{
			if (reference.TargetHandle != 0)
			{
				wantedRuntimeHandles.Add(reference.TargetHandle);
			}
		}
		var runtimeIndexByHandle = new Dictionary<nuint, int>(wantedRuntimeHandles.Count);
		for (var runtimeIndex = 0; runtimeIndex < nativeHierarchy.Length; runtimeIndex++)
        {
            if ((runtimeIndex & 0xfff) == 0)
            {
                cancellationToken.ThrowIfCancellationRequested();
            }
            var runtime = nativeHierarchy[runtimeIndex];
			if (wantedRuntimeHandles.Contains(runtime.Handle)
				&& !runtimeIndexByHandle.TryAdd(runtime.Handle, runtimeIndex))
			{
				throw new InvalidDataException("capture requested native hierarchy handle is duplicated");
            }
            if (runtimeIndex == 0)
            {
                continue;
            }
            if (runtime.ParentIndex < 0 || runtime.ParentIndex >= runtimeIndex)
            {
                throw new InvalidDataException("capture full native hierarchy parent is invalid");
            }
        }

        var mappedSourceByRuntime = new Dictionary<int, string>();
        var mappedRootHandles = new List<nuint>(requestedSourceIndexes.Count);
        foreach (var sourceId in requestedSourceIndexes.Keys)
        {
            cancellationToken.ThrowIfCancellationRequested();
            if (!acquisition.MappedRootHandles.TryGetValue(sourceId, out var handle)
                || !runtimeIndexByHandle.TryGetValue(handle, out var runtimeIndex))
            {
                throw new InvalidOperationException(
                    $"mapped capture root {sourceId} is absent from the full native hierarchy");
            }
            mappedRootHandles.Add(handle);
            mappedSourceByRuntime.Add(runtimeIndex, sourceId);

            // A mapping root is an opaque capture barrier. Its runtime subtree
            // may contain temporary Studio drift; the complete subtree is
            // excluded by the propagation pass below and canonical filesystem
            // source is grafted back by Rust. Capture never verifies or walks a
            // source/runtime pair below this root.
        }
        var excludedMappedRuntime = new bool[nativeHierarchy.Length];
        for (var runtimeIndex = 1; runtimeIndex < nativeHierarchy.Length; runtimeIndex++)
        {
            if ((runtimeIndex & 0xfff) == 0)
            {
                cancellationToken.ThrowIfCancellationRequested();
            }
            excludedMappedRuntime[runtimeIndex] = mappedSourceByRuntime.ContainsKey(runtimeIndex)
                || excludedMappedRuntime[nativeHierarchy[runtimeIndex].ParentIndex];
        }
        var hasPersistentChildren = new bool[nativeHierarchy.Length];
        for (var runtimeIndex = 1; runtimeIndex < nativeHierarchy.Length; runtimeIndex++)
        {
            var runtime = nativeHierarchy[runtimeIndex];
            if (runtime.ParentIndex >= 0
                && HasCapturePersistence(runtime.PersistenceFlags, isServiceShell: false))
            {
                hasPersistentChildren[runtime.ParentIndex] = true;
            }
        }

        var nodes = new List<CaptureHierarchyNode>(nativeHierarchy.Length);
        var captureHandlesByOrdinal = new List<nuint>(nativeHierarchy.Length);
        var captureOrdinalByRuntime = new int[nativeHierarchy.Length];
        var serviceIndexByRuntime = new Dictionary<int, int>();
        Array.Fill(captureOrdinalByRuntime, -1);
        nodes.Add(new(
            CaptureEnvelope.NoParent,
            nativeHierarchy[0].ClassName,
            nativeHierarchy[0].Name,
            CaptureHierarchyFlags.ServiceShell));
        captureHandlesByOrdinal.Add(nativeHierarchy[0].Handle);
        captureOrdinalByRuntime[0] = 0;

        var serviceRuntimeIndexes = new List<int>();
        var serializedRootOrdinals = new List<uint>();
        var firstSerializedRoot = new List<uint>();
        var serializedRootCount = new List<uint>();
        for (var runtimeIndex = 1; runtimeIndex < nativeHierarchy.Length; runtimeIndex++)
        {
            if ((runtimeIndex & 0xfff) == 0)
            {
                cancellationToken.ThrowIfCancellationRequested();
            }
            var runtime = nativeHierarchy[runtimeIndex];
            var isServiceShell = runtime.ParentIndex == 0;
            if (excludedMappedRuntime[runtimeIndex]
                || runtime.ParentIndex < 0
                || captureOrdinalByRuntime[runtime.ParentIndex] < 0
                || !HasCapturePersistence(runtime.PersistenceFlags, isServiceShell))
            {
                continue;
            }
            if (runtime.ParentIndex == 0)
            {
                if (!acquisition.PublicRootClasses.TryGetValue(
                        runtime.Handle,
                        out var publicClass))
                {
                    throw new InvalidDataException(
                        "capture DataModel root is absent from the acquisition boundary");
                }
                if (publicClass.Error is not null)
                {
                    throw new InvalidDataException(
                        "capture DataModel root public class is unavailable",
                        publicClass.Error);
                }
                var publicClassName = publicClass.ClassName;
                if (ManagedHierarchy.IsInternalDataModelRoot(publicClassName))
                {
                    continue;
                }
                if (!string.Equals(publicClassName, runtime.ClassName, StringComparison.Ordinal))
                {
                    throw new InvalidDataException(
                        $"capture DataModel root class disagrees: native '{runtime.ClassName}', public '{publicClassName}'");
                }
            }
            var ordinal = checked((uint)nodes.Count);
            nodes.Add(new(
                checked((uint)captureOrdinalByRuntime[runtime.ParentIndex]),
                runtime.ClassName,
                runtime.Name,
                isServiceShell
                    ? CaptureHierarchyFlags.ServiceShell
                    : CaptureHierarchyFlags.Serialized));
            captureHandlesByOrdinal.Add(runtime.Handle);
            captureOrdinalByRuntime[runtimeIndex] = checked((int)ordinal);
            if (isServiceShell)
            {
                serviceIndexByRuntime.Add(runtimeIndex, serviceRuntimeIndexes.Count);
                serviceRuntimeIndexes.Add(runtimeIndex);
                firstSerializedRoot.Add(uint.MaxValue);
                serializedRootCount.Add(0);
                continue;
            }

            if (nativeHierarchy[runtime.ParentIndex].ParentIndex != 0)
            {
                continue;
            }
            if (!serviceIndexByRuntime.TryGetValue(runtime.ParentIndex, out var serviceIndex))
            {
                throw new InvalidDataException("capture serialized root has no service shell");
            }
            if (firstSerializedRoot[serviceIndex] == uint.MaxValue)
            {
                firstSerializedRoot[serviceIndex] = checked((uint)serializedRootOrdinals.Count);
            }
            serializedRootCount[serviceIndex]++;
            serializedRootOrdinals.Add(ordinal);
        }

		var contentObjectBlockers = nativePayload.ContentObjects
			.Where(blocker => captureOrdinalByRuntime[blocker.OwnerIndex] >= 0)
			.ToArray();
		if (contentObjectBlockers.Length != 0)
		{
			const int diagnosticLimit = 16;
			var details = contentObjectBlockers
				.Take(diagnosticLimit)
				.Select(blocker =>
					$"{CaptureStudioPath(nativeHierarchy, blocker.OwnerIndex)}.{blocker.Property}")
				.ToList();
			if (contentObjectBlockers.Length > diagnosticLimit)
			{
				details.Add($"and {contentObjectBlockers.Length - diagnosticLimit} more");
			}
			throw new InvalidOperationException(
				"Capture Manifest cannot serialize Content.Object losslessly. " +
				"Replace each value with Content.none or Content.fromUri(...). Blockers: " +
				string.Join("; ", details));
		}

        var serviceRoots = new List<CaptureServiceRoot>(serviceRuntimeIndexes.Count);
        for (var serviceIndex = 0; serviceIndex < serviceRuntimeIndexes.Count; serviceIndex++)
        {
            if ((serviceIndex & 0xfff) == 0)
            {
                cancellationToken.ThrowIfCancellationRequested();
            }
            var runtimeIndex = serviceRuntimeIndexes[serviceIndex];
            var runtime = nativeHierarchy[runtimeIndex];
            serviceRoots.Add(new(
                checked((uint)captureOrdinalByRuntime[runtimeIndex]),
                runtime.ClassName,
                runtime.Name,
                firstSerializedRoot[serviceIndex] == uint.MaxValue
                    ? checked((uint)serializedRootOrdinals.Count)
                    : firstSerializedRoot[serviceIndex],
                serializedRootCount[serviceIndex]));
        }

        var directSerializedRootOrdinals = serializedRootOrdinals.ToArray();
        // Roblox drops a JointInstance when an endpoint is outside the serializer
        // input. The sideband owns the final reference values, so add each remote
        // endpoint only as an isolated serializer dependency.
        var referenceDependencyTargets = new Dictionary<(uint Owner, string Endpoint), uint>();
        foreach (var reference in nativePayload.References)
        {
            var endpoint = string.Equals(
                reference.Property,
                "Part0",
                StringComparison.OrdinalIgnoreCase)
                    ? "Part0"
                    : string.Equals(
                        reference.Property,
                        "Part1",
                        StringComparison.OrdinalIgnoreCase)
                            ? "Part1"
                            : null;
            if (endpoint is null)
            {
                continue;
            }
            var ownerOrdinalValue = captureOrdinalByRuntime[reference.OwnerIndex];
            if (ownerOrdinalValue <= 0
                || reference.TargetHandle == 0
                || !runtimeIndexByHandle.TryGetValue(reference.TargetHandle, out var targetRuntimeIndex))
            {
                continue;
            }
            var targetOrdinalValue = captureOrdinalByRuntime[targetRuntimeIndex];
            if (targetOrdinalValue <= 0)
            {
                continue;
            }
            referenceDependencyTargets.TryAdd(
                (checked((uint)ownerOrdinalValue), endpoint),
                checked((uint)targetOrdinalValue));
        }
        var referenceDependencies = referenceDependencyTargets
            .Select(pair => new CaptureReferenceDependency(pair.Key.Owner, pair.Value))
            .ToArray();
        var persistentChunkLayouts = CaptureChunkPlanner.Plan(
            nodes,
            directSerializedRootOrdinals,
            referenceDependencies: referenceDependencies);
        var dependencyRuntimeIndexes = persistentChunkLayouts
            .SelectMany(layout => layout.DependencyOrdinals)
            .Distinct()
            .ToDictionary(
                ordinal => runtimeIndexByHandle[captureHandlesByOrdinal[checked((int)ordinal)]],
                ordinal => ordinal);
        var dependencyChildHandles = dependencyRuntimeIndexes.Values
            .ToDictionary(ordinal => ordinal, _ => new List<nuint>());
        for (var runtimeIndex = 1; runtimeIndex < nativeHierarchy.Length; runtimeIndex++)
        {
            if (dependencyRuntimeIndexes.TryGetValue(
                    nativeHierarchy[runtimeIndex].ParentIndex,
                    out var dependencyOrdinal))
            {
                dependencyChildHandles[dependencyOrdinal].Add(
                    nativeHierarchy[runtimeIndex].Handle);
            }
        }
        var persistentChunks = persistentChunkLayouts
            .Select(layout =>
            {
                var rootHandles = layout.RootOrdinals
                    .Select(ordinal => captureHandlesByOrdinal[checked((int)ordinal)])
                    .ToArray();
                var frontierHandles = layout.FrontierOrdinals
                    .Select(ordinal => captureHandlesByOrdinal[checked((int)ordinal)])
                    .ToArray();
                var memberHandles = layout.MemberOrdinals
                    .Select(ordinal => captureHandlesByOrdinal[checked((int)ordinal)])
                    .ToArray();
                var dependencyRootHandles = layout.DependencyOrdinals
                    .Select(ordinal => captureHandlesByOrdinal[checked((int)ordinal)])
                    .ToArray();
                var maskedDependencyChildHandles = layout.DependencyOrdinals
                    .SelectMany(ordinal => dependencyChildHandles[ordinal])
                    .Distinct()
                    .ToArray();
                if (rootHandles.Concat(dependencyRootHandles)
                    .Intersect(maskedDependencyChildHandles)
                    .Any())
                {
                    throw new InvalidDataException(
                        "capture reference dependency cannot be isolated from another serializer root");
                }
                return new CapturePlannedChunk(
                    layout.RootOrdinals,
                    rootHandles,
                    frontierHandles,
                    memberHandles,
                    layout.DependencyOrdinals,
                    dependencyRootHandles,
                    maskedDependencyChildHandles,
                    CaptureDirtyPageTable.ComputePageId(
                        rootHandles,
                        frontierHandles,
                        memberHandles,
                        dependencyRootHandles,
                        maskedDependencyChildHandles));
            })
            .ToArray();
        serializedRootOrdinals.Clear();
        foreach (var chunk in persistentChunks)
        {
            serializedRootOrdinals.AddRange(chunk.RootOrdinals);
        }
        if (!serializedRootOrdinals
            .Take(directSerializedRootOrdinals.Length)
            .SequenceEqual(directSerializedRootOrdinals))
        {
            throw new InvalidDataException(
                "capture chunk plan changed direct service-root order");
        }

        var plannedMappedBindings = new Dictionary<string, CaptureMappedBinding>(StringComparer.Ordinal);
        if (requestedSourceIndexes.Count != 0)
        {
            for (var runtimeIndex = 1; runtimeIndex < nativeHierarchy.Length; runtimeIndex++)
            {
                if ((runtimeIndex & 0xfff) == 0)
                {
                    cancellationToken.ThrowIfCancellationRequested();
                }
                var parentRuntimeIndex = nativeHierarchy[runtimeIndex].ParentIndex;
                if (mappedSourceByRuntime.TryGetValue(runtimeIndex, out var sourceId))
                {
                    var parentOrdinalValue = captureOrdinalByRuntime[parentRuntimeIndex];
                    if (parentOrdinalValue < 0)
                    {
                        throw new InvalidOperationException(
                            $"mapped capture root {sourceId} has no persistent graft parent");
                    }
                    plannedMappedBindings.Add(sourceId, new(
                        sourceId,
                        CaptureEnvelope.SyntheticNode,
                        checked((uint)parentOrdinalValue)));
                    continue;
                }
            }
        }

        var shellSchema = request.ShellClasses.ToDictionary(
            shell => shell.ClassName,
            shell => shell.Properties,
            StringComparer.Ordinal);
        var shellRuntimeIndexes = serviceRuntimeIndexes.Prepend(0).ToArray();
        var capturedShellClasses = shellRuntimeIndexes
            .Select(index => nativeHierarchy[index].ClassName)
            .ToHashSet(StringComparer.Ordinal);
        CaptureLeaseManager.EnsureShellSchemaCoverage(capturedShellClasses, shellSchema.Keys);

        var mappedSourceIdsByHandle = new Dictionary<nuint, string>(
            acquisition.MappedSourceIdsByHandle);
        var unboundMappedReferenceHandles = SelectUnboundCaptureReferenceHandles(
            nativePayload.References
                .Where(reference =>
                    reference.TargetHandle != 0
                    && runtimeIndexByHandle.TryGetValue(
                        reference.TargetHandle,
                        out var targetRuntimeIndex)
                    && excludedMappedRuntime[targetRuntimeIndex])
                .Select(reference => reference.TargetHandle),
            mappedSourceIdsByHandle);
        if (unboundMappedReferenceHandles.Length != 0)
        {
            var resolved = await OnEngineThread(
                () => ResolveCaptureMappedReferenceSourceIds(
                    unboundMappedReferenceHandles),
                cancellationToken);
            foreach (var (handle, sourceId) in resolved)
            {
                mappedSourceIdsByHandle.Add(handle, sourceId);
            }
        }

        var externalReferences = new List<CaptureExternalReference>();
        var referenceIndex = 0;
        foreach (var reference in nativePayload.References)
        {
            if ((referenceIndex++ & 0xfff) == 0)
            {
                cancellationToken.ThrowIfCancellationRequested();
            }
            var ownerIsMapped = excludedMappedRuntime[reference.OwnerIndex];
			var ownerOrdinalValue = captureOrdinalByRuntime[reference.OwnerIndex];
			var ownerIsManifest = ownerOrdinalValue >= 0;
            var targetRuntimeIndex = -1;
            var targetIsInHierarchy = reference.TargetHandle != 0
                && runtimeIndexByHandle.TryGetValue(
                    reference.TargetHandle,
                    out targetRuntimeIndex);
			var targetIsMapped = targetIsInHierarchy
				&& excludedMappedRuntime[targetRuntimeIndex];
			var targetIsManifest = targetIsInHierarchy
				&& captureOrdinalByRuntime[targetRuntimeIndex] >= 0;
			if (CrossesCaptureOwnershipBarrier(
				ownerIsMapped,
				ownerIsManifest,
				targetIsMapped,
				targetIsManifest))
            {
                throw new InvalidOperationException(
                    "cross-domain reference blocker: " +
                    $"{CaptureStudioPath(nativeHierarchy, reference.OwnerIndex)}.{reference.Property} " +
                    $"targets {CaptureStudioPath(nativeHierarchy, targetRuntimeIndex)} across the " +
                    "Studio/filesystem ownership barrier");
            }
            if (ownerOrdinalValue < 0)
            {
                continue;
            }
            var ownerOrdinal = checked((uint)ownerOrdinalValue);
            var ownerClass = nativeHierarchy[reference.OwnerIndex].ClassName;
            var ownerIsShell = (nodes[checked((int)ownerOrdinal)].Flags
                & CaptureHierarchyFlags.ServiceShell) != 0;
            if (ownerIsShell
                && !IsRequestedCaptureShellProperty(ownerClass, reference.Property, shellSchema))
            {
                // Native reflection may expose internal XML-readable service
                // references that are intentionally absent from Carbon's pinned
                // shell schema. They are not part of the capture contract.
                continue;
            }
            uint targetOrdinal;
            string? mappedSourceId = null;
            if (reference.TargetHandle == 0)
            {
                targetOrdinal = CaptureEnvelope.NullReference;
            }
            else if (targetIsMapped)
            {
                if (!mappedSourceIdsByHandle.TryGetValue(
                        reference.TargetHandle,
                        out mappedSourceId))
                {
                    throw new InvalidOperationException(
                        "mapped reference target has no verified source identity: " +
                        $"{CaptureStudioPath(nativeHierarchy, reference.OwnerIndex)}.{reference.Property} " +
                        $"targets {CaptureStudioPath(nativeHierarchy, targetRuntimeIndex)}");
                }
                targetOrdinal = CaptureEnvelope.MappedReference;
            }
            else if (!runtimeIndexByHandle.TryGetValue(
                reference.TargetHandle,
                out var capturedTargetRuntimeIndex)
                || captureOrdinalByRuntime[capturedTargetRuntimeIndex] < 0)
            {
                if (IsExcludedEditCameraReference(
                    ownerClass,
                    reference.Property,
                    reference.TargetHandle,
                    acquisition.ExcludedEditCameraHandle))
                {
                    targetOrdinal = CaptureEnvelope.NullReference;
                }
                else
                {
                    throw new InvalidOperationException(
                        $"persistent reference {CaptureStudioPath(nativeHierarchy, reference.OwnerIndex)}." +
                        $"{reference.Property} targets a non-persistent or external instance");
                }
            }
            else
            {
                targetOrdinal = checked((uint)captureOrdinalByRuntime[capturedTargetRuntimeIndex]);
            }
            if (ShouldIncludeCaptureExternalReference(
                ownerIsShell,
                ownerClass,
                reference.Property,
                shellSchema))
            {
                externalReferences.Add(new(
                    ownerOrdinal,
                    reference.Property,
                    targetOrdinal,
                    mappedSourceId));
            }
        }

        var mappedBindings = SelectCaptureMappedBindings(
            request.MappedRootSourceIds,
            plannedMappedBindings);
        IReadOnlyList<ManifestIdentity> manifestIdentities;
        lock (_manifestIdentityLock)
        {
            if (_manifestIdentities.IsAuthoritative != request.ManifestIdentitiesAuthoritative)
            {
                throw new InvalidOperationException(
                    "capture manifest identity mode disagrees with the native ledger");
            }
            manifestIdentities = _manifestIdentities.Snapshot(captureHandlesByOrdinal);
        }
        var pagePlan = _captureDirtyPages.Plan(
            request.CaptureId,
            new(
                request.EngineGeneration,
                request.StudioSessionId,
                request.InstanceId,
                request.ManagedContractId,
                request.ReflectionSchemaHash,
                CaptureDirtyPageTable.ComputeMappingFingerprint(
                    request.MappedRootSourceIds),
                request.ManifestIdentitiesAuthoritative),
            acquisition.HierarchySequence,
            acquisition.ChangeSequence,
            persistentChunks
                .Select(chunk => new CapturePageDefinition(
                    chunk.PageId,
                    chunk.MemberHandles))
                .ToArray(),
            request.AllowPageReuse);
        cancellationToken.ThrowIfCancellationRequested();
        reportPhase(CaptureLeasePhase.Serializing);
        return await OnEngineThreadUninterruptibleOnceStarted(() =>
        {
            ValidateCaptureLeaseRequest(request);
            EnsureCaptureLeaseEpochsUnchanged(
                acquisition.HierarchySequence,
                acquisition.ChangeSequence,
                Interlocked.Read(ref _hierarchySequence),
                Interlocked.Read(ref _changeSequence),
                "between the native hierarchy read and serializer launch");
            var mappedInstances = mappedRootHandles
                .Select(handle => Instance.FromHandle(handle)
                    ?? throw new InvalidOperationException(
                        "mapped capture root runtime handle is unavailable"))
                .ToArray();
            var engineChunks = persistentChunks
                .Select((chunk, index) => new CaptureSerializationChunk(
                    chunk.RootOrdinals
                        .Concat(chunk.DependencyOrdinals.Select(
                            CaptureModelArtifact.EncodeReferenceDependency))
                        .ToArray(),
                    chunk.RootHandles.Concat(chunk.DependencyRootHandles).ToArray(),
                    chunk.FrontierHandles.Concat(chunk.MaskedDependencyChildHandles).ToArray(),
                    index,
                    pagePlan.Pages[index].ReusedPayload))
                .ToList();

            var shellProperties = new List<CaptureShellProperty>();
            var shellCarriers = new List<CaptureShellCarrier>();
            var temporaryRoots = new List<Instance>();
            var carrierByOwnerAndClass = new Dictionary<(uint Owner, string Class), (Instance Carrier, uint RootIndex)>();
            try
            {
                for (var serviceIndex = 0; serviceIndex < serviceRuntimeIndexes.Count; serviceIndex++)
                {
                    var runtimeIndex = serviceRuntimeIndexes[serviceIndex];
                    var runtime = nativeHierarchy[runtimeIndex];
                    var ownerOrdinal = checked((uint)captureOrdinalByRuntime[runtimeIndex]);
                    var instance = Instance.FromHandle(runtime.Handle)
                        ?? throw new InvalidDataException("capture service shell handle is unavailable");
                    var hasLaunchDefaults = acquisition.LaunchHydratedRootDefaults.TryGetValue(
                        runtime.Handle,
                        out var launchDefaults);
                    var matchesLaunchDefaults = hasLaunchDefaults && CaptureServiceMatchesLaunchDefaults(
                        instance,
                        runtime.ClassName,
                        shellSchema[runtime.ClassName],
                        launchDefaults!);
                    if (ShouldOmitDefaultHydratedService(
                            hasLaunchDefaults,
                            hasPersistentChildren[runtimeIndex],
                            matchesLaunchDefaults))
                    {
                        nodes[checked((int)ownerOrdinal)] = nodes[checked((int)ownerOrdinal)] with
                        {
                            Flags = nodes[checked((int)ownerOrdinal)].Flags
                                | CaptureHierarchyFlags.DefaultHydratedService,
                        };
                    }
                }
                foreach (var runtimeIndex in shellRuntimeIndexes)
                {
                    var runtime = nativeHierarchy[runtimeIndex];
                    var ownerOrdinal = checked((uint)captureOrdinalByRuntime[runtimeIndex]);
                    var instance = Instance.FromHandle(runtime.Handle)
                        ?? throw new InvalidDataException("capture service shell handle is unavailable");
                    foreach (var property in shellSchema[runtime.ClassName])
                    {
                        var descriptor = SerializedPropertyAccess.Describe(instance, property)
                            ?? throw new InvalidOperationException(
                                $"capture shell descriptor {runtime.ClassName}.{property} is unavailable");
                        if (descriptor.IsReference)
                        {
                            if (!externalReferences.Any(reference =>
                                reference.OwnerOrdinal == ownerOrdinal
                                && string.Equals(reference.Property, property, StringComparison.Ordinal)))
                            {
                                throw new InvalidOperationException(
                                    $"capture shell reference {runtime.ClassName}.{property} is absent from the native sideband");
                            }
                            continue;
                        }
                        if (!CanReadForCapture(descriptor))
                        {
                            throw new InvalidOperationException(
                                $"capture shell property {runtime.ClassName}.{property} is outside the exact property policy");
                        }
                        if (!UsesSerializedPropertyCarrier(descriptor))
                        {
                            shellProperties.Add(new(
                                ownerOrdinal,
                                property,
                                descriptor.TypeName,
                                SerializedPropertyAccess.Read(instance, property)));
                            continue;
                        }

                        var carrierClass = SerializedPropertyCarrierClass(runtime.ClassName, descriptor);
                        var key = (ownerOrdinal, carrierClass);
                        if (!carrierByOwnerAndClass.TryGetValue(key, out var carrierEntry))
                        {
                            var wrapperHandle = Reflection.CreateInstance("Folder", CreatorRole.Engine);
                            var wrapper = Instance.FromHandle(wrapperHandle)
                                ?? throw new InvalidOperationException("engine could not create a capture shell wrapper");
                            temporaryRoots.Add(wrapper);
                            wrapper.Name = $"__CarbonCaptureShell:{ownerOrdinal}:{carrierClass}";
                            var carrierHandle = Reflection.CreateInstance(carrierClass, CreatorRole.Engine);
                            var carrier = Instance.FromHandle(carrierHandle)
                                ?? throw new InvalidOperationException(
                                    $"engine could not create capture shell carrier class {carrierClass}");
                            carrier.Name = runtime.Name;
                            carrier.Parent = wrapper;
                            var serializedRootIndex = checked((uint)serializedRootOrdinals.Count);
                            serializedRootOrdinals.Add(CaptureEnvelope.SyntheticNode);
                            carrierEntry = (carrier, serializedRootIndex);
                            carrierByOwnerAndClass.Add(key, carrierEntry);
                        }
                        if (!SerializedPropertyAccess.Copy(instance, carrierEntry.Carrier, property))
                        {
                            throw new InvalidOperationException(
                                $"engine rejected capture shell carrier copy for {runtime.ClassName}.{property}");
                        }
                        shellCarriers.Add(new(
                            ownerOrdinal,
                            property,
                            descriptor.TypeName,
                            carrierClass,
                            carrierEntry.RootIndex));
                    }
                }
            }
            catch (Exception planningFailure)
            {
                try
                {
                    DestroyCaptureTemporaryRoots(temporaryRoots);
                }
                catch (Exception cleanupFailure)
                {
                    throw new AggregateException(
                        "capture carrier planning and cleanup both failed",
                        planningFailure,
                        cleanupFailure);
                }
                System.Runtime.ExceptionServices.ExceptionDispatchInfo
                    .Capture(planningFailure)
                    .Throw();
                throw new UnreachableException();
            }

            var carrierRootsPerChunk = checked((int)Math.Max(
                1,
                CaptureChunkPlanner.DefaultNodeBudget / 2));
            for (var start = 0; start < temporaryRoots.Count; start += carrierRootsPerChunk)
            {
                var count = Math.Min(carrierRootsPerChunk, temporaryRoots.Count - start);
                engineChunks.Add(new(
                    Enumerable.Repeat(CaptureEnvelope.SyntheticNode, count).ToArray(),
                    temporaryRoots
                        .GetRange(start, count)
                        .Select(InstanceHierarchy.RuntimeHandle)
                        .ToArray(),
                    [],
                    -1,
                    null));
            }
            return new CaptureLeaseEnginePlan(
                new CaptureEnvelopeData(
                    request.CaptureId,
                    request.EngineGeneration,
                    request.SourceGeneration,
                    acquisition.HierarchySequence,
                    acquisition.HierarchySequence,
                    acquisition.ChangeSequence,
                    acquisition.ChangeSequence,
                    request.StudioSessionId,
                    request.InstanceId,
                    request.ManagedContractId,
                    request.ReflectionSchemaHash,
                    nodes,
                    serviceRoots,
                    mappedBindings,
                    externalReferences,
                    shellProperties,
                    shellCarriers,
                    serializedRootOrdinals,
                    request.ManifestIdentitiesAuthoritative,
                    manifestIdentities),
                engineChunks.ToArray(),
                pagePlan,
                mappedInstances,
                temporaryRoots.ToArray());
        }, cancellationToken);
    }

    private Dictionary<nuint, string> ResolveCaptureMappedReferenceSourceIds(
        IReadOnlyList<nuint> handles)
    {
        var resolved = new Dictionary<nuint, string>(handles.Count);
        foreach (var handle in handles)
        {
            var instance = Instance.FromHandle(handle);
            if (instance is null)
            {
                continue;
            }
            if (TryResolveManagedBinding(instance, out var binding)
                || TryResolveDisplacedManagedBinding(instance, out binding))
            {
                resolved.Add(handle, binding.SourceId);
            }
        }
        return resolved;
    }

    internal static bool HasCapturePersistence(byte persistenceFlags, bool isServiceShell)
    {
        // DataModel services are routing anchors. Several legitimate services,
        // including StarterGui, are intentionally non-Archivable even though their
        // persistent children are valid serialization roots. The shell itself only
        // needs engine serializability; every captured descendant still requires
        // the full Serializable + Archivable contract.
        var required = isServiceShell
            ? ManagedHierarchy.RuntimeSerializable
            : ManagedHierarchy.RuntimePersistent;
        return (persistenceFlags & required) == required;
    }

    internal static bool ShouldOmitDefaultHydratedService(
        bool launchHydrated,
        bool hasPersistentChildren,
        bool matchesCurrentDefaults) =>
        launchHydrated && !hasPersistentChildren && matchesCurrentDefaults;

    internal static void ReconcileLaunchHydratedRootDefaults<T>(
        Dictionary<nuint, T> launchDefaults,
        IReadOnlyDictionary<nuint, T> currentlyHydrated)
    {
        foreach (var (handle, defaults) in currentlyHydrated)
        {
            launchDefaults.TryAdd(handle, defaults);
        }
    }

    internal static void RefreshPendingLaunchHydratedRootDefaults<T>(
        Dictionary<nuint, T> launchDefaults,
        IReadOnlyDictionary<nuint, T> currentValues,
        IReadOnlySet<nuint> pendingHandles)
    {
        foreach (var handle in pendingHandles)
        {
            launchDefaults.Remove(handle);
            if (currentValues.TryGetValue(handle, out var current))
            {
                launchDefaults.Add(handle, current);
            }
        }
    }

    private static bool CaptureServiceMatchesLaunchDefaults(
        Instance service,
        string className,
        IReadOnlyList<string> properties,
        LaunchHydratedServiceDefaults defaults)
    {
        try
        {
            if (!string.Equals(className, defaults.ClassName, StringComparison.Ordinal))
            {
                return false;
            }
            if (!string.Equals(service.Name, defaults.Name, StringComparison.Ordinal))
            {
                return false;
            }
            foreach (var property in properties)
            {
                var liveDescriptor = SerializedPropertyAccess.Describe(service, property);
                if (liveDescriptor is null)
                {
                    return false;
                }
                if (!defaults.Properties.TryGetValue(property, out var baseline))
                {
                    return false;
                }
                if (liveDescriptor.Value.TypeName != baseline.Descriptor.TypeName
                    || liveDescriptor.Value.Attributes != baseline.Descriptor.Attributes)
                {
                    return false;
                }
                if (liveDescriptor.Value.IsReference)
                {
                    var liveTarget = Reflection.GetProperty<Instance>(service, property);
                    var liveHandle = liveTarget is null ? 0 : InstanceHierarchy.RuntimeHandle(liveTarget);
                    if (liveHandle != baseline.ReferenceHandle)
                    {
                        return false;
                    }
                    continue;
                }
                if (!CanReadForCapture(liveDescriptor.Value)
                    || !SerializedPropertyAccess.Read(service, property)
                        .AsSpan()
                        .SequenceEqual(baseline.Value))
                {
                    return false;
                }
            }
            return true;
        }
        catch
        {
            // Unknown or newly restricted properties must retain the service. A
            // false negative costs manifest space; a false positive loses state.
            return false;
        }
    }

    private static void DestroyCaptureTemporaryRoots(IEnumerable<Instance> roots)
    {
        List<Exception>? failures = null;
        foreach (var root in roots)
        {
            try
            {
                root.Destroy();
            }
            catch (Exception ex)
            {
                (failures ??= []).Add(ex);
            }
        }
        if (failures is not null)
        {
            throw new AggregateException(
                $"capture could not destroy {failures.Count} temporary carrier root(s)",
                failures);
        }
    }

	private static string CaptureStudioPath(
		IReadOnlyList<CaptureRuntimeNode> hierarchy,
		int ownerIndex)
	{
		var segments = new Stack<string>();
		for (var index = ownerIndex; index > 0; index = hierarchy[index].ParentIndex)
		{
			var name = hierarchy[index].Name;
			var simple = name.Length != 0
				&& (char.IsAsciiLetter(name[0]) || name[0] == '_')
				&& name.Skip(1).All(character => char.IsAsciiLetterOrDigit(character) || character == '_');
			segments.Push(simple
				? $".{name}"
				: $"[{JsonSerializer.Serialize(name)}]");
		}
		return "game" + string.Concat(segments);
	}

    private async Task<RootModelResponse> SerializeRootModelAsync(
        RootModelRequest request,
        CancellationToken cancellationToken)
    {
        for (var attempt = 0; attempt < 3; attempt++)
        {
            var capture = await OnEngineThread(() =>
            {
                var dataModel = _dataModel ?? throw new InvalidOperationException("edit DataModel is unavailable");
                var hierarchySequence = Interlocked.Read(ref _hierarchySequence);
                var changeSequence = Interlocked.Read(ref _changeSequence);
                var allRoots = GetSerializableRoots(dataModel);
                var serializationRoots = new List<Instance>();
                var modelRootParentDebugIds = new List<string>();
                foreach (var root in allRoots)
                {
                    foreach (var child in root.Instance.GetChildren())
                    {
                        if (!child.Archivable || !Reflection.IsSerializable(child))
                        {
                            continue;
                        }
                        serializationRoots.Add(child);
                        modelRootParentDebugIds.Add(root.Identity.DebugId);
                    }
                }
                var instanceDebugIds = new List<string>();
                var pending = new Queue<Instance>(serializationRoots);
                while (pending.TryDequeue(out var instance))
                {
                    instanceDebugIds.Add(instance.GetDebugId(128));
                    foreach (var child in instance.GetChildren())
                    {
                        if (child.Archivable && Reflection.IsSerializable(child))
                        {
                            pending.Enqueue(child);
                        }
                    }
                }
                foreach (var debugId in request.DebugIds.ToHashSet(StringComparer.Ordinal))
                {
                    _ = Resolve(debugId);
                }
                var temporaryRoots = new List<Instance>();
                try
                {
                    var rootPropertyCarriers = new Dictionary<string, string>(StringComparer.Ordinal);
                    var rootPropertyCarrierInstanceDebugIds = new Dictionary<string, string[]>(StringComparer.Ordinal);
                    foreach (var debugId in request.DebugIds.Distinct(StringComparer.Ordinal))
                    {
                        var root = Resolve(debugId);
                        var clone = root.Clone()
                            ?? throw new InvalidOperationException("engine could not clone a hidden root property carrier");
                        temporaryRoots.Add(clone);
                        var carrierInstanceDebugIds = new List<string>();
                        var pairs = new Queue<(Instance Original, Instance Clone)>();
                        pairs.Enqueue((root, clone));
                        while (pairs.TryDequeue(out var pair))
                        {
                            if (pair.Original.ClassName != pair.Clone.ClassName || pair.Original.Name != pair.Clone.Name)
                            {
                                throw new InvalidOperationException("hidden root property carrier changed clone identity");
                            }
                            carrierInstanceDebugIds.Add(pair.Original.GetDebugId(128));
                            var originalChildren = pair.Original.GetChildren()
                                .Where(child => child.Archivable)
                                .ToArray();
                            var cloneChildren = pair.Clone.GetChildren();
                            if (originalChildren.Length != cloneChildren.Count)
                            {
                                throw new InvalidOperationException("hidden root property carrier changed clone hierarchy");
                            }
                            for (var index = 0; index < originalChildren.Length; index++)
                            {
                                if (Reflection.IsSerializable(originalChildren[index]))
                                {
                                    pairs.Enqueue((originalChildren[index], cloneChildren[index]));
                                }
                                else
                                {
                                    cloneChildren[index].Destroy();
                                }
                            }
                        }
                        var wrapperHandle = Reflection.CreateInstance("Folder", CreatorRole.Engine);
                        var wrapper = Instance.FromHandle(wrapperHandle)
                            ?? throw new InvalidOperationException("engine could not create a hidden root property wrapper");
                        temporaryRoots.Add(wrapper);
                        wrapper.Name = RootPropertyWrapperPrefix + Guid.NewGuid().ToString("N");
                        rootPropertyCarriers.Add(debugId, wrapper.Name);
                        rootPropertyCarrierInstanceDebugIds.Add(debugId, carrierInstanceDebugIds.ToArray());
                        clone.Parent = wrapper;
                        temporaryRoots.Remove(clone);
                        serializationRoots.Add(wrapper);
                    }
                    var service = dataModel.GetService<SerializationService>();
                    // SerializationService rejects DataModel services themselves.
                    // Serialize every persistent child in one model so cross-root
                    // references remain intact; the server restores the service
                    // wrappers from their canonical source properties.
                    IReadOnlyList<Instance> serializationInput = serializationRoots;
                    var serialization = Reflection.InvokeAsync<byte[]>(
                        service,
                        "SerializeInstancesAsync",
                        serializationInput);
                    return new RootModelCapture(
                        serialization,
                        allRoots.Select(root => root.Identity).ToArray(),
                        modelRootParentDebugIds.ToArray(),
                        instanceDebugIds.ToArray(),
                        rootPropertyCarriers,
                        rootPropertyCarrierInstanceDebugIds,
                        hierarchySequence,
                        changeSequence,
                        temporaryRoots.ToArray());
                }
                catch
                {
                    foreach (var root in temporaryRoots)
                    {
                        root.Destroy();
                    }
                    throw;
                }
            }, cancellationToken);
            byte[] model;
            try
            {
                model = await capture.Serialization.WaitAsync(TimeSpan.FromSeconds(30), cancellationToken)
                    ?? throw new InvalidOperationException("engine returned no serialized root model");
            }
            finally
            {
                await OnEngineThread(() =>
                {
                    foreach (var root in capture.TemporaryRoots)
                    {
                        root.Destroy();
                    }
                    return (object?)null;
                }, CancellationToken.None);
            }
            if (capture.HierarchySequence == Interlocked.Read(ref _hierarchySequence))
            {
                return new RootModelResponse(
                    Convert.ToBase64String(model),
                    capture.Roots,
                    capture.ModelRootParentDebugIds,
                    capture.InstanceDebugIds,
                    capture.RootPropertyCarriers,
                    capture.RootPropertyCarrierInstanceDebugIds,
                    capture.ChangeSequence);
            }
        }

        throw new InvalidOperationException("edit DataModel hierarchy changed during three root snapshot attempts");
    }

    private async Task<RootApplyModelResponse> ApplyRootModelAsync(
        RootApplyModelRequest request,
        CancellationToken cancellationToken)
    {
        if (request.SourceIds.Length == 0 || request.SourceIds.Distinct(StringComparer.Ordinal).Count() != request.SourceIds.Length)
        {
            throw new InvalidOperationException("hidden-root apply requires unique source identities");
        }
        var bytes = Convert.FromBase64String(request.Model);
        var deserialization = await OnEngineThread(() =>
        {
            var dataModel = _dataModel ?? throw new InvalidOperationException("edit DataModel is unavailable");
            return dataModel.GetService<SerializationService>().DeserializeInstancesAsync(bytes);
        }, cancellationToken);
        var roots = await deserialization.WaitAsync(TimeSpan.FromSeconds(30), cancellationToken)
            ?? throw new InvalidOperationException("engine returned no hidden-root apply model");
        try
        {
            return await OnEngineThread(() =>
            {
                if (roots.Count != 1)
                {
                    throw new InvalidOperationException("hidden-root apply model must contain exactly one root");
                }
                var sourceRoot = roots[0];
                var targetRoot = Resolve(request.DebugId);
                if (sourceRoot.ClassName != targetRoot.ClassName)
                {
                    throw new InvalidOperationException("hidden-root apply model changed the native root class");
                }

                var sourceInstances = Preorder(sourceRoot);
                if (sourceInstances.Count != request.SourceIds.Length)
                {
                    throw new InvalidOperationException("hidden-root apply source identity count does not match its model");
                }
                var bySourceId = new Dictionary<string, Instance>(StringComparer.Ordinal)
                {
                    [request.SourceIds[0]] = targetRoot,
                };
                for (var index = 1; index < sourceInstances.Count; index++)
                {
                    bySourceId.Add(request.SourceIds[index], sourceInstances[index]);
                }

                foreach (var property in request.RootProperties.Distinct(StringComparer.Ordinal))
                {
                    var descriptor = SerializedPropertyAccess.Describe(targetRoot, property)
                        ?? throw new InvalidOperationException($"hidden-root property '{property}' is unavailable");
                    if (!CanCopyFromModel(descriptor))
                    {
                        throw new InvalidOperationException(
                            $"hidden-root property is outside Carbon's model-copy policy ({descriptor.TypeName}; {descriptor.Attributes})");
                    }
                    // Reference properties are assigned after the desired hierarchy
                    // has its final native identities. Copying them here could leave
                    // a self-reference pointing at the temporary service carrier.
                    if (!descriptor.IsReference && !SerializedPropertyAccess.Copy(sourceRoot, targetRoot, property))
                    {
                        throw new InvalidOperationException($"engine rejected hidden-root property copy '{property}'");
                    }
                }

                targetRoot.Name = sourceRoot.Name;
                foreach (var child in targetRoot.GetChildren())
                {
                    if (child.Archivable && Reflection.IsSerializable(child))
                    {
                        child.Destroy();
                    }
                }
                foreach (var child in sourceRoot.GetChildren().ToArray())
                {
                    child.Parent = targetRoot;
                }

                foreach (var pair in request.KnownSourceDebugIds)
                {
                    if (!bySourceId.ContainsKey(pair.Key))
                    {
                        bySourceId.Add(pair.Key, Resolve(pair.Value));
                    }
                }
                foreach (var reference in request.References)
                {
                    var owner = bySourceId.TryGetValue(reference.OwnerSourceId, out var foundOwner)
                        ? foundOwner
                        : throw new KeyNotFoundException($"reference owner source '{reference.OwnerSourceId}' is unavailable");
                    var descriptor = SerializedPropertyAccess.Describe(owner, reference.Property)
                        ?? throw new InvalidOperationException($"reference property '{reference.Property}' is unavailable");
                    if (!CanTransportReference(descriptor))
                    {
                        throw new InvalidOperationException(
                            $"property is outside Carbon's serialized-reference policy ({descriptor.TypeName}; {descriptor.Attributes})");
                    }
                    var target = reference.TargetSourceId is null
                        ? null
                        : bySourceId.TryGetValue(reference.TargetSourceId, out var foundTarget)
                            ? foundTarget
                            : throw new KeyNotFoundException($"reference target source '{reference.TargetSourceId}' is unavailable");
                    Reflection.SetProperty<Instance?>(owner, reference.Property, target);
                }

                sourceRoot.Destroy();
                return new RootApplyModelResponse(
                    request.SourceIds.Select(sourceId =>
                        new SourceDebugIdentity(sourceId, bySourceId[sourceId].GetDebugId(128))).ToArray());
            }, cancellationToken);
        }
        finally
        {
            await OnEngineThread(() =>
            {
                foreach (var root in roots)
                {
                    try
                    {
                        if (root.Parent is null)
                        {
                            root.Destroy();
                        }
                    }
                    catch
                    {
                    }
                }
                return (object?)null;
            }, CancellationToken.None);
        }
    }

    private async Task<object> ValidateRootModelAsync(
        RootApplyModelRequest request,
        CancellationToken cancellationToken)
    {
        if (request.SourceIds.Length == 0 || request.SourceIds.Distinct(StringComparer.Ordinal).Count() != request.SourceIds.Length)
        {
            throw new InvalidOperationException("hidden-root validation requires unique source identities");
        }
        var bytes = Convert.FromBase64String(request.Model);
        var deserialization = await OnEngineThread(() =>
        {
            var dataModel = _dataModel ?? throw new InvalidOperationException("edit DataModel is unavailable");
            return dataModel.GetService<SerializationService>().DeserializeInstancesAsync(bytes);
        }, cancellationToken);
        var roots = await deserialization.WaitAsync(TimeSpan.FromSeconds(30), cancellationToken)
            ?? throw new InvalidOperationException("engine returned no hidden-root validation model");
        try
        {
            return await OnEngineThread(() =>
            {
                if (roots.Count != 1)
                {
                    throw new InvalidOperationException("hidden-root validation model must contain exactly one root");
                }
                var sourceRoot = roots[0];
                var targetRoot = Resolve(request.DebugId);
                if (sourceRoot.ClassName != targetRoot.ClassName)
                {
                    throw new InvalidOperationException("hidden-root validation changed the native root class");
                }
                var sourceInstances = Preorder(sourceRoot);
                if (sourceInstances.Count != request.SourceIds.Length)
                {
                    throw new InvalidOperationException("hidden-root validation source identity count does not match its model");
                }
                var bySourceId = request.SourceIds
                    .Select((sourceId, index) => (sourceId, instance: index == 0 ? targetRoot : sourceInstances[index]))
                    .ToDictionary(pair => pair.sourceId, pair => pair.instance, StringComparer.Ordinal);
                foreach (var property in request.RootProperties.Distinct(StringComparer.Ordinal))
                {
                    var descriptor = SerializedPropertyAccess.Describe(targetRoot, property)
                        ?? throw new InvalidOperationException($"hidden-root validation property '{property}' is unavailable");
                    if (!CanCopyFromModel(descriptor))
                    {
                        throw new InvalidOperationException(
                            $"hidden-root validation property is outside model-copy policy ({descriptor.TypeName}; {descriptor.Attributes})");
                    }
                }
                foreach (var reference in request.References)
                {
                    var owner = bySourceId.TryGetValue(reference.OwnerSourceId, out var foundOwner)
                        ? foundOwner
                        : throw new KeyNotFoundException($"validation reference owner source '{reference.OwnerSourceId}' is unavailable");
                    var descriptor = SerializedPropertyAccess.Describe(owner, reference.Property)
                        ?? throw new InvalidOperationException($"validation reference property '{reference.Property}' is unavailable");
                    if (!CanTransportReference(descriptor))
                    {
                        throw new InvalidOperationException(
                            $"validation reference is outside serialized-reference policy ({descriptor.TypeName}; {descriptor.Attributes})");
                    }
                    if (reference.TargetSourceId is not null && !bySourceId.ContainsKey(reference.TargetSourceId))
                    {
                        throw new KeyNotFoundException($"validation reference target source '{reference.TargetSourceId}' is unavailable");
                    }
                }
                return new { valid = true };
            }, cancellationToken);
        }
        finally
        {
            await OnEngineThread(() =>
            {
                foreach (var root in roots)
                {
                    try
                    {
                        if (root.Parent is null)
                        {
                            root.Destroy();
                        }
                    }
                    catch
                    {
                    }
                }
                return (object?)null;
            }, CancellationToken.None);
        }
    }

    private RootApplyBundlePlan ValidateRootBundle(
        RootApplyBundleRequest request,
        IReadOnlyList<Instance> sourceRoots)
    {
        if (sourceRoots.Count != request.Roots.Length)
        {
            throw new InvalidOperationException("hidden-root bundle root count does not match its manifest");
        }
        var allSourceIds = request.Roots.SelectMany(root => root.SourceIds).ToArray();
        if (request.Roots.Any(root => root.SourceIds.Length == 0)
            || allSourceIds.Distinct(StringComparer.Ordinal).Count() != allSourceIds.Length)
        {
            throw new InvalidOperationException("hidden-root bundle requires non-empty, globally unique source identities");
        }

        var targetRoots = new Instance[request.Roots.Length];
        var bySourceId = new Dictionary<string, Instance>(StringComparer.Ordinal);
        for (var rootIndex = 0; rootIndex < request.Roots.Length; rootIndex++)
        {
            var manifest = request.Roots[rootIndex];
            var sourceRoot = sourceRoots[rootIndex];
            var targetRoot = Resolve(manifest.DebugId);
            if (targetRoots.Take(rootIndex).Any(existing => existing == targetRoot))
            {
                throw new InvalidOperationException("hidden-root bundle addresses the same native root more than once");
            }
            if (sourceRoot.ClassName != targetRoot.ClassName)
            {
                throw new InvalidOperationException("hidden-root bundle changed a native root class");
            }
            var sourceInstances = Preorder(sourceRoot);
            if (sourceInstances.Count != manifest.SourceIds.Length)
            {
                throw new InvalidOperationException("hidden-root bundle source identity count does not match its model");
            }
            targetRoots[rootIndex] = targetRoot;
            bySourceId.Add(manifest.SourceIds[0], targetRoot);
            for (var instanceIndex = 1; instanceIndex < sourceInstances.Count; instanceIndex++)
            {
                bySourceId.Add(manifest.SourceIds[instanceIndex], sourceInstances[instanceIndex]);
            }

            foreach (var property in manifest.RootProperties.Distinct(StringComparer.Ordinal))
            {
                var descriptor = SerializedPropertyAccess.Describe(targetRoot, property)
                    ?? throw new InvalidOperationException($"hidden-root bundle property '{property}' is unavailable");
                if (!CanCopyFromModel(descriptor))
                {
                    throw new InvalidOperationException(
                        $"hidden-root bundle property is outside model-copy policy ({descriptor.TypeName}; {descriptor.Attributes})");
                }
            }
        }

        foreach (var pair in request.KnownSourceDebugIds)
        {
            if (!bySourceId.TryAdd(pair.Key, Resolve(pair.Value)))
            {
                throw new InvalidOperationException($"known source identity '{pair.Key}' collides with bundled state");
            }
        }
        foreach (var reference in request.References)
        {
            var owner = bySourceId.TryGetValue(reference.OwnerSourceId, out var foundOwner)
                ? foundOwner
                : throw new KeyNotFoundException($"bundle reference owner source '{reference.OwnerSourceId}' is unavailable");
            var descriptor = SerializedPropertyAccess.Describe(owner, reference.Property)
                ?? throw new InvalidOperationException($"bundle reference property '{reference.Property}' is unavailable");
            if (!CanTransportReference(descriptor))
            {
                throw new InvalidOperationException(
                    $"bundle reference is outside serialized-reference policy ({descriptor.TypeName}; {descriptor.Attributes})");
            }
            if (reference.TargetSourceId is not null && !bySourceId.ContainsKey(reference.TargetSourceId))
            {
                throw new KeyNotFoundException($"bundle reference target source '{reference.TargetSourceId}' is unavailable");
            }
        }
        return new RootApplyBundlePlan(targetRoots, bySourceId);
    }

    private async Task<object> ValidateRootBundleAsync(
        RootApplyBundleRequest request,
        CancellationToken cancellationToken)
    {
        var bytes = Convert.FromBase64String(request.Model);
        var deserialization = await OnEngineThread(() =>
        {
            var dataModel = _dataModel ?? throw new InvalidOperationException("edit DataModel is unavailable");
            return dataModel.GetService<SerializationService>().DeserializeInstancesAsync(bytes);
        }, cancellationToken);
        var sourceRoots = await deserialization.WaitAsync(TimeSpan.FromSeconds(30), cancellationToken)
            ?? throw new InvalidOperationException("engine returned no hidden-root validation bundle");
        try
        {
            return await OnEngineThread(() =>
            {
                ValidateRootBundle(request, sourceRoots);
                return new { valid = true };
            }, cancellationToken);
        }
        finally
        {
            await OnEngineThread(() =>
            {
                foreach (var root in sourceRoots)
                {
                    try
                    {
                        if (root.Parent is null)
                        {
                            root.Destroy();
                        }
                    }
                    catch
                    {
                    }
                }
                return (object?)null;
            }, CancellationToken.None);
        }
    }

    private async Task<RootApplyModelResponse> ApplyRootBundleAsync(
        RootApplyBundleRequest request,
        CancellationToken cancellationToken)
    {
        var bytes = Convert.FromBase64String(request.Model);
        var deserialization = await OnEngineThread(() =>
        {
            var dataModel = _dataModel ?? throw new InvalidOperationException("edit DataModel is unavailable");
            return dataModel.GetService<SerializationService>().DeserializeInstancesAsync(bytes);
        }, cancellationToken);
        var sourceRoots = await deserialization.WaitAsync(TimeSpan.FromSeconds(30), cancellationToken)
            ?? throw new InvalidOperationException("engine returned no hidden-root apply bundle");
        try
        {
            return await OnEngineThread(() =>
            {
                // Resolve every root, source identity, property descriptor, and Ref
                // endpoint before the first mutation. All roots then change in one
                // engine callback, preserving links between their descendants.
                var plan = ValidateRootBundle(request, sourceRoots);
                for (var rootIndex = 0; rootIndex < request.Roots.Length; rootIndex++)
                {
                    var manifest = request.Roots[rootIndex];
                    var sourceRoot = sourceRoots[rootIndex];
                    var targetRoot = plan.TargetRoots[rootIndex];
                    foreach (var property in manifest.RootProperties.Distinct(StringComparer.Ordinal))
                    {
                        var descriptor = SerializedPropertyAccess.Describe(targetRoot, property)
                            ?? throw new InvalidOperationException($"hidden-root bundle property '{property}' disappeared after validation");
                        if (!descriptor.IsReference && !SerializedPropertyAccess.Copy(sourceRoot, targetRoot, property))
                        {
                            throw new InvalidOperationException($"engine rejected hidden-root bundle property copy '{property}'");
                        }
                    }
                    targetRoot.Name = sourceRoot.Name;
                }

                foreach (var targetRoot in plan.TargetRoots)
                {
                    foreach (var child in targetRoot.GetChildren())
                    {
                        if (child.Archivable && Reflection.IsSerializable(child))
                        {
                            child.Destroy();
                        }
                    }
                }
                for (var rootIndex = 0; rootIndex < sourceRoots.Count; rootIndex++)
                {
                    foreach (var child in sourceRoots[rootIndex].GetChildren().ToArray())
                    {
                        child.Parent = plan.TargetRoots[rootIndex];
                    }
                }
                foreach (var reference in request.References)
                {
                    var owner = plan.BySourceId[reference.OwnerSourceId];
                    var target = reference.TargetSourceId is null
                        ? null
                        : plan.BySourceId[reference.TargetSourceId];
                    Reflection.SetProperty<Instance?>(owner, reference.Property, target);
                }

                foreach (var sourceRoot in sourceRoots)
                {
                    sourceRoot.Destroy();
                }
                return new RootApplyModelResponse(
                    request.Roots.SelectMany(root => root.SourceIds).Select(sourceId =>
                        new SourceDebugIdentity(sourceId, plan.BySourceId[sourceId].GetDebugId(128))).ToArray());
            }, cancellationToken);
        }
        finally
        {
            await OnEngineThread(() =>
            {
                foreach (var root in sourceRoots)
                {
                    try
                    {
                        if (root.Parent is null)
                        {
                            root.Destroy();
                        }
                    }
                    catch
                    {
                    }
                }
                return (object?)null;
            }, CancellationToken.None);
        }
    }

    private static List<Instance> Preorder(Instance root)
    {
        var instances = new List<Instance>();
        var stack = new Stack<Instance>();
        stack.Push(root);
        while (stack.TryPop(out var instance))
        {
            instances.Add(instance);
            var children = instance.GetChildren();
            for (var index = children.Count - 1; index >= 0; index--)
            {
                stack.Push(children[index]);
            }
        }
        return instances;
    }

    private async Task<object> TriggerRejectedYieldAsync(CancellationToken cancellationToken)
    {
        var serialization = await OnEngineThread(() =>
        {
            var dataModel = _dataModel ?? throw new InvalidOperationException("edit DataModel is unavailable");
            var service = dataModel.GetService<SerializationService>();
            IReadOnlyList<Instance> invalidInput = [service];
            return Reflection.InvokeAsync<byte[]>(service, "SerializeInstancesAsync", invalidInput);
        }, cancellationToken);

        try
        {
            _ = await serialization.WaitAsync(TimeSpan.FromSeconds(10), cancellationToken);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            return new { rejected = true, error = ex.Message };
        }

        throw new InvalidOperationException("diagnostic yield unexpectedly accepted a DataModel service");
    }

    internal static bool IsDiagnosticRouteSupported(string path) =>
        string.Equals(path, "/v1/diagnostics/rejected-yield", StringComparison.Ordinal)
        || string.Equals(path, "/v1/diagnostics/save-local-place", StringComparison.Ordinal);

    internal static (string StudioSessionId, string InstanceId)? ParseStudioRoute(string value)
    {
        var separator = value.IndexOf('\n');
        if (separator <= 0
            || separator == value.Length - 1
            || value.IndexOf('\n', separator + 1) >= 0)
        {
            return null;
        }

        return (value[..separator], value[(separator + 1)..]);
    }

    internal static string StudioRouteKey(string studioSessionId, string instanceId)
    {
        var bytes = Encoding.UTF8.GetBytes($"{studioSessionId}\n{instanceId}");
        var hash = 14695981039346656037UL;
        foreach (var value in bytes)
        {
            hash ^= value;
            hash = unchecked(hash * 1099511628211UL);
        }
        return hash.ToString("x16", CultureInfo.InvariantCulture);
    }

    internal static (string StudioSessionId, string InstanceId)? UniqueStudioRoute(
        IEnumerable<(string StudioSessionId, string InstanceId)> candidates)
    {
        (string StudioSessionId, string InstanceId)? route = null;
        foreach (var candidate in candidates)
        {
            if (route is not null)
            {
                return null;
            }
            route = candidate;
        }
        return route;
    }

    internal static bool CanResumeStudioRoute(
        nuint detachedDataModelHandle,
        (string StudioSessionId, string InstanceId)? detachedRoute,
        (string StudioSessionId, string InstanceId)? activeRoute) =>
        detachedDataModelHandle != 0
        && detachedRoute is not null
        && detachedRoute == activeRoute;

    private void TryCacheStudioIdentity(Instance instance)
    {
        var handle = InstanceHierarchy.RuntimeHandle(instance);
        (string StudioSessionId, string InstanceId)? route = null;
        try
        {
            if (instance.Name == StudioRouteMarker
                && instance.ClassName == "StringValue"
                && !instance.Archivable
                && instance.Parent is { } parent
                && parent.ClassName == "CoreGui")
            {
                route = ParseStudioRoute(
                    Reflection.GetProperty<string>(instance, "Value") ?? string.Empty);
            }
        }
        catch
        {
        }

        StudioIdentity? publishedIdentity;
        var changed = false;
        lock (_engineStateLock)
        {
            if (_dataModel is null)
            {
                return;
            }

            if (route is { } validRoute)
            {
                _studioIdentityCandidates[handle] = validRoute;
            }
            else
            {
                _studioIdentityCandidates.Remove(handle);
            }
            _preservedStudioRoute = UniqueStudioRoute(_studioIdentityCandidates.Values);
            var previous = _studioIdentity;
            RefreshStudioIdentityLocked();
            changed = previous != _studioIdentity;
            publishedIdentity = _studioIdentity;
        }
        if (changed)
        {
            PublishStudioIdentity(publishedIdentity);
        }
    }

    private void ClearStudioIdentity(Instance instance)
    {
        var handle = InstanceHierarchy.RuntimeHandle(instance);
        StudioIdentity? publishedIdentity;
        var changed = false;
        lock (_engineStateLock)
        {
            _studioIdentityCandidates.Remove(handle);
            _preservedStudioRoute = UniqueStudioRoute(_studioIdentityCandidates.Values);
            var previous = _studioIdentity;
            RefreshStudioIdentityLocked();
            changed = previous != _studioIdentity;
            publishedIdentity = _studioIdentity;
        }
        if (changed)
        {
            PublishStudioIdentity(publishedIdentity);
        }
    }

    private void RefreshStudioIdentityLocked()
    {
        _studioIdentity = null;
        var route = UniqueStudioRoute(_studioIdentityCandidates.Values);
        if (route is null)
        {
            return;
        }

        _studioIdentity = new StudioIdentity(
            route.Value.StudioSessionId,
            route.Value.InstanceId,
            _bridgeId,
            Environment.ProcessId);
    }

    private StudioIdentity GetStudioIdentity()
    {
        lock (_engineStateLock)
        {
            return _studioIdentity
                ?? throw new KeyNotFoundException("Carbon Studio routing marker is unavailable or ambiguous");
        }
    }

    private Instance Resolve(string debugId)
    {
        if (_instances.TryGetValue(debugId, out var instance))
        {
            return instance;
        }
        if (ManagedHierarchy.TryParseRuntimeIdentity(debugId, out var handle))
        {
            instance = Instance.FromHandle(handle)
                ?? throw new KeyNotFoundException($"instance '{debugId}' is unavailable");
            _instances[debugId] = instance;
            return instance;
        }
        throw new KeyNotFoundException($"instance '{debugId}' is unavailable");
    }

    private async Task<ChangeBatch> ChangesAfterAsync(long after, CancellationToken cancellationToken)
    {
        ChangeBatch Snapshot()
        {
            lock (_changesLock)
            {
                var acknowledged = _changes.FindLastIndex(change => change.Sequence <= after);
                if (acknowledged >= 0)
                {
                    _changes.RemoveRange(0, acknowledged + 1);
                }
                var changes = _changes.Take(512).ToArray();
                lock (_managedHierarchyLock)
                {
                    return new(
                        changes.Select(change => _managedByDebug.TryGetValue(change.DebugId, out var binding)
                            ? change with { SourceId = binding.SourceId }
                            : change).ToArray(),
                        _diagnostics.Take(512).ToArray());
                }
            }
        }

        var snapshot = Snapshot();
        if (snapshot.Changes.Length > 0 || snapshot.Diagnostics.Length > 0)
        {
            return snapshot;
        }
        try
        {
            await _changesReady.WaitAsync(TimeSpan.FromSeconds(20), cancellationToken);
        }
        catch (OperationCanceledException)
        {
        }
        return Snapshot();
    }

    private bool Authorized(HttpListenerRequest request)
    {
        const string prefix = "Bearer ";
        var authorization = request.Headers["Authorization"];
        if (authorization is null || !authorization.StartsWith(prefix, StringComparison.Ordinal))
        {
            return false;
        }
        var supplied = Encoding.UTF8.GetBytes(authorization[prefix.Length..]);
        var expected = Encoding.UTF8.GetBytes(_token);
        return supplied.Length == expected.Length && CryptographicOperations.FixedTimeEquals(supplied, expected);
    }

    private static async Task<T> ReadAsync<T>(HttpListenerRequest request)
    {
        return await JsonSerializer.DeserializeAsync<T>(request.InputStream, JsonOptions)
            ?? throw new InvalidOperationException("request body is missing");
    }

    private static async Task<byte[]> ReadBytesAsync(
        HttpListenerRequest request,
        int maxBytes,
        CancellationToken cancellationToken)
    {
        if (request.ContentLength64 < 0 || request.ContentLength64 > maxBytes)
        {
            throw new InvalidOperationException("binary request body has an invalid length");
        }
        using var output = new MemoryStream(checked((int)request.ContentLength64));
        var buffer = new byte[64 * 1024];
        while (true)
        {
            var read = await request.InputStream.ReadAsync(buffer, cancellationToken);
            if (read == 0)
            {
                break;
            }
            if (output.Length + read > maxBytes)
            {
                throw new InvalidOperationException("binary request body exceeds the size limit");
            }
            output.Write(buffer, 0, read);
        }
        return output.ToArray();
    }

    private static async Task ReplyAsync(HttpListenerResponse response, HttpStatusCode status, object body)
    {
        response.StatusCode = (int)status;
        response.ContentType = "application/json";
        await JsonSerializer.SerializeAsync(response.OutputStream, body, body.GetType(), JsonOptions);
        response.Close();
    }

    private void PublishStudioIdentity(StudioIdentity? identity)
    {
        try
        {
            WriteDiscovery(identity);
        }
        catch (Exception ex)
        {
            ReportWarning($"Failed to publish Carbon Studio routing identity: {ex.Message}");
        }
    }

    private void WriteDiscovery(StudioIdentity? identity)
    {
        var rmlBuildVersion = AttestedRmlBuildVersion();
        var json = JsonSerializer.Serialize(new
        {
            protocolVersion = ProtocolVersion,
            rmlBuildVersion,
            bridgeId = _bridgeId,
            endpoint = _endpoint,
            wslEndpoint = _wslEndpoint,
            token = _token,
            processId = Environment.ProcessId,
            studioSessionId = identity?.StudioSessionId,
            instanceId = identity?.InstanceId,
        }, JsonOptions);
        lock (_discoveryWriteLock)
        {
            WriteDiscoveryFile(_discoveryPath, json);
            var routeDiscoveryPath = identity is null
                ? string.Empty
                : IOPath.Combine(
                    Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
                    "RobloxModLoader",
                    "carbon-bridges",
                    "routes",
                    "v1",
                    StudioRouteKey(identity.StudioSessionId, identity.InstanceId),
                    $"{_bridgeId}.json");
            if (routeDiscoveryPath.Length != 0)
            {
                Directory.CreateDirectory(IOPath.GetDirectoryName(routeDiscoveryPath)!);
                WriteDiscoveryFile(routeDiscoveryPath, json);
            }
            if (_routeDiscoveryPath.Length != 0
                && !string.Equals(_routeDiscoveryPath, routeDiscoveryPath, StringComparison.Ordinal))
            {
                IOFile.Delete(_routeDiscoveryPath);
            }
            _routeDiscoveryPath = routeDiscoveryPath;
        }
    }

    private static void WriteDiscoveryFile(string path, string json)
    {
        var temporary = path + ".tmp";
        IOFile.WriteAllText(temporary, json);
        IOFile.Move(temporary, path, true);
        if (!OperatingSystem.IsWindows())
        {
            IOFile.SetUnixFileMode(path, UnixFileMode.UserRead | UnixFileMode.UserWrite);
        }
    }

    private string DiscoveryPath()
    {
        return IOPath.Combine(DiscoveryRoot(), "v1", $"{_bridgeId}.json");
    }

    private static string DiscoveryRoot()
    {
        var local = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
        return IOPath.Combine(local, "RobloxModLoader", "carbon-bridges");
    }

    internal static string ResolveBridgeId(string? configured, Func<string> generate)
    {
        if (configured is not null
            && configured.Length == 32
            && configured.All(Uri.IsHexDigit))
        {
            return configured;
        }
        return generate();
    }

    internal static int PruneStaleDiscoveryRecords(
        string root,
        Func<int, bool> processIsRunning)
    {
        var paths = new List<string>();
        var main = IOPath.Combine(root, "v1");
        var routes = IOPath.Combine(root, "routes", "v1");
        try
        {
            if (Directory.Exists(main))
            {
                paths.AddRange(Directory.GetFiles(main, "*.json", SearchOption.TopDirectoryOnly));
            }
        }
        catch
        {
        }
        try
        {
            if (Directory.Exists(routes))
            {
                paths.AddRange(Directory.GetFiles(routes, "*.json", SearchOption.AllDirectories));
            }
        }
        catch
        {
        }

        var removed = 0;
        foreach (var path in paths.Distinct(StringComparer.OrdinalIgnoreCase))
        {
            try
            {
                var processId = DiscoveryProcessId(path);
                if (processId is null || processIsRunning(processId.Value))
                {
                    continue;
                }

                // Re-read immediately before deletion so a concurrent atomic
                // replacement for a later process can never lose its record.
                var currentProcessId = DiscoveryProcessId(path);
                if (currentProcessId != processId
                    || currentProcessId is null
                    || processIsRunning(currentProcessId.Value))
                {
                    continue;
                }
                IOFile.Delete(path);
                removed++;
            }
            catch
            {
                // An unreadable or concurrently replaced record is preserved.
            }
        }
        return removed;
    }

    private static int? DiscoveryProcessId(string path)
    {
        using var document = JsonDocument.Parse(IOFile.ReadAllText(path));
        return document.RootElement.TryGetProperty("processId", out var processId)
            && processId.TryGetInt32(out var value)
            && value > 0
                ? value
                : null;
    }

    internal static bool IsStudioProcessRunning(int processId)
    {
        try
        {
            using var process = Process.GetProcessById(processId);
            return string.Equals(
                    process.ProcessName,
                    "RobloxStudioBeta",
                    StringComparison.OrdinalIgnoreCase)
                && !process.HasExited;
        }
        catch (ArgumentException)
        {
            return false;
        }
        catch
        {
            // Permission and transient inspection failures must preserve data.
            return true;
        }
    }

    private static int ReserveLoopbackPort()
    {
        using var listener = new TcpListener(IPAddress.Loopback, 0);
        listener.Start();
        return ((IPEndPoint)listener.LocalEndpoint).Port;
    }

    private static IPAddress? FindWslAddress()
    {
        foreach (var network in NetworkInterface.GetAllNetworkInterfaces())
        {
            if (!network.Name.Contains("WSL", StringComparison.OrdinalIgnoreCase)
                && !network.Description.Contains("WSL", StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }

            foreach (var unicast in network.GetIPProperties().UnicastAddresses)
            {
                if (unicast.Address.AddressFamily == AddressFamily.InterNetwork
                    && !IPAddress.IsLoopback(unicast.Address))
                {
                    return unicast.Address;
                }
            }
        }

        return null;
    }

    internal sealed record EngineWork(
        long Generation,
        Func<object?> Callback,
        TaskCompletionSource<object?> Completion)
    {
        public void Run(long currentGeneration)
        {
            if (Completion.Task.IsCompleted)
            {
                return;
            }
            if (Generation != currentGeneration)
            {
                Completion.TrySetException(new InvalidOperationException(
                    "engine work belongs to a detached DataModel session"));
                return;
            }
            try
            {
                Completion.TrySetResult(Callback());
            }
            catch (Exception ex)
            {
                Completion.TrySetException(ex);
            }
        }

        public void Fail(Exception error)
        {
            Completion.TrySetException(error);
        }
    }

    private sealed record PropertyRequest(string DebugId, string Property);
    private sealed record ManagedHierarchyAttachmentRequest(string ContractId);
    private sealed record ManifestIdentityBootstrapRequest(
        string RootSourceId,
        int ExpectedSourceInstances,
        string ExpectedDigest,
        IReadOnlyList<ManifestIdentityRebinding> Rebindings);
    private sealed record ManifestIdentityBootstrapResponse(
        bool Authoritative,
        int SourceInstances,
        string Digest);
    private sealed record ManifestIdentityRemapResponse(
        bool Complete,
        bool Authoritative,
        int SourceInstances,
        string Digest,
        string CaptureId);
    private sealed class ManifestIdentityRemapSession(ManifestIdentity captureId, int total)
    {
        internal ManifestIdentity CaptureId { get; } = captureId;
        internal int Total { get; } = total;
        internal int Next { get; set; }
        internal Dictionary<ManifestIdentity, ManifestIdentity> Mappings { get; } = [];
    }
    private sealed record ManagedSourceContract(
        string ContractId,
        IReadOnlyList<ManagedSourceNode> Source,
        IReadOnlyDictionary<string, int> IndexBySourceId,
        IReadOnlyList<int>[] ChildrenByParent)
    {
        internal static ManagedSourceContract Create(
            string contractId,
            IReadOnlyList<ManagedSourceNode> source)
        {
            var indexBySourceId = new Dictionary<string, int>(source.Count, StringComparer.Ordinal);
            var childrenByParent = Enumerable.Range(0, source.Count)
                .Select(_ => (IReadOnlyList<int>)new List<int>())
                .ToArray();
            for (var index = 0; index < source.Count; index++)
            {
                if (!indexBySourceId.TryAdd(source[index].SourceId, index))
                {
                    throw new InvalidDataException(
                        $"managed source duplicated identity {source[index].SourceId}");
                }
                if (index == 0)
                {
                    continue;
                }
                var parentIndex = source[index].ParentIndex;
                if (parentIndex < 0 || parentIndex >= index)
                {
                    throw new InvalidDataException(
                        $"managed source identity {source[index].SourceId} has an invalid parent index");
                }
                ((List<int>)childrenByParent[parentIndex]).Add(index);
            }
            return new(contractId, source, indexBySourceId, childrenByParent);
        }
    }
    private sealed class ManagedSourceReplacementPendingException(string sourceId)
        : InvalidOperationException($"managed source identity {sourceId} is duplicated")
    {
        internal string SourceId { get; } = sourceId;
    }
    private sealed record ManagedIdentityRequest(string RequestId, string[] SourceIds, string[] DebugIds);
    private sealed record ManagedIdentityPollRequest(string RequestId);
    private sealed record ManagedRuntimeSnapshot(
        IReadOnlyList<ManagedRuntimeNode> Nodes,
        string RootDebugId,
        long HierarchySequence,
        long ChangeSequence,
        HashSet<string> RuntimeOnlyRootDebugIds,
        Dictionary<string, string> RootStudioDebugIds,
        ManagedHierarchy.RuntimeShapeIndex RuntimeShapes);
    private sealed record ManagedAttachmentReceipt(
        string ContractId,
        int SourceInstances,
        long HierarchySequence,
        long ChangeSequence,
        string[] SourceRootDebugIds);
    private sealed record ManagedRuntimeHierarchy(
        IReadOnlyList<ManagedRuntimeNode> Nodes,
        string RootDebugId,
        HashSet<string> RuntimeOnlyRootDebugIds,
        Dictionary<string, string> RootStudioDebugIds,
        Dictionary<nuint, LaunchHydratedServiceDefaults> LaunchHydratedRootDefaults,
        string[] LaunchHydratedDefaultFailures);
    private sealed record LaunchHydratedServiceDefaults(
        string ClassName,
        string Name,
        IReadOnlyDictionary<string, SerializedPropertySnapshot> Properties);
    private sealed record PropertyBatchRequest(PropertyRequest[] Requests);
    private sealed record DefaultPropertiesRequest(string ClassName, string[] Properties);
    private sealed record PropertyBatchRead(string? TypeName, string? Value, string? Error, string? ModelRootDebugId);
    private sealed record PropertyBatchCapture(
        PropertyBatchRead[] Values,
        Task<byte[]?>? Serialization,
        Instance[] SerializationRoots);
    private sealed record PropertyBatchResponse(PropertyBatchRead[] Values, string? Model);
    private sealed record ReferenceBatchRead(string? TargetDebugId, string? SourceId, string? Error);
    private sealed record ReferenceBatchResponse(ReferenceBatchRead[] Values);
    private sealed record ReferenceWriteRequest(string DebugId, string Property, string? TargetDebugId);
    private sealed record PropertyWriteRequest(string DebugId, string Property, string Value);
    private sealed record PropertyCopyRequest(string SourceDebugId, string TargetDebugId, string Property);
    private sealed record MaterializedPropertyWriteRequest(string DebugId, string Property, string Model);
    private sealed record CreateRequest(string ClassName, string ParentDebugId, string Name);
    private sealed record RootModelRequest(string[] DebugIds);
    private sealed record RootApplyReference(string OwnerSourceId, string Property, string? TargetSourceId);
    private sealed record RootApplyModelRequest(
        string DebugId,
        string Model,
        string[] SourceIds,
        string[] RootProperties,
        RootApplyReference[] References,
        Dictionary<string, string> KnownSourceDebugIds);
    private sealed record RootApplyBundleRoot(
        string DebugId,
        string[] SourceIds,
        string[] RootProperties);
    private sealed record RootApplyBundleRequest(
        string Model,
        RootApplyBundleRoot[] Roots,
        RootApplyReference[] References,
        Dictionary<string, string> KnownSourceDebugIds);
    private sealed record RootApplyBundlePlan(
        Instance[] TargetRoots,
        Dictionary<string, Instance> BySourceId);
    private sealed record SourceDebugIdentity(string SourceId, string DebugId);
    private sealed record RootApplyModelResponse(SourceDebugIdentity[] SourceInstances);
    private sealed record RootIdentity(string ClassName, string Name, string DebugId, bool InitiallyPresent);
    private sealed record SerializableRoot(Instance Instance, RootIdentity Identity);
    private static class CaptureHierarchyFlags
    {
        internal const uint Serialized = 1 << 0;
        internal const uint ServiceShell = 1 << 1;
        internal const uint DefaultHydratedService = 1 << 2;
    }
    private sealed record CaptureLeaseAcquisition(
        byte[] NativePayload,
        ManagedSourceContract StagedManaged,
        IReadOnlyDictionary<string, nuint> MappedRootHandles,
        IReadOnlyDictionary<nuint, string> MappedSourceIdsByHandle,
        IReadOnlyDictionary<nuint, CapturePublicRootClass> PublicRootClasses,
        nuint ExcludedEditCameraHandle,
        Dictionary<nuint, LaunchHydratedServiceDefaults> LaunchHydratedRootDefaults,
        long HierarchySequence,
        long ChangeSequence);
    private sealed record CapturePublicRootClass(string? ClassName, Exception? Error);
    private sealed record CaptureLeaseEnginePlan(
        CaptureEnvelopeData Envelope,
        CaptureSerializationChunk[] Chunks,
        CaptureDirtyPagePlan PagePlan,
        Instance[] MappedRoots,
        Instance[] TemporaryRoots);
    private sealed record CaptureLeaseEngineChunk(
        Task<byte[]?> Serialization,
        CaptureArchivableMaskEntry[] TemporarilyNonArchivableRoots);
    private sealed record CaptureSerializationChunk(
        uint[] RootOrdinals,
        nuint[] RootHandles,
        nuint[] MaskedRootHandles,
        int PageIndex,
        CaptureCachedPage? ReusedPayload);
    private sealed record CapturePlannedChunk(
        uint[] RootOrdinals,
        nuint[] RootHandles,
        nuint[] FrontierHandles,
        nuint[] MemberHandles,
        uint[] DependencyOrdinals,
        nuint[] DependencyRootHandles,
        nuint[] MaskedDependencyChildHandles,
        string PageId);
    private readonly record struct CaptureArchivableMaskEntry(Instance Root, nuint Handle);
    private sealed record CaptureLeaseCompletion(
        long HierarchySequence,
        long ChangeSequence);
    private sealed record RootModelCapture(
        Task<byte[]?> Serialization,
        RootIdentity[] Roots,
        string[] ModelRootParentDebugIds,
        string[] InstanceDebugIds,
        Dictionary<string, string> RootPropertyCarriers,
        Dictionary<string, string[]> RootPropertyCarrierInstanceDebugIds,
        long HierarchySequence,
        long ChangeSequence,
        Instance[] TemporaryRoots);
    private sealed record RootModelResponse(
        string Model,
        RootIdentity[] Roots,
        string[] ModelRootParentDebugIds,
        string[] InstanceDebugIds,
        Dictionary<string, string> RootPropertyCarriers,
        Dictionary<string, string[]> RootPropertyCarrierInstanceDebugIds,
        long ChangeSequence);
    private sealed record StudioIdentity(string StudioSessionId, string InstanceId, string BridgeId, int ProcessId);
    private sealed record PropertyReadResponse(string TypeName, string Value, string? Model, string? ModelRootDebugId);
    private sealed record PropertyChange(
        long Sequence,
        string DebugId,
        string Property,
        string Kind,
        string? RootDebugId,
        string? SourceId);
    private sealed record BridgeDiagnostic(long Sequence, string Severity, string Message);
    private sealed record ChangeBatch(PropertyChange[] Changes, BridgeDiagnostic[] Diagnostics);
}

internal sealed class CaptureArchivableMaskTracker
{
    private readonly object _lock = new();
    private readonly Dictionary<nuint, Entry> _entries = [];

    internal void Register(string captureId, IReadOnlyList<nuint> handles)
    {
        lock (_lock)
        {
            if (handles.Distinct().Count() != handles.Count)
            {
                throw new InvalidOperationException(
                    "capture Archivable mask contains duplicate runtime roots");
            }
            if (handles.Any(_entries.ContainsKey))
            {
                throw new InvalidOperationException(
                    "capture Archivable mask overlaps another lease");
            }
            foreach (var handle in handles)
            {
                _entries.Add(handle, new(captureId));
            }
        }
    }

    internal void ExpectNotification(string captureId, nuint handle)
    {
        lock (_lock)
        {
            var entry = OwnedEntry(captureId, handle);
            if (entry.RestorationComplete)
            {
                throw new InvalidOperationException(
                    $"capture {captureId} Archivable mask 0x{handle:x} is already restored");
            }
            entry.ExpectedNotifications = checked(entry.ExpectedNotifications + 1);
        }
    }

    internal void CancelExpectedNotification(string captureId, nuint handle)
    {
        lock (_lock)
        {
            if (!_entries.TryGetValue(handle, out var entry)
                || !string.Equals(entry.CaptureId, captureId, StringComparison.Ordinal)
                || entry.ExpectedNotifications == 0)
            {
                return;
            }
            entry.ExpectedNotifications--;
        }
    }

    internal bool TryConsume(string propertyName, nuint handle)
    {
        if (!string.Equals(propertyName, "Archivable", StringComparison.Ordinal))
        {
            return false;
        }
        lock (_lock)
        {
            if (!_entries.TryGetValue(handle, out var entry)
                || entry.ExpectedNotifications == 0)
            {
                // Ownership alone is not a suppression grant. An unexpected
                // edit to Archivable must still advance the change sequence.
                return false;
            }
            entry.ExpectedNotifications--;
            if (entry.RestorationComplete && entry.ExpectedNotifications == 0)
            {
                _entries.Remove(handle);
            }
            return true;
        }
    }

    internal void CompleteRestoration(string captureId, IReadOnlyList<nuint> handles)
    {
        lock (_lock)
        {
            // Validate the entire ownership set before changing any entry so a
            // corrupt cleanup cannot partially retire its quarantine.
            foreach (var handle in handles)
            {
                _ = OwnedEntry(captureId, handle);
            }
            foreach (var handle in handles)
            {
                var entry = _entries[handle];
                entry.RestorationComplete = true;
                if (entry.ExpectedNotifications == 0)
                {
                    _entries.Remove(handle);
                }
            }
        }
    }

    internal bool Contains(nuint handle)
    {
        lock (_lock)
        {
            return _entries.ContainsKey(handle);
        }
    }

    private Entry OwnedEntry(string captureId, nuint handle)
    {
        if (!_entries.TryGetValue(handle, out var entry)
            || !string.Equals(entry.CaptureId, captureId, StringComparison.Ordinal))
        {
            throw new InvalidOperationException(
                $"capture {captureId} lost ownership of Archivable mask 0x{handle:x}");
        }
        return entry;
    }

    private sealed class Entry(string captureId)
    {
        internal string CaptureId { get; } = captureId;
        internal int ExpectedNotifications { get; set; }
        internal bool RestorationComplete { get; set; }
    }
}

internal sealed class CaptureLeaseLaunchGate
{
    private readonly object _lock = new();
    private bool _started;
    private bool _cancelled;

    internal void Start(CancellationToken cancellationToken)
    {
        lock (_lock)
        {
            if (_cancelled)
            {
                throw new OperationCanceledException(
                    "capture serializer launch was cancelled before it started");
            }
            cancellationToken.ThrowIfCancellationRequested();
            _started = true;
        }
    }

    internal bool CancelBeforeStart(Action cancel)
    {
        lock (_lock)
        {
            if (_started)
            {
                return false;
            }
            _cancelled = true;
            cancel();
            return true;
        }
    }
}
