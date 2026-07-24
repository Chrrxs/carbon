use std::time::Duration;

/// Maximum bounded MessagePack request size accepted by the server.
pub const MAX_PAYLOAD_SIZE: usize = 16 * 1024 * 1024;

/// Long-poll timeout for subscribed Studio clients.
pub const QUEUE_TIMEOUT: Duration = Duration::from_secs(60);
