using System.Text.Json;

namespace Carbon.RmlBridge;

internal sealed record LiveSessionRequest(
    string Endpoint,
    string Project,
    string Session,
    string Generation);

internal static class LiveSessionContract
{
    internal const int ProtocolVersion = 1;
    internal const string MarkerName = "__CarbonLiveSession";

    internal static string ValidateAndSerialize(
        LiveSessionRequest request,
        JsonSerializerOptions jsonOptions)
    {
        ArgumentNullException.ThrowIfNull(request);
        ArgumentNullException.ThrowIfNull(jsonOptions);

        if (!Uri.TryCreate(request.Endpoint, UriKind.Absolute, out var endpoint)
            || endpoint.Scheme != Uri.UriSchemeHttp
            || (endpoint.Host != "127.0.0.1" && endpoint.Host != "localhost")
            || endpoint.Port is <= 0 or > 65535
            || endpoint.AbsolutePath != "/"
            || !string.IsNullOrEmpty(endpoint.Query)
            || !string.IsNullOrEmpty(endpoint.Fragment)
            || !string.IsNullOrEmpty(endpoint.UserInfo))
        {
            throw new InvalidOperationException(
                "live session endpoint must be an uncredentialed loopback HTTP origin");
        }
        if (string.IsNullOrWhiteSpace(request.Project))
        {
            throw new InvalidOperationException("live session project is empty");
        }
        if (string.IsNullOrWhiteSpace(request.Session))
        {
            throw new InvalidOperationException("live session token is empty");
        }
        if (string.IsNullOrWhiteSpace(request.Generation))
        {
            throw new InvalidOperationException("live session generation is empty");
        }

        return JsonSerializer.Serialize(new
        {
            protocolVersion = ProtocolVersion,
            endpoint = request.Endpoint,
            project = request.Project,
            session = request.Session,
            generation = request.Generation,
        }, jsonOptions);
    }
}
