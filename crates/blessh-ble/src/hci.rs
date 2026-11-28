//! Raw HCI access to a Bluetooth controller: the adapter's state through
//! `HCIGETDEVINFO`, and a Read Local Version probe on a raw HCI socket. The
//! adapter watchdog uses these to tell a controller that stopped answering
//! from one that is down or gone.
//!
//! Linux only; the crate root includes the module for Linux builds.

use std::{
    io,
    mem::size_of,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    time::{Duration, Instant},
};

/// Bluetooth HCI transport, the protocol argument of an `AF_BLUETOOTH`
/// socket.
const BTPROTO_HCI: libc::c_int = 1;
/// HCI socket option level (`SOL_HCI`).
const SOL_HCI: libc::c_int = 0;
/// HCI socket option that installs a packet filter.
const HCI_FILTER: libc::c_int = 2;
/// The raw channel: the socket sends commands and receives events.
const HCI_CHANNEL_RAW: u16 = 0;
/// `_IOR('H', 211, int)` from `<bluetooth/hci.h>`.
const HCIGETDEVINFO: libc::c_ulong = 0x8004_48d3;
/// HCI packet type of an event packet.
const HCI_EVENT_PKT: u8 = 4;
/// HCI event code of a Command Complete event.
const EVT_CMD_COMPLETE: u8 = 0x0e;
/// The `HCI_UP` bit of `hci_dev_info.flags`.
const HCI_UP: u32 = 1;

/// Read Local Version Information (OGF 0x04, OCF 0x0001): packet type
/// command, opcode, parameter length, no parameters.
pub const READ_VERSION_CMD: [u8; 4] = [0x01, 0x01, 0x10, 0x00];

/// What the kernel says about an adapter, without asking the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The adapter exists and is up.
    Up,
    /// The adapter exists but is down.
    Down,
    /// No adapter with that index exists.
    Absent,
}

/// The adapter index of an interface name: `"hci0"` gives `Some(0)`.
pub fn dev_id(name: &str) -> Option<u16> {
    let index = name.strip_prefix("hci")?;
    if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    index.parse().ok()
}

/// Reads the adapter's state with `HCIGETDEVINFO`. The ioctl asks the
/// kernel, not the controller, and needs no privilege.
pub fn state(dev: u16) -> io::Result<State> {
    let sock = hci_socket()?;
    // The kernel takes `dev_id` from the structure and fills in the rest.
    // SAFETY: the structure is plain data and all-zero is a valid value.
    let mut info: HciDevInfo = unsafe { std::mem::zeroed() };
    info.dev_id = dev;
    // SAFETY: `info` is writable for the size the ioctl copies back.
    let r = unsafe {
        libc::ioctl(
            sock.as_raw_fd(),
            HCIGETDEVINFO,
            std::ptr::addr_of_mut!(info).cast::<libc::c_void>(),
        )
    };
    if r < 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ENODEV) => Ok(State::Absent),
            _ => Err(e),
        };
    }
    Ok(if info.flags & HCI_UP != 0 {
        State::Up
    } else {
        State::Down
    })
}

