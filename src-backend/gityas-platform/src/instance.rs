//! Single instance: only one GitYas process runs per data directory.
//!
//! The first process takes an exclusive OS lock on `<data_dir>/gityas.lock` (released by the OS when the process
//! exits, crashes included) and listens on a loopback port, which it writes to `<data_dir>/gityas.port`. A later
//! launch fails to take the lock, connects to that port to ask the running window to come to the front, and exits.
//! The port is in a separate file because a locked file cannot be read on Windows.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

use gityas_core::{GitYasError, Result};

pub const LOCK_FILE: &str = "gityas.lock";
pub const PORT_FILE: &str = "gityas.port";

/// Sent by a second launch; anything else on the socket is ignored.
const ACTIVATE: &[u8] = b"gityas-activate\n";
const IO_TIMEOUT: Duration = Duration::from_millis(500);
/// The running instance writes the port file just after taking the lock; a launch in that window retries briefly.
const PORT_RETRIES: u32 = 10;
const PORT_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Result of [`claim_instance`].
#[derive(Debug)]
pub enum Instance {
    /// This process is the only instance; keep the lock alive until the app exits.
    Primary(InstanceLock),
    /// Another instance holds the lock. `activated`: it was asked to show its window.
    Secondary { activated: bool },
}

/// Held by the running instance. Dropping it releases the lock (do so before starting a replacement process).
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
    listener: TcpListener,
}

impl InstanceLock {
    /// Calls `on_activate` (on a background thread) each time another launch asks this instance to show itself.
    pub fn on_activate(&self, on_activate: impl Fn() + Send + 'static) -> Result<()> {
        let listener = self.listener.try_clone()?;
        std::thread::Builder::new()
            .name("gityas-instance".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(mut stream) => {
                            if is_activate(&mut stream) {
                                on_activate();
                            }
                        }
                        Err(err) => tracing::debug!("single-instance listener: {err}"),
                    }
                }
            })
            .map_err(|err| GitYasError::Io(format!("cannot start the single-instance listener: {err}")))?;
        Ok(())
    }
}

/// Takes the single-instance lock in `data_dir`, or, if another process holds it, asks that process to show its
/// window.
pub fn claim_instance(data_dir: &Path) -> Result<Instance> {
    let lock_path = data_dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|err| GitYasError::Io(format!("cannot open {}: {err}", lock_path.display())))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Ok(Instance::Secondary {
                activated: activate_running(data_dir),
            });
        }
        Err(TryLockError::Error(err)) => {
            return Err(GitYasError::Io(format!("cannot lock {}: {err}", lock_path.display())));
        }
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let port_path = data_dir.join(PORT_FILE);
    std::fs::write(&port_path, port.to_string()).map_err(|err| GitYasError::Io(format!("cannot write {}: {err}", port_path.display())))?;
    Ok(Instance::Primary(InstanceLock { _file: file, listener }))
}

/// Asks the instance holding the lock to show its window; `false` if it could not be reached.
fn activate_running(data_dir: &Path) -> bool {
    let port_path = data_dir.join(PORT_FILE);
    for attempt in 0..PORT_RETRIES {
        if attempt > 0 {
            std::thread::sleep(PORT_RETRY_DELAY);
        }
        let Some(port) = std::fs::read_to_string(&port_path).ok().and_then(|s| s.trim().parse::<u16>().ok()) else {
            continue;
        };
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        if let Ok(mut stream) = TcpStream::connect_timeout(&addr, IO_TIMEOUT)
            && stream.set_write_timeout(Some(IO_TIMEOUT)).is_ok()
            && stream.write_all(ACTIVATE).is_ok()
        {
            return true;
        }
    }
    false
}

fn is_activate(stream: &mut TcpStream) -> bool {
    if stream.set_read_timeout(Some(IO_TIMEOUT)).is_err() {
        return false;
    }
    let mut buf = [0u8; ACTIVATE.len()];
    stream.read_exact(&mut buf).is_ok() && buf == ACTIVATE
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn second_claim_activates_the_first_and_the_lock_is_released_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let Instance::Primary(lock) = claim_instance(tmp.path()).unwrap() else {
            panic!("first claim must be primary");
        };
        let (tx, rx) = mpsc::channel();
        lock.on_activate(move || {
            let _ = tx.send(());
        })
        .unwrap();

        let second = claim_instance(tmp.path()).unwrap();
        assert!(matches!(second, Instance::Secondary { activated: true }), "{second:?}");
        rx.recv_timeout(Duration::from_secs(5)).unwrap();

        drop(lock);
        assert!(matches!(claim_instance(tmp.path()).unwrap(), Instance::Primary(_)));
    }

    #[test]
    fn unreachable_instance_is_reported_not_activated() {
        let tmp = tempfile::tempdir().unwrap();
        let Instance::Primary(_lock) = claim_instance(tmp.path()).unwrap() else {
            panic!("first claim must be primary");
        };
        std::fs::write(tmp.path().join(PORT_FILE), "not a port").unwrap();

        let second = claim_instance(tmp.path()).unwrap();
        assert!(matches!(second, Instance::Secondary { activated: false }), "{second:?}");
    }

    #[test]
    fn foreign_connections_do_not_activate() {
        let tmp = tempfile::tempdir().unwrap();
        let Instance::Primary(lock) = claim_instance(tmp.path()).unwrap() else {
            panic!("first claim must be primary");
        };
        let (tx, rx) = mpsc::channel();
        lock.on_activate(move || {
            let _ = tx.send(());
        })
        .unwrap();
        let addr = lock.listener.local_addr().unwrap();
        TcpStream::connect(addr).unwrap().write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();

        assert!(rx.recv_timeout(Duration::from_secs(2)).is_err());
    }
}
