using System.Text.Json;

using Xunit;

namespace Carbon.RmlBridge.Tests;

public sealed class LiveSessionContractTests
{
    private static readonly JsonSerializerOptions JsonOptions = new(JsonSerializerDefaults.Web);

    [Fact]
    public void PayloadContainsOnlyTheAuthenticatedTransientContract()
    {
        var json = LiveSessionContract.ValidateAndSerialize(
            new LiveSessionRequest(
                "http://127.0.0.1:41234",
                "Captured Place",
                "secret-session",
                "source-generation"),
            JsonOptions);
        using var payload = JsonDocument.Parse(json);
        var root = payload.RootElement;

        Assert.Equal(1, root.GetProperty("protocolVersion").GetInt32());
        Assert.Equal("http://127.0.0.1:41234", root.GetProperty("endpoint").GetString());
        Assert.Equal("Captured Place", root.GetProperty("project").GetString());
        Assert.Equal("secret-session", root.GetProperty("session").GetString());
        Assert.Equal("source-generation", root.GetProperty("generation").GetString());
        Assert.Equal(5, root.EnumerateObject().Count());
        Assert.Equal("__CarbonLiveSession", LiveSessionContract.MarkerName);
    }

    [Theory]
    [InlineData("http://example.com:41234")]
    [InlineData("https://127.0.0.1:41234")]
    [InlineData("http://127.0.0.1:41234/path")]
    [InlineData("http://user:password@127.0.0.1:41234")]
    public void RejectsEndpointsThatCouldEscapeTheExactLoopbackServer(string endpoint)
    {
        Assert.Throws<InvalidOperationException>(() => LiveSessionContract.ValidateAndSerialize(
            new LiveSessionRequest(endpoint, "Project", "session", "generation"),
            JsonOptions));
    }

    [Theory]
    [InlineData("", "session", "generation")]
    [InlineData("Project", "", "generation")]
    [InlineData("Project", "session", "")]
    public void RejectsIncompleteContracts(string project, string session, string generation)
    {
        Assert.Throws<InvalidOperationException>(() => LiveSessionContract.ValidateAndSerialize(
            new LiveSessionRequest("http://localhost:41234", project, session, generation),
            JsonOptions));
    }
}
