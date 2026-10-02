//! A bidirectional byte stream that can be split across two threads.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

pub trait Duplex: Read + Write + Send {
    fn try_clone_box(&self) -> io::Result<Box<dyn Duplex>>;
    fn shutdown_write(&self);
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()>;
}

impl Duplex for TcpStream {
    fn try_clone_box(&self) -> io::Result<Box<dyn Duplex>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, t)
    }
}

/// Copies a -> b until EOF, error or idle timeout; returns bytes copied.
pub fn pump(a: &mut dyn Duplex, b: &mut dyn Duplex) -> u64 {
    let mut buf = [0u8; 16 * 1024];
    let mut total = 0u64;
    loop {
        let n = match a.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if b.write_all(&buf[..n]).is_err() {
            break;
        }
        total += n as u64;
    }
    b.shutdown_write();
    total
}
