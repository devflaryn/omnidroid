//! The kernel's side of a netlink socket: every request is answered as a kernel with one network
//! interface, the loopback, answers it. Dumps list `lo` where a dump would (links) and are
//! otherwise empty; changes (routes, rules, netfilter logging, xfrm) are acknowledged -- no packet
//! flows here for them to act on. netd configures its routing and logging so at boot.
use crate::errno::Errno;
use crate::socket::Socket;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_MULTI: u16 = 2;
const NLM_F_ACK: u16 = 4;
const NLM_F_DUMP: u16 = 0x300;

const NETLINK_ROUTE: u64 = 0;
const NETLINK_GENERIC: u64 = 16;
const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;

/// The loopback's `RTM_NEWLINK`: `ifinfomsg` (family, type ARPHRD_LOOPBACK, index 1, flags
/// UP|LOOPBACK|RUNNING) and its name and MTU.
fn loopback(seq: u32, port: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&772u16.to_le_bytes());
    body.extend_from_slice(&1i32.to_le_bytes());
    body.extend_from_slice(&(0x1u32 | 0x8 | 0x40).to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    let attr = |body: &mut Vec<u8>, kind: u16, data: &[u8]| {
        let len = 4 + data.len();
        body.extend_from_slice(&(len as u16).to_le_bytes());
        body.extend_from_slice(&kind.to_le_bytes());
        body.extend_from_slice(data);
        while body.len() % 4 != 0 {
            body.push(0);
        }
    };
    attr(&mut body, 3, b"lo\0"); // IFLA_IFNAME
    attr(&mut body, 4, &65536u32.to_le_bytes()); // IFLA_MTU
    message(RTM_NEWLINK, NLM_F_MULTI, seq, port, &body)
}

fn message(kind: u16, flags: u16, seq: u32, port: u32, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(16 + body.len());
    m.extend_from_slice(&((16 + body.len()) as u32).to_le_bytes());
    m.extend_from_slice(&kind.to_le_bytes());
    m.extend_from_slice(&flags.to_le_bytes());
    m.extend_from_slice(&seq.to_le_bytes());
    m.extend_from_slice(&port.to_le_bytes());
    m.extend_from_slice(body);
    m
}

/// `NLMSG_ERROR` with `error` (0: the acknowledgement) and the request's header.
fn error(request: &[u8], seq: u32, port: u32, error: i32) -> Vec<u8> {
    let mut body = error.to_le_bytes().to_vec();
    body.extend_from_slice(&request[..16]);
    message(NLMSG_ERROR, 0, seq, port, &body)
}

/// A message sent to the kernel: the answers are queued for the socket to read.
pub fn send(socket: &mut Socket, bytes: &[u8], port: u32) -> Result<usize, Errno> {
    let mut at = 0;
    while at + 16 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4")) as usize;
        if len < 16 || at + len > bytes.len() {
            break;
        }
        let msg = &bytes[at..at + len];
        let kind = u16::from_le_bytes([msg[4], msg[5]]);
        let flags = u16::from_le_bytes([msg[6], msg[7]]);
        let seq = u32::from_le_bytes(msg[8..12].try_into().expect("4"));
        let mut reply = Vec::new();
        if flags & NLM_F_DUMP == NLM_F_DUMP {
            if socket.protocol == NETLINK_ROUTE && kind == RTM_GETLINK {
                reply.extend(loopback(seq, port));
            }
            reply.extend(message(NLMSG_DONE, NLM_F_MULTI, seq, port, &0i32.to_le_bytes()));
        } else if socket.protocol == NETLINK_ROUTE && kind >= RTM_NEWLINK && kind % 4 == 2 {
            // A single RTM_GET*: nothing by that name here.
            reply.extend(error(msg, seq, port, -19)); // ENODEV
        } else if socket.protocol == NETLINK_GENERIC && kind == 0x10 {
            // CTRL_CMD_GETFAMILY: no generic netlink family is registered.
            reply.extend(error(msg, seq, port, -2));
        } else if flags & NLM_F_ACK != 0 {
            reply.extend(error(msg, seq, port, 0));
        }
        socket.inbox.extend(reply);
        at += len.div_ceil(4) * 4;
    }
    crate::poll::notify();
    Ok(bytes.len())
}