/// Sends Read Local Version Information on a raw HCI socket bound to `dev`
/// and waits for its Command Complete. `Ok(true)` means the controller
/// answered, `Ok(false)` that it did not within `timeout`; errors are socket
/// failures, including `EPERM` without `CAP_NET_RAW`.
///
/// Blocking; callers use `spawn_blocking`.
pub fn read_version(dev: u16, timeout: Duration) -> io::Result<bool> {
    let sock = hci_socket()?;
    let fd = sock.as_raw_fd();

    let addr = SockaddrHci {
        hci_family: libc::AF_BLUETOOTH as u16,
        hci_dev: dev,
        hci_channel: HCI_CHANNEL_RAW,
    };
    // SAFETY: `addr` is an initialized sockaddr_hci of the given length.
    let r = unsafe {
        libc::bind(
            fd,
            std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
            size_of::<SockaddrHci>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }

    let filter = HciFilter {
        type_mask: 1 << HCI_EVENT_PKT,
        event_mask: [1 << EVT_CMD_COMPLETE, 0],
        opcode: 0,
    };
    // SAFETY: `filter` is an initialized hci_filter of the given length.
    let r = unsafe {
        libc::setsockopt(
            fd,
            SOL_HCI,
            HCI_FILTER,
            std::ptr::addr_of!(filter).cast::<libc::c_void>(),
            size_of::<HciFilter>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: the command consists of its four initialized bytes.
    let n = unsafe {
        libc::write(
            fd,
            READ_VERSION_CMD.as_ptr().cast::<libc::c_void>(),
            READ_VERSION_CMD.len(),
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != READ_VERSION_CMD.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short write to the HCI socket",
        ));
    }

    wait_complete(fd, timeout)
}

/// Whether `packet` is the Command Complete event for Read Local Version
/// Information (opcode 0x1001).
pub fn is_version_complete(packet: &[u8]) -> bool {
    packet.len() >= 6
        && packet[0] == HCI_EVENT_PKT
        && packet[1] == EVT_CMD_COMPLETE
        && packet[4..6] == READ_VERSION_CMD[1..3]
}

/// Polls `fd` for events until the Command Complete for Read Local Version
/// arrives or `timeout` passes, ignoring events for other commands.
fn wait_complete(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 2 + u8::MAX as usize];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let millis = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd entry.
        let r = unsafe { libc::poll(&mut pfd, 1, millis) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if r == 0 {
            return Ok(false);
        }
        // SAFETY: `buf` is writable for its whole length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if is_version_complete(&buf[..n as usize]) {
            return Ok(true);
        }
    }
}

/// Opens a raw `BTPROTO_HCI` socket.
fn hci_socket() -> io::Result<OwnedFd> {
    // SAFETY: socket(2) takes no memory arguments; the descriptor is checked
    // below and nothing else owns it.
    let fd = unsafe {
        libc::socket(
            libc::AF_BLUETOOTH,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            BTPROTO_HCI,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor that nothing else owns or closes.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `struct sockaddr_hci` from `<bluetooth/hci.h>`. The kernel reads the
/// fields; none is read back here.
#[repr(C)]
struct SockaddrHci {
    hci_family: u16,
    hci_dev: u16,
    hci_channel: u16,
}

/// `struct hci_filter` from `<bluetooth/hci.h>`: pass only Command Complete
/// events. The kernel reads the fields; none is read back here.
#[repr(C)]
struct HciFilter {
    type_mask: u32,
    event_mask: [u32; 2],
    opcode: u16,
}

/// `struct hci_dev_info` from `<bluetooth/hci.h>`. `dev_id` goes in and
/// `flags` comes out; the kernel fills the rest of the layout.
#[repr(C)]
struct HciDevInfo {
    dev_id: u16,
    name: [u8; 8],
    bdaddr: [u8; 6],
    flags: u32,
    type_: u8,
    features: [u8; 8],
    pkt_type: u32,
    link_policy: u32,
    link_mode: u32,
    acl_mtu: u16,
    acl_pkts: u16,
    sco_mtu: u16,
    sco_pkts: u16,
    stat: [u32; 10],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_id_parses_hci_names() {
        assert_eq!(dev_id("hci0"), Some(0));
        assert_eq!(dev_id("hci12"), Some(12));
        assert_eq!(dev_id("hci"), None);
        assert_eq!(dev_id("eth0"), None);
    }

    #[test]
    fn version_complete_matches_only_its_opcode() {
        // type 0x04 event, 0x0e Command Complete, plen, ncmd, opcode 0x1001 LE, status 0
        assert!(is_version_complete(&[
            0x04, 0x0e, 0x0c, 0x01, 0x01, 0x10, 0x00, 8, 0x12, 4, 8, 0x0f, 0, 9, 0x22
        ]));
        assert!(!is_version_complete(&[
            0x04, 0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00
        ])); // Reset's opcode
        assert!(!is_version_complete(&[
            0x04, 0x0f, 0x04, 0x00, 0x01, 0x01, 0x10
        ])); // Command Status
        assert!(!is_version_complete(&[0x04, 0x0e])); // short
    }
}
