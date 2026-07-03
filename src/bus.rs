//! Realtime event bus — a unix datagram socket replacing Postgres
//! LISTEN/NOTIFY after the SQLite port.
//!
//! Topology: the scheduler (the sole long-lived listener) binds the socket;
//! every notifier — CLI one-shots, hooks — sends a fire-and-forget JSON
//! datagram `{"channel": "...", "payload": "..."}`. Senders silently no-op
//! when the socket is absent (no scheduler running), exactly mirroring the
//! dropped-NOTIFY caveat of the Postgres era: the scheduler's periodic tick
//! remains the delivery safety net, so a lost datagram only costs one tick
//! of latency.
//!
//! Socket path: `$XDG_RUNTIME_DIR/ygg.sock`, falling back to
//! `/tmp/ygg-$UID.sock`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusEvent {
    pub channel: String,
    #[serde(default)]
    pub payload: String,
}

/// Unix sockets cap `sun_path` at ~108 bytes; leave headroom.
const MAX_SOCKET_PATH: usize = 100;

/// Resolve the bus socket path: `$XDG_RUNTIME_DIR/ygg.sock`, falling back to
/// `/tmp/ygg-$UID.sock` (also used when the XDG path would exceed the unix
/// socket path limit — both sender and listener resolve identically).
pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        let p = PathBuf::from(dir).join("ygg.sock");
        if p.as_os_str().len() <= MAX_SOCKET_PATH {
            return p;
        }
    }
    let uid = unsafe { libc_geteuid() };
    PathBuf::from(format!("/tmp/ygg-{uid}.sock"))
}

/// `geteuid` without pulling in the libc crate.
unsafe fn libc_geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

/// Fire-and-forget notification. Never blocks, never errors: if the
/// scheduler isn't listening (socket absent, buffer full, …) the datagram is
/// simply dropped — the scheduler's tick picks the work up anyway.
pub fn notify(channel: &str, payload: &str) {
    let path = socket_path();
    if !path.exists() {
        return;
    }
    let msg = match serde_json::to_vec(&BusEvent {
        channel: channel.to_string(),
        payload: payload.to_string(),
    }) {
        Ok(m) => m,
        Err(_) => return,
    };
    if let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() {
        sock.set_nonblocking(true).ok();
        sock.send_to(&msg, &path).ok();
    }
}

/// The scheduler's listening end. Binding removes any stale socket file
/// first — safe because the caller already holds the scheduler singleton
/// file lock, so no other listener can be alive.
pub struct BusListener {
    sock: tokio::net::UnixDatagram,
}

impl BusListener {
    pub fn bind() -> std::io::Result<Self> {
        let path = socket_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        // Stale socket from a previous scheduler that died without cleanup.
        std::fs::remove_file(&path).ok();
        let sock = tokio::net::UnixDatagram::bind(&path)?;
        Ok(Self { sock })
    }

    /// Await the next datagram. Malformed payloads still count as a wake-up
    /// (returned as a bare event on channel "unknown") — the scheduler only
    /// uses arrival as a tick trigger.
    pub async fn recv(&self) -> BusEvent {
        let mut buf = [0u8; 4096];
        loop {
            match self.sock.recv(&mut buf).await {
                Ok(n) => {
                    return serde_json::from_slice(&buf[..n]).unwrap_or(BusEvent {
                        channel: "unknown".to_string(),
                        payload: String::new(),
                    });
                }
                Err(_) => {
                    // Transient recv error — back off briefly and keep
                    // listening rather than tearing the bus down.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }
}

impl Drop for BusListener {
    fn drop(&mut self) {
        std::fs::remove_file(socket_path()).ok();
    }
}
