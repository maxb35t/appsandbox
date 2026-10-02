//! Windows Hyper-V sockets (AF_HYPERV), hand-declared Winsock FFI.

use crate::duplex::Duplex;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::Duration;

type Socket = usize;
const INVALID_SOCKET: Socket = !0;
const AF_HYPERV: i32 = 34;
const SOCK_STREAM: i32 = 1;
const HV_PROTOCOL_RAW: i32 = 1;
const SOL_SOCKET: i32 = 0xffff;
const SO_RCVTIMEO: i32 = 0x1006;
const SD_SEND: i32 = 1;

#[repr(C)]
#[derive(Clone, Copy, PartialEq)]
pub struct Guid {
    pub d1: u32,
    pub d2: u16,
    pub d3: u16,
    pub d4: [u8; 8],
}

impl Guid {
    pub fn lower(self) -> String {
        format!(
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.d1, self.d2, self.d3, self.d4[0], self.d4[1], self.d4[2], self.d4[3], self.d4[4], self.d4[5], self.d4[6], self.d4[7]
        )
    }
}

#[repr(C)]
struct SockaddrHv {
    family: u16,
    reserved: u16,
    vm_id: Guid,
    service_id: Guid,
}

/// Listen for connections from any child VM.
pub const HV_GUID_CHILDREN: Guid = Guid { d1: 0x90db8b89, d2: 0x0d35, d3: 0x4f79, d4: [0x8c, 0xe9, 0x49, 0xea, 0x0a, 0xc8, 0xb7, 0xcd] };
/// The host, as seen from inside a VM.
pub const HV_GUID_PARENT: Guid = Guid { d1: 0xa42e7cda, d2: 0xd03f, d3: 0x480c, d4: [0x9c, 0xc2, 0xa4, 0xde, 0x20, 0xab, 0xb8, 0x78] };

/// App Sandbox service GUID for a Windows guest: a5b0cafe-<port>-4000-8000-000000000001.
pub fn windows_service(port: u16) -> Guid {
    Guid { d1: 0xa5b0cafe, d2: port, d3: 0x4000, d4: [0x80, 0, 0, 0, 0, 0, 0, 0x01] }
}
/// Service GUID a Linux guest's AF_VSOCK port maps to: <port>-facb-11e6-bd58-64006a7986d3.
pub fn linux_service(port: u16) -> Guid {
    Guid { d1: port as u32, d2: 0xfacb, d3: 0x11e6, d4: [0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3] }
}

#[link(name = "ws2_32")]
extern "system" {
    fn WSAStartup(version: u16, data: *mut u8) -> i32;
    fn socket(af: i32, ty: i32, protocol: i32) -> Socket;
    fn bind(s: Socket, name: *const SockaddrHv, len: i32) -> i32;
    fn listen(s: Socket, backlog: i32) -> i32;
    fn accept(s: Socket, addr: *mut SockaddrHv, len: *mut i32) -> Socket;
    fn connect(s: Socket, name: *const SockaddrHv, len: i32) -> i32;
    fn recv(s: Socket, buf: *mut u8, len: i32, flags: i32) -> i32;
    fn send(s: Socket, buf: *const u8, len: i32, flags: i32) -> i32;
    fn shutdown(s: Socket, how: i32) -> i32;
    fn closesocket(s: Socket) -> i32;
    fn setsockopt(s: Socket, level: i32, name: i32, val: *const u8, len: i32) -> i32;
    fn WSAGetLastError() -> i32;
}

pub fn init() -> io::Result<()> {
    let mut wsa = [0u8; 1024];
    // SAFETY: `wsa` is larger than WSADATA on every Windows target.
    if unsafe { WSAStartup(0x0202, wsa.as_mut_ptr()) } != 0 {
        return Err(io::Error::other("WSAStartup failed"));
    }
    Ok(())
}

fn last_error(what: &str) -> io::Error {
    // SAFETY: plain Winsock call.
    let code = unsafe { WSAGetLastError() };
    io::Error::new(io::Error::from_raw_os_error(code).kind(), format!("{what}: {}", io::Error::from_raw_os_error(code)))
}

