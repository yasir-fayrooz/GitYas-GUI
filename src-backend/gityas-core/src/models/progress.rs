//! Progress of long-running remote operations (fetch / pull / push / clone), pushed as
//! [`AppEvent::RemoteProgress`](crate::AppEvent::RemoteProgress) for the status bar. Transient: no serde.

/// Which remote operation a progress report belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoteOp {
    Fetch,
    Pull,
    Push,
    Clone,
}

/// One progress report. `percent` is `None` while the total is unknown (e.g. before the server sends a count).
/// `message` is display text (object counts or the server's sideband output, decoded lossily).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProgress {
    pub op: RemoteOp,
    pub percent: Option<u8>,
    pub message: String,
}
