//! Notify systemd only after the owning process is ready to serve requests.
use anyhow::Result;
use std::{
    ffi::OsStr,
    os::{
        linux::net::SocketAddrExt,
        unix::{
            ffi::OsStrExt,
            net::{SocketAddr, UnixDatagram},
        },
    },
};

pub fn ready() -> Result<()> {
    if let Some(socket) = std::env::var_os("NOTIFY_SOCKET") {
        notify(&socket)?;
    }
    Ok(())
}

fn notify(socket: &OsStr) -> Result<()> {
    let bytes = socket.as_bytes();
    let target = if bytes.first() == Some(&b'@') {
        SocketAddr::from_abstract_name(&bytes[1..])?
    } else {
        SocketAddr::from_pathname(socket)?
    };
    UnixDatagram::unbound()?.send_to_addr(b"READY=1", &target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readiness_reaches_the_manager_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let receiver = UnixDatagram::bind(&path).unwrap();
        receiver.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
        notify(path.as_os_str()).unwrap();
        let mut buffer = [0; 32];
        let n = receiver.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], b"READY=1");
    }
    #[test]
    fn missing_manager_socket_is_an_explicit_failure() {
        let dir = tempfile::tempdir().unwrap();
        assert!(notify(dir.path().join("missing").as_os_str()).is_err());
    }
}