fn sockaddr(vm_id: Guid, service: Guid) -> SockaddrHv {
    SockaddrHv { family: AF_HYPERV as u16, reserved: 0, vm_id, service_id: service }
}

struct Raw(Socket);
impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: owned socket, closed once.
        unsafe { closesocket(self.0) };
    }
}

fn new_socket() -> io::Result<Raw> {
    // SAFETY: plain Winsock call; result checked.
    let s = unsafe { socket(AF_HYPERV, SOCK_STREAM, HV_PROTOCOL_RAW) };
    if s == INVALID_SOCKET {
        return Err(last_error("socket(AF_HYPERV)"));
    }
    Ok(Raw(s))
}

pub struct Listener(Raw);

impl Listener {
    pub fn bind(vm_id: Guid, service: Guid) -> io::Result<Listener> {
        let s = new_socket()?;
        let a = sockaddr(vm_id, service);
        // SAFETY: `a` is a valid SOCKADDR_HV for the call.
        if unsafe { bind(s.0, &a, std::mem::size_of::<SockaddrHv>() as i32) } != 0 {
            return Err(last_error("bind"));
        }
        // SAFETY: bound socket owned by `s`.
        if unsafe { listen(s.0, 128) } != 0 {
            return Err(last_error("listen"));
        }
        Ok(Listener(s))
    }

    /// Returns the connection and the connecting VM's id.
    pub fn accept(&self) -> io::Result<(HvStream, Guid)> {
        let mut peer = sockaddr(HV_GUID_CHILDREN, HV_GUID_CHILDREN);
        let mut len = std::mem::size_of::<SockaddrHv>() as i32;
        // SAFETY: out-pointers sized for SOCKADDR_HV.
        let s = unsafe { accept(self.0 .0, &mut peer, &mut len) };
        if s == INVALID_SOCKET {
            return Err(last_error("accept"));
        }
        Ok((HvStream(Arc::new(Raw(s))), peer.vm_id))
    }
}

/// A connected Hyper-V socket; clones share the socket (one reader, one writer).
pub struct HvStream(Arc<Raw>);

impl HvStream {
    pub fn connect(vm_id: Guid, service: Guid) -> io::Result<HvStream> {
        let s = new_socket()?;
        let a = sockaddr(vm_id, service);
        // SAFETY: `a` is a valid SOCKADDR_HV for the call.
        if unsafe { connect(s.0, &a, std::mem::size_of::<SockaddrHv>() as i32) } != 0 {
            return Err(last_error("connect"));
        }
        Ok(HvStream(Arc::new(s)))
    }
}

impl Read for HvStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(i32::MAX as usize) as i32;
        // SAFETY: `buf` is valid for `len` bytes.
        let n = unsafe { recv(self.0 .0, buf.as_mut_ptr(), len, 0) };
        if n < 0 { Err(last_error("recv")) } else { Ok(n as usize) }
    }
}

impl Write for HvStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = buf.len().min(i32::MAX as usize) as i32;
        // SAFETY: `buf` is valid for `len` bytes.
        let n = unsafe { send(self.0 .0, buf.as_ptr(), len, 0) };
        if n < 0 { Err(last_error("send")) } else { Ok(n as usize) }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Duplex for HvStream {
    fn try_clone_box(&self) -> io::Result<Box<dyn Duplex>> {
        Ok(Box::new(HvStream(self.0.clone())))
    }
    fn shutdown_write(&self) {
        // SAFETY: owned socket; failure harmless.
        unsafe { shutdown(self.0 .0, SD_SEND) };
    }
    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        let ms: u32 = t.map(|d| d.as_millis().min(u32::MAX as u128) as u32).unwrap_or(0);
        // SAFETY: `ms` is a DWORD, as SO_RCVTIMEO expects on Windows.
        let r = unsafe { setsockopt(self.0 .0, SOL_SOCKET, SO_RCVTIMEO, &ms as *const u32 as *const u8, 4) };
        if r != 0 { Err(last_error("setsockopt")) } else { Ok(()) }
    }
}
