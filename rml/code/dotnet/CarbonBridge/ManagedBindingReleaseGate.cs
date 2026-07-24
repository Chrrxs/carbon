namespace Carbon.RmlBridge;

internal sealed class ManagedBindingReleaseGate
{
    private readonly object _lock = new();
    private readonly Dictionary<string, (long Token, string DebugId)> _pending = new(StringComparer.Ordinal);
    private long _nextToken;

    public long Schedule(string runtimeId, string debugId)
    {
        lock (_lock)
        {
            var token = ++_nextToken;
            _pending[runtimeId] = (token, debugId);
            return token;
        }
    }

    public void Cancel(string runtimeId)
    {
        lock (_lock)
        {
            _pending.Remove(runtimeId);
        }
    }

    public bool Cancel(string runtimeId, string debugId)
    {
        lock (_lock)
        {
            if (!_pending.TryGetValue(runtimeId, out var pending)
                || !string.Equals(pending.DebugId, debugId, StringComparison.Ordinal))
            {
                return false;
            }
            _pending.Remove(runtimeId);
            return true;
        }
    }

    public bool HasPending(string runtimeId)
    {
        lock (_lock)
        {
            return _pending.ContainsKey(runtimeId);
        }
    }

    public bool Complete(string runtimeId, long token)
    {
        lock (_lock)
        {
            if (!_pending.TryGetValue(runtimeId, out var current) || current.Token != token)
            {
                return false;
            }
            _pending.Remove(runtimeId);
            return true;
        }
    }

    public void Clear()
    {
        lock (_lock)
        {
            _pending.Clear();
        }
    }
}
