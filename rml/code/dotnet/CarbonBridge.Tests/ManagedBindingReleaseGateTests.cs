using Xunit;

namespace Carbon.RmlBridge.Tests;

public sealed class ManagedBindingReleaseGateTests
{
    [Fact]
    public void ReaddCancelsDelayedRelease()
    {
        var gate = new ManagedBindingReleaseGate();
        var token = gate.Schedule("runtime", "debug");

        Assert.True(gate.Cancel("runtime", "debug"));

        Assert.False(gate.Complete("runtime", token));
    }

    [Fact]
    public void OnlyLatestRemovalCanReleaseBinding()
    {
        var gate = new ManagedBindingReleaseGate();
        var stale = gate.Schedule("runtime", "old-debug");
        var current = gate.Schedule("runtime", "current-debug");

        Assert.False(gate.Complete("runtime", stale));
        Assert.True(gate.Complete("runtime", current));
        Assert.False(gate.Complete("runtime", current));
    }

    [Fact]
    public void ReusedHandleCannotCancelAnotherLifetimeRelease()
    {
        var gate = new ManagedBindingReleaseGate();
        var token = gate.Schedule("runtime", "deleted-debug");

        Assert.False(gate.Cancel("runtime", "replacement-debug"));
        Assert.True(gate.HasPending("runtime"));
        Assert.True(gate.Complete("runtime", token));
    }
}
