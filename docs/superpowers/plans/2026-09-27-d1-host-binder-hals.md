# D1 — host-side binder services (execution brief)

**Goal:** a HAL implemented in host Rust registers with the real `servicemanager` and a guest
process finds and calls it over binder. This is the foundation every host HAL (allocator, composer,
audio, …) sits on, and the unblock for the display (D3) and `system_server` past DisplayManager.

**Gate (write first, watch fail):** `crates/omni-linux/tests/d1_host_service.rs` — start the real
`servicemanager` (as C1 does); from the host, `create_host_service("omni.echo", handler)` where the
handler returns its request bytes; register it; run `/system/bin/service call omni.echo 1 i32 42`
in a guest process and assert the reply carries 42. Also `service check omni.echo` → "found".

## Files

- `crates/omni-linux/src/binder.rs` — the host endpoint (below).
- `crates/omni-linux/tests/d1_host_service.rs` — the gate.

## binder.rs changes

1. `const HOST: ProcId = u64::MAX;` (proc ids for guests start at 1, so this never collides).

2. `State.host_services: HashMap<u64, Arc<dyn Fn(u32, &[u8]) -> Vec<u8> + Send + Sync>>` keyed by
   the host node's `ptr`. `next_host_ptr: u64` (start 1).

3. `Broker::create_host_service(handler) -> u64`: under the lock, `ptr = next_host_ptr; += 1`,
   insert `handler`, `local_node(HOST, ptr, ptr, 0)`, mark the node `held` (a host service lives as
   long as the broker). Return `ptr`.

4. Inline dispatch in `transaction()`, inserted **after** the `awaiting += 1` block and **before**
   the `let dead = …; st.queue(target_proc, …)` tail:
   ```rust
   if !reply && target_proc == HOST {
       let handler = st.host_services.get(&target_ptr).cloned();
       drop(st); // handlers must not hold the broker lock (they may allocate shm, etc.)
       let reply_data = handler.map_or_default(|h| h(code, &txn.data));
       let mut st = file.broker.state.lock();
       if !oneway {
           let r = Txn { reply: true, data: reply_data, offsets: vec![], fds: vec![], sg: vec![],
                         fda: vec![], from: None, oneway: false, target_ptr: 0, target_cookie: 0,
                         secctx: false, code: 0, flags: 0, sender_pid: 0, sender_euid: 0 };
           st.queue(file.id, Some(t.tid), Work::Txn(Box::new(r)));
       }
       st.queue(file.id, Some(t.tid), Work::Complete);
       return Ok(());
   }
   ```
   `target_ptr` for a handle already resolves to the node's `ptr` (set where `target_*` is chosen);
   confirm it is carried for the non-reply branch (add it there if not).

5. `Broker::host_transact(context, handle, code, data: &[u8], objects: &[FlatObject]) -> Vec<u8>`:
   the host as a binder client (for `addService`). Reuse the object-translation + routing tail of
   `transaction()` by extracting it into `State::route(sender, sender_tid, target_node, reply, code,
   flags, data, offsets, objects_as_fds_and_binders) `. `host_transact` builds `data`/`offsets`
   for the parcel, calls `route` with `sender = HOST`, then waits on a `Condvar` for the reply Txn
   queued to `(HOST, host_tid)` and returns its `data`. Deliver-to-HOST: in `queue`, when the target
   is HOST, move the Txn into a `host_inbox: Mutex<HashMap<i32, Txn>>` slot and notify, instead of a
   guest todo.

## servicemanager addService parcel (Android 15, `IServiceManager` AIDL)

`service` and `libbinder` send, over handle 0, transaction code = `addService` (**3** in the
current `IServiceManager.aidl` order: getService=1, checkService=2, addService=3, listServices=4).
Reply for `checkService`/`getService` is a `flat_binder_object` (the service handle) or nothing.

Parcel body for `addService(String name, IBinder service, boolean allowIsolated, int dumpPriority)`:
- header: `strict_mode_policy` i32 (0), `work_source` i32 (-1 = `IBinder.UNSET_WORKSOURCE`), then
  the interface token: i32 length + UTF-16LE of `"android.os.IServiceManager"` + NUL, 4-aligned.
  (Android 13+ also writes a header size marker; match what `libbinder` on this image writes —
  capture it from a real `service` call's `BC_TRANSACTION` in a trace and mirror the bytes.)
- `name`: i32 char count + UTF-16LE + NUL, 4-aligned.
- `service`: a `flat_binder_object` (`hdr.type = BINDER`, `flags = 0x7f | ACCEPTS_FDS`, `binder =
  ptr`, `cookie = cookie`), with its offset in the offsets array.
- `allowIsolated`: i32 (0).
- `dumpPriority`: i32 (`DUMP_FLAG_PRIORITY_DEFAULT` = 8).

**Do not hand-guess the header.** Trace a real `service` registering something (or `servicemanager`'s
own self-registration, already visible in C1) with `OMNI_SYSCALL_TRACE` + a dump of the
`BC_TRANSACTION` buffer, and reproduce the leading bytes exactly; the AIDL body after the token is
stable.

## Order of work

1. Gate (RED): host echo service + `service call` from a guest → fails (no host endpoint).
2. Host endpoint (steps 1–4) + `host_transact` (step 5).
3. `addService` parcel; register the echo service; gate GREEN.
4. Then D2 (`IAllocator`/`IMapper` over `shm`), D3 (`IComposer3` → the framebuffer).

## Why this shape

Handlers run off the broker lock, so a HAL can allocate `shm` or block without deadlocking the
driver. The host is just another binder participant (`HOST`), so nothing in the guest path changes
except the one `target_proc == HOST` branch — the C1/C2 gates stay green.
