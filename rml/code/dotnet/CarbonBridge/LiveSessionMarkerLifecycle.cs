namespace Carbon.RmlBridge;

internal static class LiveSessionMarkerLifecycle
{
    internal static void Replace<T>(ref T? slot, Action<T> destroy, Func<T> create)
        where T : class
    {
        var previous = slot;
        slot = null;
        if (previous is not null)
        {
            destroy(previous);
        }
        slot = create();
    }
}
