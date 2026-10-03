//! Linux AF_VSOCK client (guest side), hand-declared libc FFI.

use crate::duplex::Duplex;
use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

const AF_VSOCK: i32 = 40;
const SOCK_STREAM: i32 = 1;
const SOCK_CLOEXEC: i32 = 0o2000000;
const VMADDR_CID_HOST: u32 = 2;
const SOL_SOCKET: i32 = 1;
const SO_RCVTIMEO: i32 = 20;
const SHUT_WR: i32 = 1;

#[repr(C)]
struct SockaddrVm {
    family: u16,
    reserved1: u16,
    port: u32,
    cid: u32,
    zero: [u8; 4],
}

#[repr(C)]
struct Timeval {
    sec: i64,
    usec: i64,
}

extern "C" {
    fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
    fn connect(fd: i32, addr: *const SockaddrVm, len: u32) -> i32;
    fn read(fd: i32, buf: *mut c_void, n: usize) -> isize;
    fn write(fd: i32, buf: *const c_void, n: usize) -> isize;
    fn shutdown(fd: i32, how: i32) -> i32;
    fn setsockopt(fd: i32, level: i32, name: i32, val: *const u8, len: u32) -> i32;
}

pub struct VsockStream(Arc<OwnedFd>);

impl VsockStream {
    /// Connect to the host on AF_VSOCK `port` (App Sandbox maps it to the
    /// <port>-facb-11e6-bd58-64006a7986d3 Hyper-V service).
    pub fn connect_host(port: u32) -> io::Result<VsockStream> {
        // SAFETY: plain syscall; result checked.
        let fd = unsafe { socket(AF_VSOCK, SOCK_STREAM | SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor we own.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let a = SockaddrVm { family: AF_VSOCK as u16, reserved1: 0, port, cid: VMADDR_CID_HOST, zero: [0; 4] };
        // SAFETY: `a` is a valid sockaddr_vm for the call.
        if unsafe { connect(fd, &a, std::mem::size_of::<SockaddrVm>() as u32) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(VsockStream(Arc::new(owned)))
    }
}

impl Read for VsockStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is valid for its length.
        let n = unsafe { read(self.0.as_raw_fd(), buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
    }
}

impl Write for VsockStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: `buf` is valid for its length.
        let n = unsafe { write(self.0.as_raw_fd(), buf.as_ptr() as *const c_void, buf.len()) };
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Duplex for VsockStream {
    fn try_clone_box(&self) -> io::Result<Box<dyn Duplex>> {
        Ok(Box::new(VsockStream(self.0.clone())))
    }
    fn shutdown_write(&self) {
        // SAFETY: owned descriptor; failure harmless.
        unsafe { shutdown(self.0.as_raw_fd(), SHUT_WR) };
    }
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        let d = t.unwrap_or(Duration::ZERO);
        let tv = Timeval { sec: d.as_secs() as i64, usec: d.subsec_micros() as i64 };
        // SAFETY: `tv` is a valid timeval for the call.
        let r = unsafe { setsockopt(self.0.as_raw_fd(), SOL_SOCKET, SO_RCVTIMEO, &tv as *const Timeval as *const u8, std::mem::size_of::<Timeval>() as u32) };
        if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }
}
