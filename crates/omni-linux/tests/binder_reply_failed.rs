//! A reply that cannot be made takes the transaction it answers off the replier once: its caller
//! hears BR_FAILED_REPLY, and the replier's outer transactions are still answered. The reply's
//! transaction was taken off twice, so a thread serving two nested calls from one caller lost the
//! outer one, and its reply went nowhere -- the caller waited forever. One test per file: the
//! broker is per host process.
mod binder_guest;

use binder_guest::*;

const TYPE_FD: u32 = 0x6664_2a85;

/// Read until there is nothing more for `t`.
fn drain(g: &Guest, t: &mut omni_linux::Task) -> Vec<Br> {
    let mut all = Vec::new();
    for _ in 0..16 {
        let got = g.read(t, &[]);
        if got.is_empty() {
            break;
        }
        all.extend(got);
    }
    all
}

#[test]
fn a_failed_nested_reply_leaves_the_outer_call_answerable() {
    let mut server = Guest::new();
    let mut r = server.looper();
    server.set_context_mgr(&mut r);
    let mut client = Guest::new();
    let mut c = client.thread();

    // The client calls the server, handing it one of its objects.
    let call = client.transaction(BC_TRANSACTION, 0, 1, 0, &object(TYPE_BINDER, 0x700, 0x701), &[0]);
    assert!(cmds(&[client.read(&mut c, &call), drain(&client, &mut c)].concat()).contains(&BR_TRANSACTION_COMPLETE));
    let outer = server.read(&mut r, &[]);
    assert_eq!(codes(&outer), [1]);
    let back = u32::from_le_bytes(outer[0].data[8..12].try_into().unwrap());

    // Serving it, the server calls the client back; serving that, the client calls the server
    // again: the server's thread now serves two calls from the client's.
    let nested = server.transaction(BC_TRANSACTION, back, 2, 0, b"n", &[]);
    assert_eq!(cmds(&server.read(&mut r, &nested)), [BR_TRANSACTION_COMPLETE]);
    assert_eq!(codes(&drain(&client, &mut c)), [2]);
    let inner_call = client.transaction(BC_TRANSACTION, 0, 3, 0, b"i", &[]);
    assert_eq!(cmds(&client.read(&mut c, &inner_call)), [BR_TRANSACTION_COMPLETE]);
    assert_eq!(codes(&drain(&server, &mut r)), [3]);

    // The server's reply to the inner call cannot be made (a descriptor it does not have).
    let mut fd = [0u8; 24];
    fd[0..4].copy_from_slice(&TYPE_FD.to_le_bytes());
    fd[8..12].copy_from_slice(&999u32.to_le_bytes());
    let bad = server.transaction(BC_REPLY, 0, 0, 0, &fd, &[0]);
    assert_eq!(cmds(&server.read(&mut r, &bad)), [BR_TRANSACTION_COMPLETE]);
    assert_eq!(cmds(&drain(&client, &mut c)), [BR_FAILED_REPLY], "the inner call fails");

    // The client answers the server's call; the server answers the outer call.
    let answer = client.transaction(BC_REPLY, 0, 0, 0, b"a", &[]);
    assert_eq!(client.write_read(&mut c, &answer, false).0, 0);
    let got = drain(&server, &mut r);
    assert!(cmds(&got).contains(&BR_REPLY), "{got:x?}");
    let last = server.transaction(BC_REPLY, 0, 0, 0, b"outer", &[]);
    assert_eq!(server.write_read(&mut r, &last, false).0, 0);
    let got = drain(&client, &mut c);
    let reply = got.iter().find(|b| b.cmd == BR_REPLY).unwrap_or_else(|| panic!("the outer call is answered: {got:x?}"));
    assert_eq!(reply.data, b"outer");
}
