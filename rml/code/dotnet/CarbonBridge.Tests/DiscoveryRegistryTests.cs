using System.Text.Json;

using Xunit;

namespace Carbon.RmlBridge.Tests;

public sealed class DiscoveryRegistryTests : IDisposable
{
    private readonly string _root = Path.Combine(
        Path.GetTempPath(),
        $"carbon-discovery-pruning-{Environment.ProcessId}-{Guid.NewGuid():N}");

    [Fact]
    public void PrunesDeadProcessRecordsWhilePreservingLiveAndUnreadableRecords()
    {
        var staleBridge = "0123456789abcdef0123456789abcdef";
        var liveBridge = "fedcba9876543210fedcba9876543210";
        var mainStale = Path.Combine(_root, "v1", $"{staleBridge}.json");
        var mainLive = Path.Combine(_root, "v1", $"{liveBridge}.json");
        var mainUnreadable = Path.Combine(_root, "v1", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json");
        var routeStale = Path.Combine(_root, "routes", "v1", "route", $"{staleBridge}.json");
        var routeLive = Path.Combine(_root, "routes", "v1", "route", $"{liveBridge}.json");

        WriteDiscovery(mainStale, staleBridge, 41);
        WriteDiscovery(mainLive, liveBridge, 42);
        WriteDiscovery(routeStale, staleBridge, 41);
        WriteDiscovery(routeLive, liveBridge, 42);
        Directory.CreateDirectory(Path.GetDirectoryName(mainUnreadable)!);
        File.WriteAllText(mainUnreadable, "not-json");

        Assert.Equal(
            2,
            CarbonBridgeMod.PruneStaleDiscoveryRecords(
                _root,
                processId => processId == 42));
        Assert.False(File.Exists(mainStale));
        Assert.False(File.Exists(routeStale));
        Assert.True(File.Exists(mainLive));
        Assert.True(File.Exists(routeLive));
        Assert.True(File.Exists(mainUnreadable));
    }

    [Fact]
    public void UsesAConfiguredExactBridgeIdentityOnlyWhenItIsValid()
    {
        var generated = "fedcba9876543210fedcba9876543210";
        Assert.Equal(
            "0123456789abcdef0123456789abcdef",
            CarbonBridgeMod.ResolveBridgeId(
                "0123456789abcdef0123456789abcdef",
                () => generated));
        Assert.Equal(generated, CarbonBridgeMod.ResolveBridgeId(null, () => generated));
        Assert.Equal(generated, CarbonBridgeMod.ResolveBridgeId("../not-a-bridge", () => generated));
    }

    [Fact]
    public void RejectsALiveProcessThatIsNotRobloxStudio()
    {
        Assert.False(CarbonBridgeMod.IsStudioProcessRunning(Environment.ProcessId));
    }

    public void Dispose()
    {
        if (Directory.Exists(_root))
        {
            Directory.Delete(_root, true);
        }
    }

    private static void WriteDiscovery(string path, string bridgeId, int processId)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        File.WriteAllText(
            path,
            JsonSerializer.Serialize(new
            {
                protocolVersion = 2,
                rmlBuildVersion = "test",
                bridgeId,
                endpoint = "http://127.0.0.1:1/",
                wslEndpoint = (string?)null,
                token = "secret",
                processId,
            }));
    }
}
