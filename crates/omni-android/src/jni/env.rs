//! The `JNIEnv` and `JavaVM` handlers: the 59 slots the engine uses, and the 174 that refuse.
//!
//! # One handler, dispatched by slot, and why not 233 of them
//!
//! [`ImportFn`](crate::ImportFn) is a bare `fn` pointer, so a handler cannot be handed its slot
//! index. What it *can* have is its own thunk address, and [`Jni::env_slot_of`] turns that back
//! into the index — the same trick [`inline_trampoline`](crate::Boundary) uses one level down to
//! turn one `fn` into 170 symbols.
//!
//! # Which slots take the exit path
//!
//! [`is_reentrant`] answers it, and the rule is *can this reach guest code*: the whole
//! `Call…Method…` family, `NewObject…`, `AllocObject` and `RegisterNatives`. A Java method may be
//! `native`, in which case calling it means calling back into the guest, and
//! [`ImportCall`] structurally cannot (D18). Nothing on §8 steps 6-12 takes
//! that path — the Java side there is all host-defined — but the boundary is chosen by what the
//! call *may* do, not by what this milestone happens to need, because changing it later would
//! mean changing which dispatch path a live call site is on.
//!
//! D17 measures the exit path at 80-105 ns against ≈33 ns inline. The whole engine reaches the
//! `…MethodV` slots through **one ICF-merged call site each** (§2.1), so the cost lands on ten
//! sites, not on the 943.
//!
//! # Every unimplemented slot names itself
//!
//! `jni-surface.md` §8.1 ranks `FindClass` returning `NULL` where the caller does not check third
//! among the expected failure modes: it is a fatal abort thousands of instructions later. That is
//! the general shape of a plausible stub, so there are none here. A slot this layer does not
//! implement produces [`AbiError::JniRefused`] carrying its own name, and the guest's return
//! register is left alone.

use omni_cpu::RunLimit;
use omni_mem::GuestAddr;

use crate::abi::{Args, Ret};
use crate::boundary::{GuestArg, ImportCall, ReentrantCall};
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};
use crate::varargs::GuestVaList;

use super::classes::{Answer, ClassId, FieldId, Member, MethodId, Miss, Registry};
use super::refs::Handles;
use super::pool::PinKind;
use super::refs::{Object, ObjectId, RefKind};
use super::slots::{
    self, JNI_EDETACHED, JNI_FALSE, JNI_OK, JNI_TRUE, JNI_VERSION_1_6,
};
use super::values::{Descriptor, JavaString, TypeTag, Value};
use super::{Jni, JniState, Registration, ATTACH_ARGS_NAME_OFFSET};

/// Guest instructions a `native` Java method reached through `Call…Method…` is allowed.
///
/// A counted budget rather than [`RunLimit::Unlimited`], for D16's reason: a runaway guest is
/// contained by short windows, and a host-to-guest call with no bound would hang rather than
/// fail. Generous, because a registered native may do real work.
const NATIVE_METHOD_BUDGET: RunLimit = RunLimit::Instructions(200_000_000);

/// What a JNI function returns, before it is put in a register.
///
/// One type so that the inline path and the exit path share **one** marshaller. Two would be how
/// the two paths come to disagree about which register a `jdouble` goes in — the same argument
/// [`Ret`] itself rests on one level down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JniReturn {
    /// `void`.
    Void,
    /// An integer, a `jboolean`, a handle or a pointer, in `X0`.
    Word(u64),
    /// A `jint`, sign-extended into `X0`.
    Int(i32),
    /// A `jlong`.
    Long(i64),
    /// A `jfloat`, in `V0`.
    Float(f32),
    /// A `jdouble`, in `V0`.
    Double(f64),
}

impl JniReturn {
    fn write(self, mut ret: Ret<'_>) {
        match self {
            JniReturn::Void => ret.void(),
            JniReturn::Word(value) => ret.u64(value),
            JniReturn::Int(value) => ret.i32(value),
            // A `jlong` is 64 bits and every bit of it is significant, so it is written whole
            // rather than through `i32`.
            JniReturn::Long(value) => ret.u64(value as u64),
            JniReturn::Float(value) => ret.f32(value),
            JniReturn::Double(value) => ret.f64(value),
        }
    }
}

/// Whether slot `index` is serviced on the exit path. See the module docs.
#[must_use]
pub fn is_reentrant(index: usize) -> bool {
    let name = slots::ENV_SLOTS[index];
    name.starts_with("Call")
        || matches!(
            name,
            "NewObject" | "NewObjectV" | "NewObjectA" | "AllocObject" | "RegisterNatives"
        )
}

/// The `JNIEnv` slots serviced inside the run loop.
///
/// # Errors
///
/// Any [`AbiError`]; [`AbiError::JniRefused`] for a slot this layer does not implement, naming it.
pub fn inline_slot(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let address = call.address();
    let Some((jni, thread)) = super::active_opt() else {
        return Err(AbiError::JniNotActive { function: call.symbol().to_string(), address });
    };
    let Some(index) = jni.env_slot_of(address) else {
        return Err(AbiError::JniRefused {
            function: call.symbol().to_string(),
            address,
            detail: "this thunk address is not a JNINativeInterface slot of the active instance"
                .to_string(),
        });
    };
    let name = slots::ENV_SLOTS[index];
    let value = {
        let mem = call.mem();
        let mut args = call.args();
        // Argument 0 is the `JNIEnv*` on every one of them. Read and discarded rather than
        // skipped, so the cursor lands on argument 1 by the same rule every other handler uses.
        let _env = args.next_pointer()?;
        env_call(&jni, thread, name, address, &mut args, mem)?
    };
    value.write(call.ret());
    Ok(())
}

/// The `JNIEnv` slots serviced on the exit path, which may call guest code.
///
/// # Errors
///
/// As [`inline_slot`], plus anything a called `native` method raises.
pub fn reentrant_slot(call: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let address = call.address();
    let Some((jni, thread)) = super::active_opt() else {
        return Err(AbiError::JniNotActive { function: call.symbol().to_string(), address });
    };
    let Some(index) = jni.env_slot_of(address) else {
        return Err(AbiError::JniRefused {
            function: call.symbol().to_string(),
            address,
            detail: "this thunk address is not a JNINativeInterface slot of the active instance"
                .to_string(),
        });
    };
    let name = slots::ENV_SLOTS[index];
    let value = match name {
        "RegisterNatives" => {
            let mut args = call.args();
            let _env = args.next_pointer()?;
            let class = args.next_u64()?;
            let methods = args.next_pointer()?;
            let count = args.next_i32()?;
            register_natives(&jni, name, address, call.mem(), class, methods, count)?
        }
        _ if name.starts_with("Call") || name == "NewObjectV" => {
            call_method(&jni, thread, name, address, call)?
        }
        _ => return Err(refuse_slot(name, address)),
    };
    call.ret(|ret| value.write(ret));
    Ok(())
}

/// The three `JavaVM` slots the engine uses, and refusals for the other five.
///
/// # Errors
///
/// [`AbiError::JniRefused`] for `DestroyJavaVM`, `AttachCurrentThreadAsDaemon` and the three
/// reserved slots. §2.2 measured the engine's startup calling only `GetEnv` and
/// `AttachCurrentThread`; `DetachCurrentThread` is answered since the first game join, where
/// FMOD's threads call it on their way out (see its arm).
pub fn vm_slot(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let address = call.address();
    let Some((jni, thread)) = super::active_opt() else {
        return Err(AbiError::JniNotActive { function: call.symbol().to_string(), address });
    };
    let Some(index) = jni.vm_slot_of(address) else {
        return Err(AbiError::JniRefused {
            function: call.symbol().to_string(),
            address,
            detail: "this thunk address is not a JNIInvokeInterface slot of the active instance"
                .to_string(),
        });
    };
    let name = slots::VM_SLOTS[index];
    let value = {
        let mem = call.mem();
        let mut args = call.args();
        let _vm = args.next_pointer()?;
        match name {
            // `GetEnv(vm, void** env, jint version)`.
            "GetEnv" => {
                jni.count(name);
                let out = args.next_pointer()?;
                let version = args.next_i32()?;
                if version > JNI_VERSION_1_6 {
                    // A version this layer cannot supply. `JNI_EVERSION` is the specified answer
                    // and the caller is required to test for it; the out parameter is left alone,
                    // as the spec says.
                    JniReturn::Int(slots::JNI_EVERSION)
                } else if jni.is_attached(thread) {
                    mem.write_u64(out, jni.env_for(thread) as u64, Blame::new(name, address, 1))?;
                    JniReturn::Int(JNI_OK)
                } else {
                    // **The value the engine branches on.** §8 step 6a: the scoped-attach helper
                    // at `0x2174c04` tests the result with `cmn w0,#2`, which is a comparison
                    // against `-2`, and attaches when it matches. The out parameter is written
                    // null, which is what the spec requires and what stops a caller that ignores
                    // the return value from using a stale pointer.
                    mem.write_u64(out, 0, Blame::new(name, address, 1))?;
                    JniReturn::Int(JNI_EDETACHED)
                }
            }
            // `AttachCurrentThread(vm, JNIEnv** env, void* args)`.
            "AttachCurrentThread" => {
                jni.count(name);
                let out = args.next_pointer()?;
                let attach_args = args.next_pointer()?;
                // **The thread name is honoured**, which §2.2 states as part of the contract: the
                // engine calls `gettid`, formats a name and puts it in `JavaVMAttachArgs`. A null
                // `args` is legal and means "no name", which is the JNI 1.1 form.
                let name_text = if attach_args == 0 {
                    None
                } else {
                    let pointer = mem.read_u64(
                        attach_args + ATTACH_ARGS_NAME_OFFSET,
                        Blame::new(name, address, 2),
                    )? as GuestAddr;
                    if pointer == 0 {
                        None
                    } else {
                        let bytes = mem.cstr(pointer, Blame::new(name, address, 2))?;
                        Some(String::from_utf8_lossy(&bytes).into_owned())
                    }
                };
                jni.attach_thread(thread, name_text);
                mem.write_u64(out, jni.env_for(thread) as u64, Blame::new(name, address, 1))?;
                JniReturn::Int(JNI_OK)
            }
            // `DetachCurrentThread(vm)`. MEASURED: on the first game join, two of FMOD's threads
            // (started at link `0x4fbcbc4`, its thread trampoline) called it on their way out and
            // died on the refusal it used to be. It undoes `AttachCurrentThread`: the thread is no
            // longer attached (so `GetEnv` answers `JNI_EDETACHED` again), its name and pending
            // exception go with the attachment, and its `JNIEnv` slot is given back at the
            // thread's end as before. ART answers `JNI_ERR` for a thread that is not attached.
            "DetachCurrentThread" => {
                jni.count(name);
                if jni.is_attached(thread) {
                    jni.detach_thread(thread);
                    JniReturn::Int(JNI_OK)
                } else {
                    JniReturn::Int(slots::JNI_ERR)
                }
            }
            _ => {
                return Err(AbiError::JniRefused {
                    function: format!("JavaVM::{name}"),
                    address,
                    detail: format!(
                        "`{name}` is one of the five JavaVM slots no run of `libroblox.so` has \
                         called (jni-surface.md §2.2 measured 2 of 8 over 5 call sites; \
                         `DetachCurrentThread` was the third, at the first game join), so it has \
                         no implementation here; a call to it means the engine's behaviour has \
                         changed and is reported rather than guessed at"
                    ),
                })
            }
        }
    };
    value.write(call.ret());
    Ok(())
}

fn refuse_slot(name: &str, address: GuestAddr) -> AbiError {
    AbiError::JniRefused {
        function: format!("JNIEnv::{name}"),
        address,
        detail: format!(
            "`{name}` is one of the 174 JNINativeInterface slots `libroblox.so` never \
             dereferences (jni-surface.md §0 measured 59 of 233 used, 170 untouched and 4 \
             reserved), so it has no implementation here. It has a thunk address rather than a \
             null entry so that a call to it says this, instead of branching to zero"
        ),
    }
}

/// Every `JNIEnv` slot serviced inside the run loop, with the `JNIEnv*` argument already read.
#[allow(clippy::too_many_lines)]
fn env_call(
    jni: &Jni,
    thread: usize,
    name: &'static str,
    address: GuestAddr,
    args: &mut Args<'_>,
    mem: &GuestMem,
) -> AbiResult<JniReturn> {
    jni.count(name);
    let blame = |argument: usize| Blame::new(name, address, argument);
    match name {
        // ---- classes and members ---------------------------------------------------------
        "FindClass" => {
            let pointer = args.next_pointer()?;
            let text = read_cstr(mem, pointer, blame(1))?;
            let mut state = jni.state();
            let pending_before = state.threads.get(thread).is_some_and(|t| t.pending.is_some());
            let found = state.registry.find(&text);
            let record = |class: u64| super::ClassLookup {
                thread,
                function: name.to_string(),
                what: text.clone(),
                class,
                pending_before,
            };
            match found {
                Some(class) => {
                    let handle = state.handles.new_local(name, address, Object::Class(class))?;
                    jni.record_lookup(record(handle));
                    Ok(JniReturn::Word(handle))
                }
                None => {
                    jni.record_lookup(record(0));
                    // §3.1 Tier X: `DeviceUtils` and five `signalVideo*` methods have no
                    // declaring class in the whole APK, so `libroblox.so` gets null for them on a
                    // real device and tolerates it. Null with a pending
                    // `ClassNotFoundException` is therefore the *specified* answer, not a stub —
                    // and the miss is recorded, so a Tier 0 class arriving here is visible before
                    // the `CHECK_NOT_NULL` that would follow it.
                    state.registry.record_miss(Miss {
                        function: name.to_string(),
                        class: text.clone(),
                        member: String::new(),
                        descriptor: String::new(),
                    });
                    throw_pending(
                        &mut state,
                        thread,
                        name,
                        address,
                        "java/lang/ClassNotFoundException",
                        &text,
                    )?;
                    Ok(JniReturn::Word(0))
                }
            }
        }
        "GetObjectClass" => {
            let object = args.next_u64()?;
            let mut state = jni.state();
            let id = state.handles.resolve_id(name, address, object)?;
            let class = class_of(&state.handles, &state.registry, id)
                .ok_or_else(|| AbiError::JniRefused {
                function: name.to_string(),
                address,
                detail: "this object has no declared class: it is an array or a string, and \
                         nothing on the measured surface asks for the class of one"
                    .to_string(),
            })?;
            let handle = state.handles.new_local(name, address, Object::Class(class))?;
            Ok(JniReturn::Word(handle))
        }
        "GetMethodID" | "GetStaticMethodID" => {
            let is_static = name == "GetStaticMethodID";
            let class = args.next_u64()?;
            let member = read_cstr(mem, args.next_pointer()?, blame(2))?;
            let descriptor = read_cstr(mem, args.next_pointer()?, blame(3))?;
            // Parsed even though nothing here needs the parts yet: a descriptor that is not one
            // means the caller is not doing what it thinks it is, and finding that out at the
            // lookup is worth more than finding it out at the call.
            Descriptor::parse(name, address, &descriptor)?;
            let mut state = jni.state();
            jni.record_lookup(super::ClassLookup {
                thread,
                function: name.to_string(),
                what: format!("{member}{descriptor}"),
                class,
                pending_before: state.threads.get(thread).is_some_and(|t| t.pending.is_some()),
            });
            let class = class_handle(&state, name, address, class)
                .map_err(|error| asked_for(error, &member, &descriptor))?;
            match state.registry.method(class, &member, &descriptor, is_static) {
                Some(id) => Ok(JniReturn::Word(state.handles.method_id(id))),
                None => {
                    let class_name = state.registry.class_name(class).to_string();
                    state.registry.record_miss(Miss {
                        function: name.to_string(),
                        class: class_name.clone(),
                        member: member.clone(),
                        descriptor: descriptor.clone(),
                    });
                    throw_pending(
                        &mut state,
                        thread,
                        name,
                        address,
                        "java/lang/NoSuchMethodError",
                        &format!("{class_name}.{member}{descriptor}"),
                    )?;
                    Ok(JniReturn::Word(0))
                }
            }
        }
        "GetFieldID" | "GetStaticFieldID" => {
            let is_static = name == "GetStaticFieldID";
            let class = args.next_u64()?;
            let member = read_cstr(mem, args.next_pointer()?, blame(2))?;
            let descriptor = read_cstr(mem, args.next_pointer()?, blame(3))?;
            let mut state = jni.state();
            let class = class_handle(&state, name, address, class)
                .map_err(|error| asked_for(error, &member, &descriptor))?;
            match state.registry.field(class, &member, &descriptor, is_static) {
                Some(id) => Ok(JniReturn::Word(state.handles.field_id(id))),
                None => {
                    let class_name = state.registry.class_name(class).to_string();
                    state.registry.record_miss(Miss {
                        function: name.to_string(),
                        class: class_name.clone(),
                        member: member.clone(),
                        descriptor: descriptor.clone(),
                    });
                    throw_pending(
                        &mut state,
                        thread,
                        name,
                        address,
                        "java/lang/NoSuchFieldError",
                        &format!("{class_name}.{member} {descriptor}"),
                    )?;
                    Ok(JniReturn::Word(0))
                }
            }
        }

        // ---- references ------------------------------------------------------------------
        "NewGlobalRef" | "NewWeakGlobalRef" | "NewLocalRef" => {
            let kind = match name {
                "NewGlobalRef" => RefKind::Global,
                "NewWeakGlobalRef" => RefKind::Weak,
                _ => RefKind::Local,
            };
            let object = args.next_u64()?;
            // JNI says a null argument yields null. Honoured rather than refused, because the
            // engine's own `NewGlobalRef(FindClass(...))` pattern relies on it: a failed
            // `FindClass` on the Tier X path flows straight into this.
            if object == 0 {
                return Ok(JniReturn::Word(0));
            }
            let mut state = jni.state();
            Ok(JniReturn::Word(state.handles.duplicate(name, address, kind, object)?))
        }
        "DeleteGlobalRef" | "DeleteLocalRef" => {
            let kind =
                if name == "DeleteGlobalRef" { RefKind::Global } else { RefKind::Local };
            let object = args.next_u64()?;
            if object == 0 {
                // Deleting null is a no-op in JNI and the engine does it on the Tier X path.
                return Ok(JniReturn::Void);
            }
            jni.state().handles.delete(name, address, kind, object)?;
            Ok(JniReturn::Void)
        }
        "IsSameObject" => {
            let a = args.next_u64()?;
            let b = args.next_u64()?;
            let state = jni.state();
            let a = state.handles.resolve_nullable(name, address, a)?;
            let b = state.handles.resolve_nullable(name, address, b)?;
            Ok(JniReturn::Word(u64::from(if a == b { JNI_TRUE } else { JNI_FALSE })))
        }

        // ---- exceptions ------------------------------------------------------------------
        "ExceptionCheck" => {
            let state = jni.state();
            let pending = state.threads[thread].pending.is_some();
            Ok(JniReturn::Word(u64::from(if pending { JNI_TRUE } else { JNI_FALSE })))
        }
        "ExceptionOccurred" => {
            let mut state = jni.state();
            match state.threads[thread].pending {
                // A **new local reference** each time, as the spec requires: the caller may
                // delete what it is given, and handing back the stored handle would let a
                // `DeleteLocalRef` destroy the pending exception itself.
                Some(handle) => Ok(JniReturn::Word(state.handles.duplicate(
                    name,
                    address,
                    RefKind::Local,
                    handle,
                )?)),
                None => Ok(JniReturn::Word(0)),
            }
        }
        "ExceptionClear" => {
            clear_pending(&mut jni.state(), thread, name, address)?;
            Ok(JniReturn::Void)
        }
        "ExceptionDescribe" => {
            let mut state = jni.state();
            if let Some(handle) = state.threads[thread].pending {
                let text = match state.handles.object(name, address, handle) {
                    Ok(object) => super::render(&state, object),
                    Err(_) => "<the pending exception is no longer resolvable>".to_string(),
                };
                state.described.push(text);
            }
            // The spec says `ExceptionDescribe` **clears** the exception after reporting it.
            clear_pending(&mut state, thread, name, address)?;
            Ok(JniReturn::Void)
        }
        "Throw" => {
            let throwable = args.next_u64()?;
            let mut state = jni.state();
            // Resolved before it is stored, so a handle the guest invented is refused here rather
            // than at whatever later call reads the pending exception.
            state.handles.resolve_id(name, address, throwable)?;
            let held = state.handles.duplicate(name, address, RefKind::Global, throwable)?;
            clear_pending(&mut state, thread, name, address)?;
            state.threads[thread].pending = Some(held);
            Ok(JniReturn::Int(JNI_OK))
        }
        "ThrowNew" => {
            let class = args.next_u64()?;
            let message = args.next_pointer()?;
            let message =
                if message == 0 { String::new() } else { read_cstr(mem, message, blame(2))? };
            let mut state = jni.state();
            let class = class_handle(&state, name, address, class)?;
            let class_name = state.registry.class_name(class).to_string();
            throw_pending(&mut state, thread, name, address, &class_name, &message)?;
            Ok(JniReturn::Int(JNI_OK))
        }

        // ---- fields ----------------------------------------------------------------------
        "GetObjectField" | "GetBooleanField" | "GetIntField" | "GetLongField" | "GetFloatField"
        | "GetDoubleField" => {
            let object = args.next_u64()?;
            let field = args.next_u64()?;
            let mut state = jni.state();
            let id = state.handles.resolve_id(name, address, object)?;
            let field = state.handles.decode_field(name, address, field)?;
            let value = instance_field(&state, name, address, id, field)?;
            field_return(&mut state, name, address, value)
        }
        "GetStaticObjectField" | "GetStaticIntField" => {
            let _class = args.next_u64()?;
            let field = args.next_u64()?;
            let mut state = jni.state();
            let field = state.handles.decode_field(name, address, field)?;
            let member = field_member(&state, name, address, field)?.clone();
            let value = static_field(&mut state, name, address, field, &member)?;
            field_return(&mut state, name, address, value)
        }

        // ---- strings ---------------------------------------------------------------------
        "NewStringUTF" => {
            let pointer = args.next_pointer()?;
            if pointer == 0 {
                // JNI: a null `bytes` yields a null `jstring`.
                return Ok(JniReturn::Word(0));
            }
            let bytes = mem.cstr(pointer, blame(1))?;
            let text = JavaString::from_modified_utf8(name, address, &bytes)?;
            let mut state = jni.state();
            Ok(JniReturn::Word(state.handles.new_local(name, address, Object::String(text))?))
        }
        "NewString" => {
            let pointer = args.next_pointer()?;
            let len = args.next_i32()?;
            let len = usize::try_from(len).map_err(|_| AbiError::JniRefused {
                function: name.to_string(),
                address,
                detail: format!("a length of {len} is negative and cannot be a string length"),
            })?;
            let bytes = mem.read_bytes(pointer, len * 2, blame(1))?;
            let units =
                bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
            let mut state = jni.state();
            Ok(JniReturn::Word(state.handles.new_local(
                name,
                address,
                Object::String(JavaString::from_units(units)),
            )?))
        }
        "GetStringLength" => {
            let string = args.next_u64()?;
            let state = jni.state();
            let text = string_of(&state, name, address, string)?;
            Ok(JniReturn::Int(i32::try_from(text.len()).unwrap_or(i32::MAX)))
        }
        // **Outside the 59 §0 measured, and a run reached it**: gate74's thread 16 (started at
        // link `0x284d168`) died on this slot's refusal once `android.os.Build`'s strings were
        // answered. The length of what `GetStringUTFChars` copies out, less its terminator --
        // modified UTF-8, so U+0000 counts two and a supplementary character six.
        "GetStringUTFLength" => {
            let string = args.next_u64()?;
            let state = jni.state();
            let text = string_of(&state, name, address, string)?;
            Ok(JniReturn::Int(i32::try_from(text.modified_utf8_len()).unwrap_or(i32::MAX)))
        }
        "GetStringUTFChars" | "GetStringChars" => {
            let string = args.next_u64()?;
            let is_copy = args.next_pointer()?;
            // The state lock is taken, the copy is made, and the lock is **dropped before the
            // pool is touched**. See `Jni::state`: the two locks never overlap, which is the
            // whole of how a lock order is kept without one.
            let (id, text) = {
                let state = jni.state();
                let id = state.handles.resolve_id(name, address, string)?;
                let text = string_of(&state, name, address, string)?.clone();
                (id, text)
            };
            let (kind, bytes) = if name == "GetStringUTFChars" {
                let mut bytes = text.to_modified_utf8();
                // NUL-terminated, which is the whole reason U+0000 takes the two-byte form.
                bytes.push(0);
                (PinKind::StringUtf8, bytes)
            } else {
                let mut bytes = Vec::with_capacity(text.len() * 2);
                for unit in text.units() {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
                (PinKind::StringChars, bytes)
            };
            let at = jni.pin(name, address, mem, kind, id, &bytes)?;
            if is_copy != 0 {
                // Always a copy: this layer's strings are host-side values with no guest
                // representation, so `JNI_TRUE` is the fact rather than a convenience.
                mem.write_bytes(is_copy, &[JNI_TRUE as u8], blame(2))?;
            }
            Ok(JniReturn::Word(at as u64))
        }
        "ReleaseStringUTFChars" | "ReleaseStringChars" => {
            let _string = args.next_u64()?;
            let at = args.next_pointer()?;
            jni.release_pin(name, address, mem, at, slots::JNI_RELEASE_COPY_BACK)?;
            Ok(JniReturn::Void)
        }

        // ---- arrays ----------------------------------------------------------------------
        "GetArrayLength" => {
            let array = args.next_u64()?;
            let state = jni.state();
            let len = array_len(&state, name, address, array)?;
            Ok(JniReturn::Int(i32::try_from(len).unwrap_or(i32::MAX)))
        }
        "NewObjectArray" => {
            let len = args.next_i32()?;
            let element = args.next_u64()?;
            let initial = args.next_u64()?;
            let len = usize::try_from(len).map_err(|_| AbiError::JniRefused {
                function: name.to_string(),
                address,
                detail: format!("a length of {len} is negative"),
            })?;
            let mut state = jni.state();
            let element = class_handle(&state, name, address, element)?;
            let initial = state.handles.resolve_nullable(name, address, initial)?;
            let object = Object::ObjectArray { element, elements: vec![initial; len] };
            Ok(JniReturn::Word(state.handles.new_local(name, address, object)?))
        }
        "NewLongArray" => {
            let len = args.next_i32()?;
            let len = usize::try_from(len).map_err(|_| AbiError::JniRefused {
                function: name.to_string(),
                address,
                detail: format!("a length of {len} is negative"),
            })?;
            let mut state = jni.state();
            Ok(JniReturn::Word(state.handles.new_local(
                name,
                address,
                Object::LongArray(vec![0; len]),
            )?))
        }
        // **Outside the 59 §0 measured, and a run reached it**: gate80's thread 16 (started at
        // link `0x284d168`) died on this slot's refusal right after the engine logged
        // `handleTextBoxFocused_AndroidLayer_` for a tap on the login screen's username field.
        // A new `byte[]` of `len` zeros, as ART allocates one.
        "NewByteArray" => {
            let len = args.next_i32()?;
            let len = usize::try_from(len).map_err(|_| AbiError::JniRefused {
                function: name.to_string(),
                address,
                detail: format!("a length of {len} is negative"),
            })?;
            let mut state = jni.state();
            Ok(JniReturn::Word(state.handles.new_local(
                name,
                address,
                Object::ByteArray(vec![0; len]),
            )?))
        }
        "GetObjectArrayElement" => {
            let array = args.next_u64()?;
            let index = args.next_i32()?;
            let mut state = jni.state();
            let id = state.handles.resolve_id(name, address, array)?;
            let element = match state.handles.object_of(id) {
                Some(Object::ObjectArray { elements, .. }) => {
                    *element_at(name, address, elements, index)?
                }
                Some(other) => return Err(wrong_kind(name, address, other, "an Object[]")),
                None => return Err(freed(name, address)),
            };
            match element {
                None => Ok(JniReturn::Word(0)),
                Some(element) => {
                    let handle = state.handles.reference_to(name, address, RefKind::Local, element)?;
                    Ok(JniReturn::Word(handle))
                }
            }
        }
        "SetObjectArrayElement" => {
            let array = args.next_u64()?;
            let index = args.next_i32()?;
            let value = args.next_u64()?;
            let mut state = jni.state();
            let id = state.handles.resolve_id(name, address, array)?;
            let value = state.handles.resolve_nullable(name, address, value)?;
            match state.handles.object_of_mut(id) {
                Some(Object::ObjectArray { elements, .. }) => {
                    let slot = element_at_mut(name, address, elements, index)?;
                    *slot = value;
                    Ok(JniReturn::Void)
                }
                Some(other) => Err(wrong_kind(name, address, other, "an Object[]")),
                None => Err(freed(name, address)),
            }
        }
        "GetByteArrayElements" | "GetIntArrayElements" | "GetFloatArrayElements" => {
            let array = args.next_u64()?;
            let is_copy = args.next_pointer()?;
            // As the string case: the copy is made under the state lock and the pool is touched
            // only after it is dropped.
            let (id, kind, bytes) = {
                let state = jni.state();
                let id = state.handles.resolve_id(name, address, array)?;
                let (kind, bytes) = match state.handles.object_of(id) {
                    Some(Object::ByteArray(values)) if name == "GetByteArrayElements" => {
                        (PinKind::ByteArray, values.iter().map(|v| *v as u8).collect::<Vec<u8>>())
                    }
                    Some(Object::IntArray(values)) if name == "GetIntArrayElements" => (
                        PinKind::IntArray,
                        values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>(),
                    ),
                    Some(Object::FloatArray(values)) if name == "GetFloatArrayElements" => (
                        PinKind::FloatArray,
                        values.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>(),
                    ),
                    Some(other) => {
                        return Err(wrong_kind(
                            name,
                            address,
                            other,
                            "the array type this call takes",
                        ))
                    }
                    None => return Err(freed(name, address)),
                };
                (id, kind, bytes)
            };
            let at = jni.pin(name, address, mem, kind, id, &bytes)?;
            if is_copy != 0 {
                mem.write_bytes(is_copy, &[JNI_TRUE as u8], blame(2))?;
            }
            Ok(JniReturn::Word(at as u64))
        }
        "ReleaseByteArrayElements" | "ReleaseIntArrayElements" | "ReleaseFloatArrayElements" => {
            let _array = args.next_u64()?;
            let at = args.next_pointer()?;
            let mode = args.next_i32()?;
            jni.release_pin(name, address, mem, at, mode)?;
            Ok(JniReturn::Void)
        }
        "GetByteArrayRegion" => {
            let array = args.next_u64()?;
            let start = args.next_i32()?;
            let len = args.next_i32()?;
            let buffer = args.next_pointer()?;
            let state = jni.state();
            let id = state.handles.resolve_id(name, address, array)?;
            let bytes = match state.handles.object_of(id) {
                Some(Object::ByteArray(values)) => {
                    region(name, address, values.len(), start, len)?;
                    values[start as usize..(start + len) as usize]
                        .iter()
                        .map(|v| *v as u8)
                        .collect::<Vec<u8>>()
                }
                Some(other) => return Err(wrong_kind(name, address, other, "a byte[]")),
                None => return Err(freed(name, address)),
            };
            mem.write_bytes(buffer, &bytes, blame(4))?;
            Ok(JniReturn::Void)
        }
        // **Outside the 59 §0 measured, and a run reached it**: gate85's thread 17 (started at
        // link `0x284d168`) died on this slot's refusal once `NewByteArray` answered, right
        // after `onTextBoxFocused` -- the text box's text going into the `byte[]` for
        // `showKeyboard`. `len` bytes from `buffer` into the array from `start`, bounds checked
        // first, as `SetLongArrayRegion`.
        "SetByteArrayRegion" => {
            let array = args.next_u64()?;
            let start = args.next_i32()?;
            let len = args.next_i32()?;
            let buffer = args.next_pointer()?;
            let mut state = jni.state();
            let id = state.handles.resolve_id(name, address, array)?;
            let existing = match state.handles.object_of(id) {
                Some(Object::ByteArray(values)) => values.len(),
                Some(other) => return Err(wrong_kind(name, address, other, "a byte[]")),
                None => return Err(freed(name, address)),
            };
            region(name, address, existing, start, len)?;
            let bytes = mem.read_bytes(buffer, len as usize, blame(4))?;
            let Some(Object::ByteArray(values)) = state.handles.object_of_mut(id) else {
                return Err(freed(name, address));
            };
            for (index, byte) in bytes.iter().enumerate() {
                values[start as usize + index] = *byte as i8;
            }
            Ok(JniReturn::Void)
        }
        "SetLongArrayRegion" => {
            let array = args.next_u64()?;
            let start = args.next_i32()?;
            let len = args.next_i32()?;
            let buffer = args.next_pointer()?;
            let mut state = jni.state();
            let id = state.handles.resolve_id(name, address, array)?;
            let existing = match state.handles.object_of(id) {
                Some(Object::LongArray(values)) => values.len(),
                Some(other) => return Err(wrong_kind(name, address, other, "a long[]")),
                None => return Err(freed(name, address)),
            };
            region(name, address, existing, start, len)?;
            let bytes = mem.read_bytes(buffer, len as usize * 8, blame(4))?;
            let Some(Object::LongArray(values)) = state.handles.object_of_mut(id) else {
                return Err(freed(name, address));
            };
            for (index, chunk) in bytes.chunks_exact(8).enumerate() {
                values[start as usize + index] =
                    i64::from_le_bytes(chunk.try_into().expect("chunks_exact(8) gives eight"));
            }
            Ok(JniReturn::Void)
        }

        // ---- the rest of what the engine reaches -----------------------------------------
        "GetJavaVM" => {
            let out = args.next_pointer()?;
            mem.write_u64(out, jni.java_vm() as u64, blame(1))?;
            Ok(JniReturn::Int(JNI_OK))
        }
        "NewDirectByteBuffer" => {
            let at = args.next_pointer()?;
            let capacity = args.next_u64()? as i64;
            if capacity < 0 {
                return Err(AbiError::JniRefused {
                    function: name.to_string(),
                    address,
                    detail: format!("a capacity of {capacity} is negative"),
                });
            }
            // The range is checked **now**, against the address space, rather than when
            // something reads through it: the engine hands this buffer to
            // `NativeHelper.gameActivity_onFlagsLoaded`, and a window onto unmapped memory would
            // otherwise surface as a fault attributed to whatever read it.
            mem.checked_ptr(at, capacity as usize, false, blame(1))?;
            let mut state = jni.state();
            Ok(JniReturn::Word(state.handles.new_local(
                name,
                address,
                Object::DirectByteBuffer { address: at, capacity },
            )?))
        }

        _ => Err(refuse_slot(name, address)),
    }
}

/// `Call<T>Method[Static]V` and `NewObjectV`.
fn call_method(
    jni: &Jni,
    thread: usize,
    name: &'static str,
    address: GuestAddr,
    call: &mut ReentrantCall<'_>,
) -> AbiResult<JniReturn> {
    jni.count(name);
    let is_static = name.starts_with("CallStatic");
    let is_constructor = name == "NewObjectV";
    let (receiver, method, va_list) = {
        let mut args = call.args();
        let _env = args.next_pointer()?;
        let receiver = args.next_u64()?;
        let method = args.next_u64()?;
        let va_list = args.next_pointer()?;
        (receiver, method, va_list)
    };

    // Everything that needs the state lock happens before the possible guest call, and the lock
    // is dropped before it: a handler that re-enters the guest while holding this instance's lock
    // would deadlock the moment the guest called another JNI function.
    let (member, class, arguments, receiver_id) = {
        let state = jni.state();
        // **The receiver is named in the failure, and that is the whole diagnosis.** A null
        // `jmethodID` arrives with no name of its own — `decode_method` says so in those words —
        // but the `jobject` beside it does have one, and it is what identifies the lookup that
        // was never checked. Without it the refusal says only "some method of some class".
        let id = state
            .handles
            .decode_method(name, address, method)
            .map_err(|error| {
                name_the_receiver(&state.handles, &state.registry, error, receiver)
            })?;
        let member = state
            .registry
            .member(id)
            .ok_or_else(|| AbiError::JniBadHandle {
                function: name.to_string(),
                address,
                kind: "jmethodID",
                handle: method,
                why: "it names a member index this class does not have".to_string(),
            })?
            .clone();
        if member.is_static != is_static && !is_constructor {
            return Err(AbiError::JniRefused {
                function: name.to_string(),
                address,
                detail: format!(
                    "`{}.{}{}` is {}, and this call is the {} form",
                    state.registry.class_name(id.class),
                    member.name,
                    member.descriptor,
                    if member.is_static { "static" } else { "an instance method" },
                    if is_static { "static" } else { "instance" }
                ),
            });
        }
        let descriptor = Descriptor::parse(name, address, &member.descriptor)?;
        check_return_slot(name, address, &state, id, &member, &descriptor)?;
        let receiver_id = if is_static || is_constructor {
            None
        } else {
            Some(state.handles.resolve_id(name, address, receiver)?)
        };
        drop(state);
        let arguments =
            read_varargs(name, address, call.mem(), va_list, descriptor.parameters())?;
        (member, id.class, arguments, receiver_id)
    };

    // A `native` Java method is the one case that reaches guest code, and it is why this whole
    // family is on the exit path. Nothing on §8 steps 6-12 takes it.
    if member.answer == Answer::Native {
        let Some(target) = member.bound_native else {
            return Err(AbiError::JniRefused {
                function: name.to_string(),
                address,
                detail: format!(
                    "`{}.{}{}` is declared native and nothing has bound it: no \
                     `RegisterNatives` has named it and no exported `Java_*` symbol was supplied",
                    jni.class_name(class),
                    member.name,
                    member.descriptor
                ),
            });
        };
        let mut guest_args = vec![
            GuestArg::Pointer(jni.env_for(thread)),
            GuestArg::Int(if is_static { class_object(jni, name, address, class)? } else { receiver }),
        ];
        for value in &arguments {
            guest_args.push(guest_argument(value));
        }
        let returned = call.call_guest(target, &guest_args, NATIVE_METHOD_BUDGET)?;
        jni.record_call(class, &member, &arguments);
        return Ok(native_return(name, returned));
    }

    let mut state = jni.state();
    let value = evaluate(&mut state, name, address, class, &member, receiver_id, &arguments)?;
    state.record(class, &member, &arguments);
    drop(state);
    to_return(jni, name, address, value)
}

/// `RegisterNatives(env, jclass, const JNINativeMethod*, jint)`.
///
/// The array is three pointers per entry — `name`, `signature`, `fnPtr` — which is
/// `sizeof(JNINativeMethod) = 24` on LP64. Section F of the lists file confirms it against the
/// real table: `24 x 24 bytes` at `.data.rel.ro 0x062dc1c8`, so the stride is VERIFIED from this
/// binary rather than assumed from a header.
fn register_natives(
    jni: &Jni,
    name: &'static str,
    address: GuestAddr,
    mem: &GuestMem,
    class: u64,
    methods: GuestAddr,
    count: i32,
) -> AbiResult<JniReturn> {
    jni.count(name);
    if count < 0 {
        return Err(AbiError::JniRefused {
            function: name.to_string(),
            address,
            detail: format!("a method count of {count} is negative"),
        });
    }
    let blame = |argument| Blame::new(name, address, argument);
    let mut state = jni.state();
    let class = class_handle(&state, name, address, class)?;
    let class_name = state.registry.class_name(class).to_string();
    let mut bound = 0;
    for index in 0..count as usize {
        let entry = methods + index * NATIVE_METHOD_BYTES;
        let member = read_cstr(mem, mem.read_u64(entry, blame(2))? as GuestAddr, blame(2))?;
        let descriptor =
            read_cstr(mem, mem.read_u64(entry + 8, blame(2))? as GuestAddr, blame(2))?;
        let function = mem.read_u64(entry + 16, blame(2))? as GuestAddr;
        // `declared_method`, not `method`: JNI requires a `RegisterNatives` entry to name a
        // method **of the class it was given**, and binding a superclass's member to a subclass's
        // function pointer would silently rewrite what every other receiver of that class calls.
        match state.registry.declared_method(class, &member, &descriptor, true).or_else(|| {
            state.registry.declared_method(class, &member, &descriptor, false)
        }) {
            Some(id) => {
                if let Some(target) = state.registry.member_mut(id) {
                    target.bound_native = Some(function);
                    target.answer = Answer::Native;
                }
                bound += 1;
            }
            None => {
                // **Not an error.** `RegisterNatives` on a method this layer has not declared is
                // the engine telling the host something it did not know, and the whole point of
                // the record is to hear it. The binding is kept so a later milestone can declare
                // the class and find the function pointer already there.
                state.registry.record_miss(Miss {
                    function: name.to_string(),
                    class: class_name.clone(),
                    member: member.clone(),
                    descriptor: descriptor.clone(),
                });
            }
        }
        state.registrations.push(Registration {
            class: class_name.clone(),
            member,
            descriptor,
            function,
        });
    }
    let _ = bound;
    Ok(JniReturn::Int(JNI_OK))
}

/// `sizeof(JNINativeMethod)` on LP64: three pointers. VERIFIED against the real 24-entry table
/// (`jni-surface-lists.txt` Section F).
pub const NATIVE_METHOD_BYTES: usize = 24;

// ------------------------------------------------------------------------------ helpers

fn read_cstr(mem: &GuestMem, at: GuestAddr, blame: Blame<'_>) -> AbiResult<String> {
    let bytes = mem.cstr(at, blame)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Add the receiver's class to a `Call…Method…` handle failure.
///
/// The receiver is the one thing still nameable when the `jmethodID` is not: for an instance call
/// it is the object whose class declares the method that was looked up, and for a static call it
/// is that class itself. `jni-surface.md` §8.1's third failure mode is a lookup that returned
/// null and was not checked, and which class it was asked of is what says *where*.
fn name_the_receiver(
    handles: &Handles,
    registry: &Registry,
    error: AbiError,
    receiver: u64,
) -> AbiError {
    let AbiError::JniBadHandle { function, address, kind, handle, why } = error else {
        return error;
    };
    let named = if receiver == 0 {
        "the receiver is null too".to_string()
    } else {
        match handles.resolve_nullable("Call…Method…", address, receiver) {
            // A static call's receiver **is** the class, so it is named as itself rather than
            // through `class_of`, which would answer `java/lang/Class` and hide it.
            Ok(Some(id)) => match handles.object_of(id) {
                Some(Object::Class(class)) => {
                    format!("the receiver is the class `{}`", registry.class_name(*class))
                }
                _ => match class_of(handles, registry, id) {
                    Some(class) => format!("the receiver is a `{}`", registry.class_name(class)),
                    None => "the receiver is an array or a direct buffer".to_string(),
                },
            },
            Ok(None) | Err(_) => {
                format!("the receiver {receiver:#x} does not decode either")
            }
        }
    };
    AbiError::JniBadHandle { function, address, kind, handle, why: format!("{why} -- {named}") }
}

/// The class **of** an object, which is what `GetObjectClass` answers.
///
/// **A `jclass` is an instance of `java.lang.Class`, not of itself**, and getting that wrong is
/// not a detail: `RBX::Security::Android::Detail::JvmClassLoaderHelper` does
/// `GetObjectClass(someClass)` and then `GetMethodID(that, "getClassLoader",
/// "()Ljava/lang/ClassLoader;")` — the cache-the-app-ClassLoader pattern jni-surface.md section 6
/// describes. Answering the class itself makes that lookup ask
/// `NativeGLJavaInterface.getClassLoader`, which does not exist, and the null `jmethodID` goes
/// straight into `CallObjectMethodV`. MEASURED: that is exactly what it did.
fn class_of(handles: &Handles, registry: &Registry, id: ObjectId) -> Option<ClassId> {
    match handles.object_of(id)? {
        Object::Class(_) => registry.find("java/lang/Class"),
        Object::String(_) => registry.find("java/lang/String"),
        Object::Instance { class, .. } | Object::Throwable { class, .. } => Some(*class),
        // An array's class is `[B`, `[I`, `[Ljava/lang/Object;` and so on, and a direct
        // `ByteBuffer`'s is a framework class with no dex declaration. Nothing on the measured
        // surface asks for either, so the caller refuses by name rather than this returning
        // something believable.
        _ => None,
    }
}

/// Decode a `jclass` handle to the class it names.
/// A member lookup's bad-class refusal, with the member it was for.
///
/// MEASURED why: a worker died on `GetStaticMethodID` with a null class, and the refusal named
/// the null but not the method -- whose name and signature the call had already read -- so which
/// `FindClass` had answered null for it could not be told from the run.
fn asked_for(error: AbiError, member: &str, descriptor: &str) -> AbiError {
    match error {
        AbiError::JniBadHandle { function, address, kind, handle, why } => AbiError::JniBadHandle {
            function,
            address,
            kind,
            handle,
            why: format!("{why} (it asked for `{member}` `{descriptor}`)"),
        },
        other => other,
    }
}

fn class_handle(
    state: &JniState,
    name: &str,
    address: GuestAddr,
    handle: u64,
) -> AbiResult<ClassId> {
    match state.handles.object(name, address, handle)? {
        Object::Class(class) => Ok(*class),
        other => Err(wrong_kind(name, address, other, "a jclass")),
    }
}

fn string_of<'a>(
    state: &'a JniState,
    name: &str,
    address: GuestAddr,
    handle: u64,
) -> AbiResult<&'a JavaString> {
    match state.handles.object(name, address, handle)? {
        Object::String(text) => Ok(text),
        other => Err(wrong_kind(name, address, other, "a java.lang.String")),
    }
}

fn array_len(state: &JniState, name: &str, address: GuestAddr, handle: u64) -> AbiResult<usize> {
    Ok(match state.handles.object(name, address, handle)? {
        Object::ByteArray(values) => values.len(),
        Object::IntArray(values) => values.len(),
        Object::LongArray(values) => values.len(),
        Object::FloatArray(values) => values.len(),
        Object::ObjectArray { elements, .. } => elements.len(),
        other => return Err(wrong_kind(name, address, other, "an array")),
    })
}

fn wrong_kind(name: &str, address: GuestAddr, object: &Object, wanted: &str) -> AbiError {
    AbiError::JniRefused {
        function: name.to_string(),
        address,
        detail: format!("the handle names {}, and this call takes {wanted}", object.kind_name()),
    }
}

fn freed(name: &str, address: GuestAddr) -> AbiError {
    AbiError::JniRefused {
        function: name.to_string(),
        address,
        detail: "the object behind that handle has been freed between two steps of this call"
            .to_string(),
    }
}

/// Bounds-check an array region.
///
/// **A guest-chosen start and length**, and the two `try_from`s are what bound them: an `i32`
/// that survives `usize::try_from` is in `0..=i32::MAX`, so on a 64-bit host the sum of two of
/// them cannot overflow a `usize` and the `checked_add` is belt-and-braces rather than the bound.
/// That is stated rather than implied because mutation row `jni-A8` originally injected
/// `wrapping_add` here and **nothing caught it** — correctly, since on this host the two are the
/// same function. The bound that does the work is `end > len`, which `jni-A8` injects instead.
/// The `checked_add` stays for a 32-bit host, where it would not be the same function.
fn region(name: &str, address: GuestAddr, len: usize, start: i32, count: i32) -> AbiResult<()> {
    let refuse = || AbiError::JniRefused {
        function: name.to_string(),
        address,
        detail: format!(
            "the region [{start}, {start}+{count}) is not inside an array of {len} elements"
        ),
    };
    let start = usize::try_from(start).map_err(|_| refuse())?;
    let count = usize::try_from(count).map_err(|_| refuse())?;
    let end = start.checked_add(count).ok_or_else(refuse)?;
    if end > len {
        return Err(refuse());
    }
    Ok(())
}

fn element_at<'a, T>(
    name: &str,
    address: GuestAddr,
    elements: &'a [T],
    index: i32,
) -> AbiResult<&'a T> {
    let refuse = || AbiError::JniRefused {
        function: name.to_string(),
        address,
        detail: format!("index {index} is outside an array of {} elements", elements.len()),
    };
    let index = usize::try_from(index).map_err(|_| refuse())?;
    elements.get(index).ok_or_else(refuse)
}

fn element_at_mut<'a, T>(
    name: &str,
    address: GuestAddr,
    elements: &'a mut [T],
    index: i32,
) -> AbiResult<&'a mut T> {
    let len = elements.len();
    let refuse = || AbiError::JniRefused {
        function: name.to_string(),
        address,
        detail: format!("index {index} is outside an array of {len} elements"),
    };
    let index = usize::try_from(index).map_err(|_| refuse())?;
    elements.get_mut(index).ok_or_else(refuse)
}

fn field_member<'a>(
    state: &'a JniState,
    name: &str,
    address: GuestAddr,
    field: FieldId,
) -> AbiResult<&'a Member> {
    state.registry.field_member(field).ok_or_else(|| AbiError::JniBadHandle {
        function: name.to_string(),
        address,
        kind: "jfieldID",
        handle: 0,
        why: "it names a field index this class does not have".to_string(),
    })
}

/// What a **static** field holds, read the one way every reader reads it.
///
/// `GetStaticObjectField`/`GetStaticIntField` answer through this, and so does
/// [`Answer::StaticIsSet`], so a method that tests a field and a read of that field cannot
/// disagree about it. [`Answer::StaticInstance`] is the object `<clinit>` made,
/// [`Answer::Assigned`] is whatever a Java statement last stored (or `null`), and anything else is
/// the declared constant -- or a refusal naming the field when there is none.
pub(super) fn static_field(
    state: &mut JniState,
    name: &str,
    address: GuestAddr,
    field: FieldId,
    member: &Member,
) -> AbiResult<Value> {
    match member.answer {
        Answer::StaticInstance => static_instance(state, name, address, field, member),
        Answer::Assigned => assigned(state, name, address, field, member),
        other => Registry::simple_answer(other)
            .ok_or_else(|| unanswered(state, name, address, field.class, member)),
    }
}

/// [`Answer::Assigned`]: the object a Java statement stored in this static field, anchored in
/// `JniState::statics`, or Java `null` when no statement has stored one.
///
/// # Errors
///
/// [`AbiError::JniRefused`] when the declaration is not a static object field -- a `null` for a
/// primitive would be the plausible wrong answer -- or the read is a primitive getter, and
/// whatever the handle table refuses.
fn assigned(
    state: &mut JniState,
    name: &str,
    address: GuestAddr,
    field: FieldId,
    member: &Member,
) -> AbiResult<Value> {
    let primitive_read = name.starts_with("GetStatic") && name != "GetStaticObjectField";
    if !member.is_static || !member.descriptor.starts_with('L') || primitive_read {
        return Err(AbiError::JniRefused {
            function: name.to_string(),
            address,
            detail: format!(
                "`{}.{}` ({}, {}) is declared as a static object field the Java side assigns, \
                 which only a static field of an object type read as an object can be",
                state.registry.class_name(field.class),
                member.name,
                member.descriptor,
                if member.is_static { "static" } else { "not static" }
            ),
        });
    }
    match state.statics.get(&field) {
        Some(&held) => Ok(Value::Object(Some(state.handles.resolve_id(name, address, held)?))),
        None => Ok(Value::Object(None)),
    }
}

/// [`Answer::StaticInstance`]: the one object the class's `<clinit>` stored in this field.
///
/// Created on the first read and anchored by a global reference in [`JniState::statics`], so
/// every read -- on any thread -- is the same object, which `IsSameObject` and an identity-keyed
/// cache both depend on. The caller turns the answer into a local reference, as for any field.
///
/// # Errors
///
/// [`AbiError::JniRefused`] when the field is not typed as its own class or is read through
/// something other than `GetStaticObjectField` -- the declaration would then be claiming a
/// `<clinit>` that could not have run -- and whatever the handle table refuses.
pub(super) fn static_instance(
    state: &mut JniState,
    name: &str,
    address: GuestAddr,
    field: FieldId,
    member: &Member,
) -> AbiResult<Value> {
    let class = state.registry.class_name(field.class).to_string();
    let own_type = format!("L{class};");
    if name != "GetStaticObjectField" || !member.is_static || member.descriptor != own_type {
        return Err(AbiError::JniRefused {
            function: name.to_string(),
            address,
            detail: format!(
                "`{class}.{}` ({}, {}) is declared as the instance its own class's `<clinit>` \
                 stores, which only a static field of type `{own_type}` read by \
                 `GetStaticObjectField` can be",
                member.name,
                member.descriptor,
                if member.is_static { "static" } else { "not static" }
            ),
        });
    }
    if let Some(&held) = state.statics.get(&field) {
        return Ok(Value::Object(Some(state.handles.resolve_id(name, address, held)?)));
    }
    let id = state.handles.create(
        name,
        address,
        Object::Instance { class: field.class, fields: std::collections::BTreeMap::new() },
    )?;
    let held = state.handles.reference_to(name, address, RefKind::Global, id)?;
    state.statics.insert(field, held);
    Ok(Value::Object(Some(id)))
}

/// An instance field: whatever was stored on the object, or the class's declared default.
/// The elements of a `java.util.List` receiver: its `ArrayList` backing (`elementData`, the
/// first `size`), or none for a list without one -- the empty list this layer hands out.
fn list_elements(
    state: &JniState,
    name: &str,
    address: GuestAddr,
    class: ClassId,
    member: &Member,
    receiver: Option<ObjectId>,
) -> AbiResult<Vec<Option<ObjectId>>> {
    let Some(receiver) = receiver else {
        return Err(AbiError::JniRefused {
            function: name.to_string(),
            address,
            detail: format!(
                "`{}.{}` is an instance method and this call has no receiver",
                state.registry.class_name(class),
                member.name
            ),
        });
    };
    let Some(Object::Instance { class: of, fields }) = state.handles.object_of(receiver) else {
        return Err(AbiError::JniRefused {
            function: name.to_string(),
            address,
            detail: "the receiver of a `java.util.List` method is not a list object".to_string(),
        });
    };
    let size_field = state.registry.field(*of, "size", "I", false);
    let data_field = state.registry.field(*of, "elementData", "[Ljava/lang/Object;", false);
    let (Some(size_field), Some(data_field)) = (size_field, data_field) else {
        return Ok(Vec::new());
    };
    let size = match fields.get(&size_field) {
        None => return Ok(Vec::new()),
        Some(Value::Int(size)) => usize::try_from(*size).unwrap_or(0),
        Some(other) => {
            return Err(AbiError::JniRefused {
                function: name.to_string(),
                address,
                detail: format!("an ArrayList's size is {other:?}, not an int"),
            })
        }
    };
    let data = match fields.get(&data_field) {
        Some(Value::Object(Some(data))) => *data,
        _ if size == 0 => return Ok(Vec::new()),
        other => {
            return Err(AbiError::JniRefused {
                function: name.to_string(),
                address,
                detail: format!("an ArrayList of {size} has no backing array ({other:?})"),
            })
        }
    };
    match state.handles.object_of(data) {
        Some(Object::ObjectArray { elements, .. }) if elements.len() >= size => Ok(elements[..size].to_vec()),
        _ => Err(AbiError::JniRefused {
            function: name.to_string(),
            address,
            detail: format!("an ArrayList's backing array does not hold its {size} elements"),
        }),
    }
}

fn instance_field(
    state: &JniState,
    name: &str,
    address: GuestAddr,
    object: ObjectId,
    field: FieldId,
) -> AbiResult<Value> {
    if let Some(Object::Instance { fields, .. }) = state.handles.object_of(object) {
        if let Some(value) = fields.get(&field) {
            return Ok(value.clone());
        }
    }
    let member = field_member(state, name, address, field)?;
    Registry::simple_answer(member.answer)
        .ok_or_else(|| unanswered(state, name, address, field.class, member))
}

fn unanswered(
    state: &JniState,
    name: &str,
    address: GuestAddr,
    class: ClassId,
    member: &Member,
) -> AbiError {
    AbiError::JniRefused {
        function: name.to_string(),
        address,
        detail: format!(
            "`{}.{}` ({}) is on the measured JNI surface and this layer has not decided what it \
             answers; returning a believable value here is exactly the failure Global Constraint \
             1 forbids, so it refuses instead",
            state.registry.class_name(class),
            member.name,
            member.descriptor
        ),
    }
}

/// Turn a host value into the register form the slot returns.
fn field_return(
    state: &mut JniState,
    name: &str,
    address: GuestAddr,
    value: Value,
) -> AbiResult<JniReturn> {
    Ok(match value {
        Value::Void => JniReturn::Void,
        Value::Boolean(v) => JniReturn::Word(u64::from(if v { JNI_TRUE } else { JNI_FALSE })),
        Value::Byte(v) => JniReturn::Int(i32::from(v)),
        Value::Char(v) => JniReturn::Word(u64::from(v)),
        Value::Short(v) => JniReturn::Int(i32::from(v)),
        Value::Int(v) => JniReturn::Int(v),
        Value::Long(v) => JniReturn::Long(v),
        Value::Float(v) => JniReturn::Float(v),
        Value::Double(v) => JniReturn::Double(v),
        Value::Object(None) => JniReturn::Word(0),
        Value::Object(Some(id)) => {
            JniReturn::Word(state.handles.reference_to(name, address, RefKind::Local, id)?)
        }
        Value::Text(text) => {
            let handle = state.handles.new_local(
                name,
                address,
                Object::String(JavaString::from_str(&text)),
            )?;
            JniReturn::Word(handle)
        }
    })
}

fn to_return(jni: &Jni, name: &str, address: GuestAddr, value: Value) -> AbiResult<JniReturn> {
    let mut state = jni.state();
    field_return(&mut state, name, address, value)
}

/// Walk a guest `va_list` for the parameters a descriptor names.
///
/// **The promotion is where this goes silently wrong.** In the variadic part a `float` has
/// already been widened to `double` by the caller and a `jboolean`/`jbyte`/`jchar`/`jshort` to
/// `int`, so reading a `float` as four bytes yields `0.0` — [`crate::varargs`]'s own recorded
/// failure one level down. [`TypeTag::is_floating`] is what decides which bank each parameter
/// comes out of.
fn read_varargs(
    name: &'static str,
    address: GuestAddr,
    mem: &GuestMem,
    at: GuestAddr,
    parameters: &[TypeTag],
) -> AbiResult<Vec<Value>> {
    if parameters.is_empty() {
        // A method with no parameters: the engine still passes a `va_list`, and reading it would
        // be a guest-memory access for nothing.
        return Ok(Vec::new());
    }
    let mut list = GuestVaList::read(mem, at, Blame::new(name, address, 3))?;
    let mut values = Vec::with_capacity(parameters.len());
    for tag in parameters {
        values.push(match tag {
            TypeTag::Void => unreachable!("`V` is refused as a parameter type by Descriptor::parse"),
            TypeTag::Boolean => Value::Boolean(list.next_u64()? & 0xff != 0),
            TypeTag::Byte => Value::Byte(list.next_u64()? as u8 as i8),
            TypeTag::Char => Value::Char(list.next_u64()? as u16),
            TypeTag::Short => Value::Short(list.next_u64()? as u16 as i16),
            TypeTag::Int => Value::Int(list.next_u64()? as u32 as i32),
            TypeTag::Long => Value::Long(list.next_u64()? as i64),
            // Promoted to `double` by the caller, so it is taken as one and narrowed.
            TypeTag::Float => Value::Float(list.next_f64()? as f32),
            TypeTag::Double => Value::Double(list.next_f64()?),
            // Kept as the raw handle: it is resolved by whoever consumes the argument, with the
            // function that consumed it named in the error.
            TypeTag::Object => Value::Long(list.next_u64()? as i64),
        });
    }
    Ok(values)
}

/// The slot the engine used must agree with the descriptor's return type.
///
/// A `CallIntMethodV` on a `()V` method is not a mistake this layer can paper over: the engine
/// would read `W0` as an `int` the method never produced. Refusing names both sides.
fn check_return_slot(
    name: &str,
    address: GuestAddr,
    state: &JniState,
    id: MethodId,
    member: &Member,
    descriptor: &Descriptor,
) -> AbiResult<()> {
    let expected = match name {
        "NewObjectV" => return Ok(()),
        "CallVoidMethodV" | "CallStaticVoidMethodV" | "CallNonvirtualVoidMethodV" => TypeTag::Void,
        "CallObjectMethodV" | "CallStaticObjectMethodV" | "CallNonvirtualObjectMethodV" => {
            TypeTag::Object
        }
        "CallBooleanMethodV" | "CallStaticBooleanMethodV" | "CallNonvirtualBooleanMethodV" => {
            TypeTag::Boolean
        }
        "CallIntMethodV" | "CallStaticIntMethodV" | "CallNonvirtualIntMethodV" => TypeTag::Int,
        "CallLongMethodV" | "CallStaticLongMethodV" | "CallNonvirtualLongMethodV" => TypeTag::Long,
        "CallFloatMethodV" | "CallStaticFloatMethodV" | "CallNonvirtualFloatMethodV" => {
            TypeTag::Float
        }
        "CallDoubleMethodV" | "CallStaticDoubleMethodV" | "CallNonvirtualDoubleMethodV" => {
            TypeTag::Double
        }
        _ => return Err(refuse_slot_named(name, address)),
    };
    if descriptor.returns() != expected {
        return Err(AbiError::JniRefused {
            function: name.to_string(),
            address,
            detail: format!(
                "`{}.{}{}` returns {:?} and `{name}` reads {:?}",
                state.registry.class_name(id.class),
                member.name,
                member.descriptor,
                descriptor.returns(),
                expected
            ),
        });
    }
    Ok(())
}

fn refuse_slot_named(name: &str, address: GuestAddr) -> AbiError {
    AbiError::JniRefused {
        function: format!("JNIEnv::{name}"),
        address,
        detail: format!(
            "`{name}` is a Call-family slot `libroblox.so` never dereferences: §2.1 measured \
             exactly one call site for each of the `…MethodV` forms and **zero** for every \
             non-`V` and `…A` variant, because the engine reaches them all through the ICF-merged \
             `jni.h` inline wrapper"
        ),
    }
}

/// Evaluate a member's [`Answer`].
/// `SharedPreferences`, as far as the guest uses it: see `Context.getSharedPreferences`'s
/// declaration in `classes`. Object parameters arrive as raw handles (`Value::Long`), as in
/// `ShowKeyboard`.
fn preferences(
    state: &mut JniState,
    name: &str,
    address: GuestAddr,
    class: ClassId,
    member: &Member,
    receiver: Option<ObjectId>,
    arguments: &[Value],
) -> AbiResult<Value> {
    use super::PreferencesObject;
    let refuse = |state: &JniState, why: String| AbiError::JniRefused {
        function: name.to_string(),
        address,
        detail: format!(
            "`{}.{}{}`: {why}",
            state.registry.class_name(class),
            member.name,
            member.descriptor
        ),
    };
    let text = |state: &mut JniState, raw: i64, what: &str| -> AbiResult<String> {
        if raw == 0 {
            return Err(refuse(state, format!("the {what} is null, which Android throws on")));
        }
        let id = state.handles.resolve_id(name, address, raw as u64)?;
        match state.handles.object_of(id) {
            Some(Object::String(text)) => Ok(text.to_string_lossy()),
            other => Err(refuse(state, format!("the {what} is {other:?}, not a String"))),
        }
    };
    let this = |state: &JniState| -> AbiResult<(ObjectId, PreferencesObject)> {
        let Some(receiver) = receiver else {
            return Err(refuse(state, "called with no receiver".to_string()));
        };
        match state.preference_objects.get(&receiver) {
            Some(object) => Ok((receiver, object.clone())),
            None => Err(refuse(state, "the receiver is not a preferences object this layer made".to_string())),
        }
    };
    match member.answer {
        Answer::GetSharedPreferences => {
            let [Value::Long(store), Value::Int(mode)] = arguments else {
                return Err(refuse(state, format!("the arguments are {arguments:?}, not (String, int)")));
            };
            // `MODE_PRIVATE` (0), and `MODE_MULTI_PROCESS` (4), which is deprecated and changes
            // nothing on the Android this layer presents. `MODE_WORLD_READABLE`/`WRITEABLE` throw
            // `SecurityException` from API 24, and this layer raises no Java exceptions.
            if !matches!(mode, 0 | 4) {
                return Err(refuse(state, format!("mode {mode} throws SecurityException on API 24+")));
            }
            let store = text(state, *store, "name")?;
            let Some(impl_class) = state.registry.find("android/app/SharedPreferencesImpl") else {
                return Err(refuse(state, "`android/app/SharedPreferencesImpl` is not declared".to_string()));
            };
            let object = state.handles.create(
                name,
                address,
                Object::Instance { class: impl_class, fields: std::collections::BTreeMap::new() },
            )?;
            state.preference_objects.insert(object, PreferencesObject::Store(store));
            Ok(Value::Object(Some(object)))
        }
        Answer::PreferencesEdit => {
            let (_, object) = this(state)?;
            let PreferencesObject::Store(store) = object else {
                return Err(refuse(state, "the receiver is an editor, not a SharedPreferences".to_string()));
            };
            let Some(editor_class) = state.registry.find("android/app/SharedPreferencesImpl$EditorImpl") else {
                return Err(refuse(state, "`SharedPreferencesImpl$EditorImpl` is not declared".to_string()));
            };
            let editor = state.handles.create(
                name,
                address,
                Object::Instance { class: editor_class, fields: std::collections::BTreeMap::new() },
            )?;
            state.preference_objects.insert(editor, PreferencesObject::Editor { store, pending: Vec::new() });
            Ok(Value::Object(Some(editor)))
        }
        Answer::EditorPutString => {
            let (editor, object) = this(state)?;
            let [Value::Long(key), Value::Long(value)] = arguments else {
                return Err(refuse(state, format!("the arguments are {arguments:?}, not (String, String)")));
            };
            let key = text(state, *key, "key")?;
            // A null value is `remove(key)` on Android; the engine always passes a string.
            let value = text(state, *value, "value")?;
            match state.preference_objects.get_mut(&editor) {
                Some(PreferencesObject::Editor { pending, .. }) => pending.push((key, value)),
                _ => {
                    let _ = object;
                    return Err(refuse(state, "the receiver is not an editor".to_string()));
                }
            }
            Ok(Value::Object(Some(editor)))
        }
        Answer::EditorApply => {
            let (editor, _) = this(state)?;
            let Some(PreferencesObject::Editor { store, pending }) = state.preference_objects.get_mut(&editor)
            else {
                return Err(refuse(state, "the receiver is not an editor".to_string()));
            };
            let (store, writes) = (store.clone(), std::mem::take(pending));
            state.shared_preferences.entry(store).or_default().extend(writes);
            Ok(Value::Void)
        }
        _ => unreachable!("preferences() is only called for the four preference answers"),
    }
}

pub(super) fn evaluate(
    state: &mut JniState,
    name: &str,
    address: GuestAddr,
    class: ClassId,
    member: &Member,
    receiver: Option<ObjectId>,
    arguments: &[Value],
) -> AbiResult<Value> {
    match member.answer {
        Answer::NewInstance => {
            // `create` rather than `new_local`: the return marshaller makes the reference, and
            // making one here too would hand back one handle and leak the other.
            let object = state.handles.create(
                name,
                address,
                Object::Instance { class, fields: std::collections::BTreeMap::new() },
            )?;
            Ok(Value::Object(Some(object)))
        }
        Answer::NewInstanceOf(other) => {
            let Some(other_id) = state.registry.find(other) else {
                return Err(AbiError::JniRefused {
                    function: name.to_string(),
                    address,
                    detail: format!(
                        "`{}.{}` returns an instance of `{other}`, which is not declared",
                        state.registry.class_name(class),
                        member.name
                    ),
                });
            };
            let object = state.handles.create(
                name,
                address,
                Object::Instance { class: other_id, fields: std::collections::BTreeMap::new() },
            )?;
            Ok(Value::Object(Some(object)))
        }
        Answer::ResolveClass => {
            // The name resolver, not a code loader. See `classes`'s module documentation.
            let Some(Value::Long(handle)) = arguments.first() else {
                return Err(AbiError::JniRefused {
                    function: name.to_string(),
                    address,
                    detail: format!(
                        "`{}.{}` takes a class name and was called with {} arguments",
                        state.registry.class_name(class),
                        member.name,
                        arguments.len()
                    ),
                });
            };
            let text = match state.handles.object(name, address, *handle as u64)? {
                Object::String(text) => text.to_string_lossy(),
                other => return Err(wrong_kind(name, address, other, "a java.lang.String")),
            };
            // Both spellings: the engine caches the loader and then asks it for app classes by
            // their dotted name, while `FindClass` uses the slashed one.
            let jni_name = text.replace('.', "/");
            match state.registry.find(&jni_name) {
                Some(found) => {
                    let object = state.handles.create(name, address, Object::Class(found))?;
                    Ok(Value::Object(Some(object)))
                }
                None => {
                    state.registry.record_miss(Miss {
                        function: format!("{}.{}", "java/lang/ClassLoader", member.name),
                        class: jni_name,
                        member: String::new(),
                        descriptor: String::new(),
                    });
                    Ok(Value::Object(None))
                }
            }
        }
        Answer::StringBytes => {
            let Some(receiver) = receiver else {
                return Err(AbiError::JniRefused {
                    function: name.to_string(),
                    address,
                    detail: "`String.getBytes` was called with no receiver".to_string(),
                });
            };
            let bytes = match state.handles.object_of(receiver) {
                Some(Object::String(text)) => text.to_string_lossy().into_bytes(),
                Some(other) => {
                    return Err(wrong_kind(name, address, other, "a java.lang.String"))
                }
                None => return Err(freed(name, address)),
            };
            let object = state.handles.create(
                name,
                address,
                Object::ByteArray(bytes.into_iter().map(|b| b as i8).collect()),
            )?;
            Ok(Value::Object(Some(object)))
        }
        Answer::IdentityHash => match arguments.first() {
            // `Value::Long` is how an object parameter arrives: the raw handle, resolved here.
            Some(Value::Long(0)) => Ok(Value::Int(0)),
            Some(Value::Long(handle)) => {
                Ok(Value::Int(state.handles.resolve_id(name, address, *handle as u64)?.identity_hash()))
            }
            other => Err(AbiError::JniRefused {
                function: name.to_string(),
                address,
                detail: format!(
                    "`{}.{}` takes one object and was called with {other:?}",
                    state.registry.class_name(class),
                    member.name
                ),
            }),
        },
        Answer::ShowKeyboard => {
            let refuse = |why: &str| AbiError::JniRefused {
                function: name.to_string(),
                address,
                detail: format!(
                    "`{}.{}{}`: {why}",
                    state.registry.class_name(class),
                    member.name,
                    member.descriptor
                ),
            };
            // `Value::Long` is how an object parameter arrives: the raw handle, resolved here
            // (see `read_varargs`). MEASURED: gate86's first version matched resolved objects
            // and refused a real array as null.
            let [Value::Long(text_box), Value::Boolean(lay_out), Value::Long(bytes), Value::Long(info)] =
                arguments
            else {
                return Err(refuse("the arguments are not (long, boolean, byte[], NativeTextBoxInfo)"));
            };
            // Kotlin's `checkNotNullParameter` (`gameActivity_showKeyboard`) and
            // `new String(null, UTF_8)` (`showKeyboard`) both throw on a null array.
            if *bytes == 0 {
                return Err(refuse("the byte[] is null, which the Java side throws on"));
            }
            let bytes = state.handles.resolve_id(name, address, *bytes as u64)?;
            let Some(Object::ByteArray(bytes)) = state.handles.object_of(bytes) else {
                return Err(refuse("the third argument is not a byte[]"));
            };
            let raw: Vec<u8> = bytes.iter().map(|&byte| byte as u8).collect();
            let text = String::from_utf8_lossy(&raw).into_owned();
            let manual_focus_release = match (lay_out, *info) {
                (true, info) if info != 0 => {
                    let info = state.handles.resolve_id(name, address, info as u64)?;
                    let info_class = state
                        .registry
                        .find("com/roblox/engine/jni/model/NativeTextBoxInfo")
                        .ok_or_else(|| refuse("NativeTextBoxInfo is not declared"))?;
                    let field = state
                        .registry
                        .field(info_class, "manualFocusRelease", "Z", false)
                        .ok_or_else(|| refuse("NativeTextBoxInfo.manualFocusRelease is not declared"))?;
                    match instance_field(state, name, address, info, field)? {
                        Value::Boolean(manual) => Some(manual),
                        _ => return Err(refuse("manualFocusRelease is not a boolean")),
                    }
                }
                _ => None,
            };
            state.keyboard.push(super::KeyboardRequest::Show {
                text_box: *text_box,
                text,
                manual_focus_release,
            });
            Ok(Value::Void)
        }
        Answer::HideKeyboard => {
            state.keyboard.push(super::KeyboardRequest::Hide);
            Ok(Value::Void)
        }
        Answer::HostCallback | Answer::HostRequest => {
            let refuse = |state: &JniState, why: String| AbiError::JniRefused {
                function: name.to_string(),
                address,
                detail: format!(
                    "`{}.{}{}`: {why}",
                    state.registry.class_name(class),
                    member.name,
                    member.descriptor
                ),
            };
            let Some(entry) = receiver.and_then(|object| state.host_callbacks.get(&object)).cloned() else {
                return Err(refuse(
                    state,
                    "the receiver is not a callback object the embedding made, so there is no \
                     Java method body here to run"
                        .to_string(),
                ));
            };
            // `Value::Long` is how an object parameter arrives: the raw handle (see `ShowKeyboard`).
            let [Value::Long(raw)] = arguments else {
                return Err(refuse(state, format!("the arguments are {arguments:?}, not (String)")));
            };
            let argument = if *raw == 0 {
                None
            } else {
                let id = state.handles.resolve_id(name, address, *raw as u64)?;
                match state.handles.object_of(id) {
                    Some(Object::String(text)) => Some(text.to_string_lossy()),
                    other => return Err(refuse(state, format!("the argument is {other:?}, not a String"))),
                }
            };
            state.host_calls.push(super::HostCall { tag: entry.tag, argument });
            match (member.answer, entry.response) {
                (Answer::HostCallback, _) => Ok(Value::Void),
                (_, Some(response)) => Ok(Value::Text(response)),
                (_, None) => Err(refuse(
                    state,
                    "the receiver was made as a callback, not a request handler, so it has no \
                     answer to return"
                        .to_string(),
                )),
            }
        }
        Answer::Construct(fields) => {
            if fields.len() != arguments.len() {
                return Err(AbiError::JniRefused {
                    function: name.to_string(),
                    address,
                    detail: format!(
                        "`{}.{}{}` stores {} arguments into {} fields, and {} arrived",
                        state.registry.class_name(class),
                        member.name,
                        member.descriptor,
                        fields.len(),
                        fields.len(),
                        arguments.len()
                    ),
                });
            }
            let mut stored = std::collections::BTreeMap::new();
            for ((field, descriptor), value) in fields.iter().zip(arguments) {
                // An object parameter arrives as a raw handle, and storing the object behind it
                // would need a reference this instance holds for the field's life. No declared
                // constructor has one, so it refuses rather than keep an unheld object.
                if descriptor.starts_with('L') || descriptor.starts_with('[') {
                    return Err(AbiError::JniRefused {
                        function: name.to_string(),
                        address,
                        detail: format!(
                            "`{}.{}` stores an object into `{field}` ({descriptor}), which this \
                             layer does not keep",
                            state.registry.class_name(class),
                            member.name
                        ),
                    });
                }
                let Some(id) = state.registry.field(class, field, descriptor, false) else {
                    return Err(AbiError::JniRefused {
                        function: name.to_string(),
                        address,
                        detail: format!(
                            "`{}.{}` stores into `{field}` ({descriptor}), which is not declared",
                            state.registry.class_name(class),
                            member.name
                        ),
                    });
                };
                stored.insert(id, value.clone());
            }
            // `create` rather than `new_local`, as `NewInstance`: the return marshaller makes
            // the reference.
            let object =
                state.handles.create(name, address, Object::Instance { class, fields: stored })?;
            Ok(Value::Object(Some(object)))
        }
        Answer::GetSharedPreferences
        | Answer::PreferencesEdit
        | Answer::EditorPutString
        | Answer::EditorApply => preferences(state, name, address, class, member, receiver, arguments),
        Answer::ListSize | Answer::ListIsEmpty | Answer::ListGet | Answer::ListToArray => {
            let elements = list_elements(state, name, address, class, member, receiver)?;
            match member.answer {
                Answer::ListSize => Ok(Value::Int(i32::try_from(elements.len()).unwrap_or(i32::MAX))),
                Answer::ListIsEmpty => Ok(Value::Boolean(elements.is_empty())),
                Answer::ListGet => {
                    let index = match arguments.first() {
                        Some(Value::Int(index)) => *index,
                        other => {
                            return Err(AbiError::JniRefused {
                                function: name.to_string(),
                                address,
                                detail: format!("`List.get` takes an int and was called with {other:?}"),
                            })
                        }
                    };
                    match usize::try_from(index).ok().and_then(|at| elements.get(at)) {
                        Some(element) => Ok(Value::Object(*element)),
                        None => Err(AbiError::JniRefused {
                            function: name.to_string(),
                            address,
                            detail: format!(
                                "`List.get({index})` on a list of {} -- Java throws \
                                 IndexOutOfBoundsException, which this layer does not raise",
                                elements.len()
                            ),
                        }),
                    }
                }
                _ => {
                    let Some(element) = state.registry.find("java/lang/Object") else {
                        return Err(AbiError::JniRefused {
                            function: name.to_string(),
                            address,
                            detail: "`java/lang/Object` is not declared".to_string(),
                        });
                    };
                    let object = state.handles.create(name, address, Object::ObjectArray { element, elements })?;
                    Ok(Value::Object(Some(object)))
                }
            }
        }
        Answer::EmptyObjectArray => {
            let Some(element) = state.registry.find("java/lang/Object") else {
                return Err(AbiError::JniRefused {
                    function: name.to_string(),
                    address,
                    detail: "`java/lang/Object` is not declared".to_string(),
                });
            };
            let object = state.handles.create(
                name,
                address,
                Object::ObjectArray { element, elements: Vec::new() },
            )?;
            Ok(Value::Object(Some(object)))
        }
        Answer::Field(field_name) => {
            let Some(receiver) = receiver else {
                return Err(AbiError::JniRefused {
                    function: name.to_string(),
                    address,
                    detail: format!(
                        "`{}.{}` reads a field of its receiver and this call has none",
                        state.registry.class_name(class),
                        member.name
                    ),
                });
            };
            let Some(field) = state
                .registry
                .class(class)
                .and_then(|c| c.fields.iter().position(|f| f.name == field_name))
            else {
                return Err(AbiError::JniRefused {
                    function: name.to_string(),
                    address,
                    detail: format!(
                        "`{}.{}` reads field `{field_name}`, which that class does not declare",
                        state.registry.class_name(class),
                        member.name
                    ),
                });
            };
            instance_field(state, name, address, receiver, FieldId { class, member: field as u16 })
        }
        Answer::Native => Err(AbiError::JniRefused {
            function: name.to_string(),
            address,
            detail: format!(
                "`{}.{}{}` is native and this path does not reach guest code",
                state.registry.class_name(class),
                member.name,
                member.descriptor
            ),
        }),
        Answer::StaticIsSet(field_name) => {
            // The field is looked up on the method's own class, static and object-typed: that is
            // the shape `sget-object <field>; if-eqz` has, and anything else is a declaration
            // defect that must say so rather than answer.
            let Some(index) = state.registry.class(class).and_then(|c| {
                c.fields
                    .iter()
                    .position(|f| f.name == field_name && f.is_static && f.descriptor.starts_with('L'))
            }) else {
                return Err(AbiError::JniRefused {
                    function: name.to_string(),
                    address,
                    detail: format!(
                        "`{}.{}{}` tests the static object field `{field_name}`, which that class \
                         does not declare",
                        state.registry.class_name(class),
                        member.name,
                        member.descriptor
                    ),
                });
            };
            let field = FieldId { class, member: index as u16 };
            let held = field_member(state, name, address, field)?.clone();
            let value = static_field(state, name, address, field, &held)?;
            Ok(Value::Boolean(matches!(value, Value::Object(Some(_)))))
        }
        Answer::Unanswered => Err(unanswered(state, name, address, class, member)),
        // A field's answer reaching a method call is a declaration defect, and it says so
        // rather than falling into `simple_answer` and reading as "not decided".
        Answer::StaticInstance | Answer::Assigned => Err(AbiError::JniRefused {
            function: name.to_string(),
            address,
            detail: format!(
                "`{}.{}{}` is declared {:?}, which is an answer for a static field and not for a \
                 call",
                state.registry.class_name(class),
                member.name,
                member.descriptor,
                member.answer
            ),
        }),
        simple => {
            let _ = arguments;
            Registry::simple_answer(simple)
                .ok_or_else(|| unanswered(state, name, address, class, member))
        }
    }
}

fn guest_argument(value: &Value) -> GuestArg {
    match value {
        Value::Boolean(v) => GuestArg::Int(u64::from(*v)),
        Value::Byte(v) => GuestArg::Int(*v as i64 as u64),
        Value::Char(v) => GuestArg::Int(u64::from(*v)),
        Value::Short(v) => GuestArg::Int(*v as i64 as u64),
        Value::Int(v) => GuestArg::Int(*v as i64 as u64),
        Value::Long(v) => GuestArg::Int(*v as u64),
        Value::Float(v) => GuestArg::Float(*v),
        Value::Double(v) => GuestArg::Double(*v),
        Value::Void | Value::Object(None) => GuestArg::Int(0),
        Value::Object(Some(id)) => GuestArg::Int(u64::from(id.index())),
        Value::Text(_) => GuestArg::Int(0),
    }
}

fn native_return(name: &str, returned: crate::boundary::GuestReturn) -> JniReturn {
    match name {
        "CallVoidMethodV" | "CallStaticVoidMethodV" => JniReturn::Void,
        "CallFloatMethodV" | "CallStaticFloatMethodV" => JniReturn::Float(returned.as_f32()),
        "CallDoubleMethodV" | "CallStaticDoubleMethodV" => JniReturn::Double(returned.as_f64()),
        "CallIntMethodV" | "CallStaticIntMethodV" => JniReturn::Int(returned.as_i32()),
        _ => JniReturn::Word(returned.x0),
    }
}

fn class_object(jni: &Jni, name: &str, address: GuestAddr, class: ClassId) -> AbiResult<u64> {
    let mut state = jni.state();
    state.handles.new_local(name, address, Object::Class(class))
}

/// Replace this thread's pending exception with a new one of `class`.
fn throw_pending(
    state: &mut JniState,
    thread: usize,
    name: &str,
    address: GuestAddr,
    class: &str,
    message: &str,
) -> AbiResult<()> {
    let Some(id) = state.registry.find(class) else {
        return Err(AbiError::JniRefused {
            function: name.to_string(),
            address,
            detail: format!("`{class}` is not declared, so it cannot be thrown"),
        });
    };
    clear_pending(state, thread, name, address)?;
    // A **global** reference: it outlives the call that created it and has to survive every
    // local reference being deleted before `ExceptionCheck` runs.
    let handle = state.handles.new_local(
        name,
        address,
        Object::Throwable { class: id, message: message.to_string() },
    )?;
    let held = state.handles.duplicate(name, address, RefKind::Global, handle)?;
    state.handles.delete(name, address, RefKind::Local, handle)?;
    state.threads[thread].pending = Some(held);
    Ok(())
}

fn clear_pending(
    state: &mut JniState,
    thread: usize,
    name: &str,
    address: GuestAddr,
) -> AbiResult<()> {
    if let Some(handle) = state.threads[thread].pending.take() {
        state.handles.delete(name, address, RefKind::Global, handle)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jni::slots::{JNI_ABORT, JNI_COMMIT, JNI_ERR};

    const HANDLER: &str = "com/roblox/protocols/systemdialog/PlatformSystemDialogHandler";

    /// **`PlatformSystemDialogHandler.INSTANCE` is one object, read after read, and it outlives
    /// the reference the guest was given.**
    ///
    /// Three failures, each caught separately: a fresh object per read (the ids differ); an
    /// object nothing on the host holds, so the guest deleting its local frees it (the second
    /// read's id no longer resolves, or a new slot is made); and an object of the wrong class
    /// (the class check). Djinni's proxy cache is identity-keyed, which is why the first matters.
    #[test]
    fn a_static_instance_field_is_one_object_that_outlives_the_guests_reference() {
        let space = std::sync::Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(space).expect("a JNI instance");
        let mut state = jni.state();
        let class = state.registry.find(HANDLER).expect("declared");
        let own = format!("L{HANDLER};");
        let field = state.registry.field(class, "INSTANCE", &own, true).expect("declared");
        let member = state.registry.field_member(field).expect("a member").clone();
        assert_eq!(member.answer, Answer::StaticInstance);

        let first = static_instance(&mut state, "GetStaticObjectField", 0, field, &member)
            .expect("the first read creates it");
        let Value::Object(Some(id)) = first else { panic!("an object, not {first:?}") };
        match state.handles.object_of(id) {
            Some(Object::Instance { class: of, .. }) => assert_eq!(*of, class),
            other => panic!("an instance of {HANDLER}, not {other:?}"),
        }
        // What the guest is handed, and then what the guest does with it.
        let local = state.handles.reference_to("test", 0, RefKind::Local, id).expect("a local");
        state.handles.delete("DeleteLocalRef", 0, RefKind::Local, local).expect("deleted");

        let second = static_instance(&mut state, "GetStaticObjectField", 0, field, &member)
            .expect("the second read");
        assert_eq!(second, Value::Object(Some(id)), "the same object, still alive");
        assert!(state.handles.object_of(id).is_some(), "not freed with the guest's local");
    }

    /// **Each Kotlin `object` the engine reads `INSTANCE` of answers one instance of itself** --
    /// the declaration, not the generated surface, which leaves every `INSTANCE` unanswered.
    /// `FacialAgeEstimationProtocol`'s `setListener` is observed, not refused.
    #[test]
    fn every_kotlin_object_the_engine_reads_is_an_instance_of_itself() {
        const FAE: &str = "com/roblox/universalapp/facialageestimation/FacialAgeEstimationProtocol";
        let space = std::sync::Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(space).expect("a JNI instance");
        let mut state = jni.state();
        for name in [HANDLER, FAE] {
            let class = state.registry.find(name).expect("declared");
            let own = format!("L{name};");
            let field = state.registry.field(class, "INSTANCE", &own, true).expect("declared");
            let member = state.registry.field_member(field).expect("a member").clone();
            assert_eq!(member.answer, Answer::StaticInstance, "{name}.INSTANCE");
            let value = static_instance(&mut state, "GetStaticObjectField", 0, field, &member)
                .expect("the read");
            let Value::Object(Some(id)) = value else { panic!("an object, not {value:?}") };
            match state.handles.object_of(id) {
                Some(Object::Instance { class: of, .. }) => assert_eq!(*of, class, "{name}"),
                other => panic!("an instance of {name}, not {other:?}"),
            }
        }
        let class = state.registry.find(FAE).expect("declared");
        let method = state.registry.method(class, "setListener", "(J)V", false).expect("declared");
        let member = state.registry.member(method).expect("a member");
        assert_eq!(member.answer, Answer::Sink);
    }

    /// **`StartAppParams.surface()` answers the very `Surface` the host stored**, through the
    /// accessor the engine calls -- identity, not a new object of the same class, because the
    /// engine turns it into the `ANativeWindow` the window came from. And a field that is not an
    /// object-typed instance field of the holder's class is refused by name.
    #[test]
    fn an_accessor_answered_by_a_stored_field_returns_the_stored_object() {
        let space = std::sync::Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(space).expect("a JNI instance");
        let surface = jni.new_object("android/view/Surface").expect("a Surface");
        let params =
            jni.new_object("com/roblox/engine/jni/autovalue/StartAppParams").expect("params");
        jni.set_object_field(params, "surface", surface).expect("stored");
        assert!(jni.set_object_field(params, "vrContext", surface).is_err(), "not a field");

        let mut state = jni.state();
        let class = state.registry.find("com/roblox/engine/jni/autovalue/StartAppParams").unwrap();
        let method = state.registry.method(class, "surface", "()Landroid/view/Surface;", false).unwrap();
        let member = state.registry.member(method).unwrap().clone();
        let receiver = state.handles.resolve_id("test", 0, params).unwrap();
        let expected = state.handles.resolve_id("test", 0, surface).unwrap();
        // The host's own local goes; the field's anchor keeps the object.
        state.handles.delete("DeleteLocalRef", 0, RefKind::Local, surface).expect("deleted");
        let got = evaluate(&mut state, "CallObjectMethodV", 0, class, &member, Some(receiver), &[])
            .expect("answered");
        assert_eq!(got, Value::Object(Some(expected)), "the same object, still alive");
        assert!(state.handles.object_of(expected).is_some(), "kept alive by the field");
    }

    /// **`System.identityHashCode` is a function of the object, not of the handle**, and `null`
    /// is 0.
    ///
    /// Two different references to one object must agree -- Djinni's proxy cache hashes whatever
    /// `jobject` it holds, and a hash of the *handle* would file one Java object under two keys.
    #[test]
    fn identity_hash_is_the_objects_and_not_the_handles() {
        let space = std::sync::Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(space).expect("a JNI instance");
        let mut state = jni.state();
        let system = state.registry.find("java/lang/System").expect("declared");
        let method = state
            .registry
            .method(system, "identityHashCode", "(Ljava/lang/Object;)I", true)
            .expect("declared");
        let member = state.registry.member(method).expect("a member").clone();
        let empty = || Object::Instance { class: system, fields: std::collections::BTreeMap::new() };

        let one = state.handles.new_local("test", 0, empty()).expect("an object");
        let also_one = state.handles.duplicate("test", 0, RefKind::Global, one).expect("a second ref");
        let other = state.handles.new_local("test", 0, empty()).expect("another object");
        assert_ne!(one, also_one, "two handles");

        let mut hash = |handle: u64| {
            match evaluate(&mut state, "CallStaticIntMethodV", 0, system, &member, None, &[
                Value::Long(handle as i64),
            ]) {
                Ok(Value::Int(value)) => value,
                other => panic!("an int, not {other:?}"),
            }
        };
        let (a, b, c, null) = (hash(one), hash(also_one), hash(other), hash(0));
        assert_eq!(a, b, "one object through two handles");
        assert_ne!(a, c, "two live objects in two slots");
        assert_eq!(null, 0, "identityHashCode(null) is 0");
    }

    /// The claim `StaticInstance` makes is only true of a static field typed as its own class,
    /// read as an object; anywhere else it refuses, naming the field.
    #[test]
    fn a_static_instance_answer_refuses_where_its_claim_would_be_false() {
        let space = std::sync::Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(space).expect("a JNI instance");
        let mut state = jni.state();
        let class = state.registry.find(HANDLER).expect("declared");
        let field = state
            .registry
            .field(class, "INSTANCE", &format!("L{HANDLER};"), true)
            .expect("declared");
        let member = state.registry.field_member(field).expect("a member").clone();

        let error = static_instance(&mut state, "GetStaticIntField", 0, field, &member)
            .expect_err("read as an int");
        assert!(error.to_string().contains("INSTANCE"), "{error}");

        let mut wrong = member.clone();
        wrong.descriptor = "Ljava/lang/Object;".to_string();
        let error = static_instance(&mut state, "GetStaticObjectField", 0, field, &wrong)
            .expect_err("typed as another class");
        assert!(error.to_string().contains("Ljava/lang/Object;"), "{error}");
        assert!(state.statics.is_empty(), "a refused read creates nothing");
    }

    /// Which slots take the exit path, as a membership assertion. `NewObjectArray` starts with
    /// `NewObject` and must **not** be caught by the rule, which a `starts_with` would do.
    #[test]
    fn the_reentrant_set_is_exactly_the_slots_that_can_reach_guest_code() {
        let reentrant: Vec<&str> = (0..slots::ENV_SLOTS.len())
            .filter(|index| is_reentrant(*index))
            .map(|index| slots::ENV_SLOTS[index])
            .collect();
        for name in ["CallVoidMethodV", "CallStaticObjectMethodV", "NewObjectV", "RegisterNatives"]
        {
            assert!(reentrant.contains(&name), "{name} must be on the exit path");
        }
        for name in [
            "NewObjectArray",
            "FindClass",
            "GetMethodID",
            "NewStringUTF",
            "GetObjectArrayElement",
            "SetObjectArrayElement",
        ] {
            assert!(!reentrant.contains(&name), "{name} must stay inline");
        }
        // 30 instance `Call…`, 30 `CallNonvirtual…` and 30 `CallStatic…` are all caught by the
        // prefix, plus `AllocObject`, the three `NewObject` forms and `RegisterNatives`. The
        // count is stated so a change to the rule is visible rather than silent.
        assert_eq!(reentrant.len(), 30 + 30 + 30 + 5);
    }

    /// Every slot the analysis measured as used must be either implemented or on the exit path —
    /// i.e. none of the 59 may fall through to [`refuse_slot`]. Checked by name against the
    /// handler's own match arms, which is the one thing a count cannot see.
    #[test]
    fn every_measured_slot_is_handled_somewhere() {
        for (_, name, _) in slots::ENV_USED {
            assert!(
                HANDLED.contains(&name) || is_reentrant(slots::env_index(name).expect("declared")),
                "{name} is one of the 59 the engine uses and nothing handles it"
            );
        }
    }

    /// The list [`every_measured_slot_is_handled_somewhere`] checks against: the slots
    /// [`env_call`] has an arm for. Kept beside the match rather than derived from it, so that
    /// deleting an arm fails the test instead of shrinking the list with it.
    static HANDLED: &[&str] = &[
        "FindClass",
        "GetObjectClass",
        "GetMethodID",
        "GetStaticMethodID",
        "GetFieldID",
        "GetStaticFieldID",
        "NewGlobalRef",
        "NewWeakGlobalRef",
        "NewLocalRef",
        "DeleteGlobalRef",
        "DeleteLocalRef",
        "IsSameObject",
        "ExceptionCheck",
        "ExceptionOccurred",
        "ExceptionClear",
        "ExceptionDescribe",
        "Throw",
        "ThrowNew",
        "GetObjectField",
        "GetBooleanField",
        "GetIntField",
        "GetLongField",
        "GetFloatField",
        "GetDoubleField",
        "GetStaticObjectField",
        "GetStaticIntField",
        "NewStringUTF",
        "NewString",
        "GetStringLength",
        "GetStringUTFLength",
        "GetStringUTFChars",
        "GetStringChars",
        "ReleaseStringUTFChars",
        "ReleaseStringChars",
        "GetArrayLength",
        "NewObjectArray",
        "NewLongArray",
        "NewByteArray",
        "GetObjectArrayElement",
        "SetObjectArrayElement",
        "GetByteArrayElements",
        "GetIntArrayElements",
        "GetFloatArrayElements",
        "ReleaseByteArrayElements",
        "ReleaseIntArrayElements",
        "ReleaseFloatArrayElements",
        "GetByteArrayRegion",
        "SetByteArrayRegion",
        "SetLongArrayRegion",
        "GetJavaVM",
        "NewDirectByteBuffer",
    ];

    /// A null `jmethodID` is reported **with the class of the object it was going to be called
    /// on**, which is the only name such a failure has.
    ///
    /// `decode_method` can say nothing but "it is null": the id carries no class and no member.
    /// The `jobject` beside it does, and naming it is what turned M5's
    /// `CallObjectMethodV ... was given 0x0` into a diagnosis — it said
    /// `com/google/androidgamesdk/GameActivity`, which is how the missing lookup was found.
    ///
    /// The three shapes are asserted separately because each answers a different question: an
    /// instance names its class, a `jclass` names **itself** rather than `java/lang/Class` (a
    /// static call's receiver *is* the class, and `class_of` would hide it), and a null receiver
    /// says so rather than being reported as a second bad handle.
    #[test]
    fn a_null_method_id_is_reported_with_the_class_of_its_receiver() {
        let registry = Registry::with_declared();
        let mut handles = Handles::new(0x1234);
        let bad = || AbiError::JniBadHandle {
            function: "CallObjectMethodV".to_string(),
            address: 0,
            kind: "jmethodID",
            handle: 0,
            why: "it is null".to_string(),
        };
        let activity = registry.find("com/roblox/client/startup/MainGameActivity").expect("declared");

        let instance = handles
            .new_local(
                "NewObjectV",
                0,
                Object::Instance { class: activity, fields: std::collections::BTreeMap::new() },
            )
            .expect("a reference");
        let said = name_the_receiver(&handles, &registry, bad(), instance).to_string();
        assert!(
            said.contains("the receiver is a `com/roblox/client/startup/MainGameActivity`"),
            "{said}"
        );

        let class = handles.new_local("FindClass", 0, Object::Class(activity)).expect("a reference");
        let said = name_the_receiver(&handles, &registry, bad(), class).to_string();
        assert!(
            said.contains("the receiver is the class `com/roblox/client/startup/MainGameActivity`"),
            "a static call's receiver is the class itself, not java/lang/Class: {said}"
        );

        let said = name_the_receiver(&handles, &registry, bad(), 0).to_string();
        assert!(said.contains("the receiver is null too"), "{said}");

        // A handle this instance never issued is reported as that, not silently dropped.
        let said = name_the_receiver(&handles, &registry, bad(), 0xdead_beef).to_string();
        assert!(said.contains("does not decode either"), "{said}");

        // And an error that is not a bad handle passes through untouched.
        let other = AbiError::JniRefused {
            function: "CallObjectMethodV".to_string(),
            address: 0,
            detail: "something else".to_string(),
        };
        let said = name_the_receiver(&handles, &registry, other, instance).to_string();
        assert!(said.contains("something else") && !said.contains("the receiver"), "{said}");
    }

    /// **`GetObjectClass(jclass)` answers `java.lang.Class`, not the class itself.**
    ///
    /// The detector for the defect M4's gate found. `JvmClassLoaderHelper` takes the class of a
    /// `jclass` and asks *that* for `getClassLoader()Ljava/lang/ClassLoader;`. Answering the
    /// class itself makes the lookup ask `NativeGLJavaInterface.getClassLoader`, which does not
    /// exist, and the null `jmethodID` goes straight into `CallObjectMethodV`.
    #[test]
    fn the_class_of_a_jclass_is_java_lang_class_and_not_the_class_itself() {
        let registry = Registry::with_declared();
        let mut handles = Handles::new(0x1234);
        let subject = registry
            .find("com/roblox/engine/jni/NativeGLJavaInterface")
            .expect("declared");
        let handle = handles
            .new_local("FindClass", 0, Object::Class(subject))
            .expect("a reference");
        let id = handles.resolve_id("GetObjectClass", 0, handle).expect("live");
        let answered = class_of(&handles, &registry, id).expect("a class");
        assert_eq!(registry.class_name(answered), "java/lang/Class");
        assert_ne!(answered, subject, "a jclass is not an instance of itself");
        // And `java.lang.Class` is the one that declares `getClassLoader`, which is what the
        // engine asks it for next.
        assert!(
            registry
                .method(answered, "getClassLoader", "()Ljava/lang/ClassLoader;", false)
                .is_some()
        );

        // A string's class is `java.lang.String`, and an instance's is its own.
        let text = handles
            .new_local("NewStringUTF", 0, Object::String(JavaString::from_str("x")))
            .expect("a reference");
        let id = handles.resolve_id("GetObjectClass", 0, text).expect("live");
        assert_eq!(
            registry.class_name(class_of(&handles, &registry, id).expect("a class")),
            "java/lang/String"
        );
        let instance = handles
            .new_local(
                "NewObjectV",
                0,
                Object::Instance { class: subject, fields: std::collections::BTreeMap::new() },
            )
            .expect("a reference");
        let id = handles.resolve_id("GetObjectClass", 0, instance).expect("live");
        assert_eq!(class_of(&handles, &registry, id), Some(subject));
    }

    /// **The arithmetic that wraps in release and panics in debug** (Global Constraint 4). A
    /// guest-chosen `start` and `len` whose sum overflows must be refused, not admitted by a
    /// wrapped comparison.
    #[test]
    fn a_region_whose_bounds_overflow_is_refused() {
        assert!(region("GetByteArrayRegion", 0, 16, 0, 16).is_ok());
        for (start, len) in [(i32::MAX, i32::MAX), (-1, 4), (0, -1), (8, 16), (17, 0)] {
            let error =
                region("GetByteArrayRegion", 0x10, 16, start, len).expect_err("out of bounds");
            assert!(matches!(error, AbiError::JniRefused { .. }), "{start},{len}: {error:?}");
        }
    }

    /// **`GetStringUTFLength` is the modified-UTF-8 length, through the slot itself.** Each unit
    /// of the string takes a different width: `A` one byte, `é` two, U+0000 two (the form that
    /// keeps a NUL out of a C string), and a surrogate pair six. A slot answering the UTF-16
    /// length (`GetStringLength`'s) says 5; standard UTF-8 says 1+2+1+4 = 8; a refusal fails
    /// the `expect`.
    #[test]
    fn get_string_utf_length_is_the_modified_utf8_length() {
        let (jni, mem) = slot_fixture();
        let text = JavaString::from_units(vec![0x41, 0xe9, 0x0000, 0xd83d, 0xde00]);
        let string = jni
            .state()
            .handles
            .new_local("NewString", 0, Object::String(text))
            .expect("a reference");
        let answer = call_slot(&jni, &mem, "GetStringUTFLength", &[string])
            .expect("GetStringUTFLength is answered");
        assert_eq!(answer, JniReturn::Int(1 + 2 + 2 + 6));
    }

    /// **`NewByteArray(n)` is a `byte[]` of `n` zeros, through the slot itself**, and a negative
    /// length is refused rather than wrapped into a huge one. A refusal of the slot fails the
    /// first `expect`; an array of the wrong kind or length fails the match.
    #[test]
    fn new_byte_array_is_a_byte_array_of_zeros() {
        let (jni, mem) = slot_fixture();
        let JniReturn::Word(array) =
            call_slot(&jni, &mem, "NewByteArray", &[5]).expect("NewByteArray is answered")
        else {
            panic!("NewByteArray answers a reference");
        };
        match jni.state().handles.object("test", 0, array).expect("a live reference") {
            Object::ByteArray(bytes) => assert_eq!(bytes, &vec![0i8; 5]),
            other => panic!("NewByteArray made {}", other.kind_name()),
        }
        let negative = u64::from((-1i32) as u32);
        let error = call_slot(&jni, &mem, "NewByteArray", &[negative]).expect_err("negative");
        assert!(matches!(error, AbiError::JniRefused { .. }), "{error:?}");
    }

    const TEXT_BOX_INFO: &str = "com/roblox/engine/jni/model/NativeTextBoxInfo";

    /// `new NativeTextBoxInfo(...)` with `manualFocusRelease` as given, the rest distinct.
    fn text_box_info(state: &mut JniState, manual: bool) -> ObjectId {
        let class = state.registry.find(TEXT_BOX_INFO).expect("declared");
        let init = state.registry.method(class, "<init>", "(FFFFFZIIIIIIZZZ)V", false).expect("declared");
        let member = state.registry.member(init).expect("a member").clone();
        let arguments = [
            Value::Float(10.0),
            Value::Float(20.0),
            Value::Float(300.0),
            Value::Float(40.0),
            Value::Float(16.0),
            Value::Boolean(false),
            Value::Int(1),
            Value::Int(2),
            Value::Int(0x00ff_ffff),
            Value::Int(4),
            Value::Int(5),
            Value::Int(6),
            Value::Boolean(manual),
            Value::Boolean(true),
            Value::Boolean(true),
        ];
        match evaluate(state, "NewObjectV", 0, class, &member, None, &arguments).expect("constructed") {
            Value::Object(Some(object)) => object,
            other => panic!("the constructor made {other:?}"),
        }
    }

    /// **`NativeTextBoxInfo.<init>` keeps what it was given**, one argument per field, as its
    /// fifteen `iput`s do: `manualFocusRelease` (argument 13) and `textColor` (argument 9) read
    /// back as passed. `NewInstance`, which it was, drops them all and the reads refuse. A call
    /// with the wrong number of arguments refuses rather than storing a prefix.
    #[test]
    fn a_text_box_info_keeps_its_constructor_arguments() {
        let (jni, _mem) = slot_fixture();
        let mut state = jni.state();
        let info = text_box_info(&mut state, true);
        let class = state.registry.find(TEXT_BOX_INFO).expect("declared");
        let read = |state: &JniState, name: &str, descriptor: &str| {
            let field = state.registry.field(class, name, descriptor, false).expect("declared");
            instance_field(state, "GetBooleanField", 0, info, field).expect("stored")
        };
        assert_eq!(read(&state, "manualFocusRelease", "Z"), Value::Boolean(true));
        assert_eq!(read(&state, "textColor", "I"), Value::Int(0x00ff_ffff));
        assert_eq!(read(&state, "width", "F"), Value::Float(300.0));

        let init = state.registry.method(class, "<init>", "(FFFFFZIIIIIIZZZ)V", false).expect("declared");
        let member = state.registry.member(init).expect("a member").clone();
        let error = evaluate(&mut state, "NewObjectV", 0, class, &member, None, &[Value::Float(1.0)])
            .expect_err("one argument for fifteen fields");
        assert!(matches!(error, AbiError::JniRefused { .. }), "{error:?}");
    }

    /// **`showKeyboard` hands the embedding what the Java side decodes**: the text box, the
    /// `byte[]` as UTF-8 (`é` is two bytes and one character), and `manualFocusRelease` only
    /// when the `boolean` asks for the field to be laid out from the info. A null `byte[]` is
    /// what the Java side throws on, so it refuses. `hideKeyboard` is a `Hide`.
    #[test]
    fn show_keyboard_queues_the_decoded_request() {
        let (jni, _mem) = slot_fixture();
        {
            let mut state = jni.state();
            let info = text_box_info(&mut state, true);
            let helper = state.registry.find("com/roblox/client/startup/NativeHelper").expect("declared");
            let show = state
                .registry
                .method(helper, "gameActivity_showKeyboard", "(JZ[BLcom/roblox/engine/jni/model/NativeTextBoxInfo;)V", false)
                .expect("declared");
            let show = state.registry.member(show).expect("a member").clone();
            let bytes: Vec<i8> = "h\u{e9}".bytes().map(|byte| byte as i8).collect();
            // Raw handles, as `read_varargs` hands object parameters on: `Value::Long`.
            let array = state.handles.new_local("NewByteArray", 0, Object::ByteArray(bytes)).expect("an array");
            let info = state.handles.reference_to("NewObjectV", 0, RefKind::Local, info).expect("a local");
            for lay_out in [true, false] {
                let arguments = [
                    Value::Long(0x7a_1000),
                    Value::Boolean(lay_out),
                    Value::Long(array as i64),
                    Value::Long(info as i64),
                ];
                assert_eq!(
                    evaluate(&mut state, "CallVoidMethodV", 0, helper, &show, None, &arguments).expect("answered"),
                    Value::Void
                );
            }
            let null_bytes = [Value::Long(1), Value::Boolean(false), Value::Long(0), Value::Long(0)];
            let error = evaluate(&mut state, "CallVoidMethodV", 0, helper, &show, None, &null_bytes)
                .expect_err("a null byte[]");
            assert!(matches!(error, AbiError::JniRefused { .. }), "{error:?}");

            let hide = state.registry.method(helper, "gameActivity_hideKeyboard", "()V", false).expect("declared");
            let hide = state.registry.member(hide).expect("a member").clone();
            evaluate(&mut state, "CallVoidMethodV", 0, helper, &hide, None, &[]).expect("answered");
        }
        assert_eq!(
            jni.take_keyboard_requests(),
            [
                crate::jni::KeyboardRequest::Show {
                    text_box: 0x7a_1000,
                    text: "h\u{e9}".to_string(),
                    manual_focus_release: Some(true),
                },
                crate::jni::KeyboardRequest::Show {
                    text_box: 0x7a_1000,
                    text: "h\u{e9}".to_string(),
                    manual_focus_release: None,
                },
                crate::jni::KeyboardRequest::Hide,
            ]
        );
        assert!(jni.take_keyboard_requests().is_empty(), "taken once");
    }

    /// **`SetByteArrayRegion` copies the guest's bytes into the array at `start`**, leaving the
    /// rest as it was, and refuses a region past the end rather than writing a prefix. A
    /// refusal of the slot fails the first `expect`; an off-by-one start fails the contents.
    #[test]
    fn set_byte_array_region_copies_the_guest_bytes_in() {
        use omni_mem::{CommitPolicy, Placement, Protection};
        let (jni, mem) = slot_fixture();
        let page = mem.space().page_size();
        let buffer = mem
            .space()
            .map_anonymous(Placement::Anywhere { align: page }, page, Protection::ReadWrite, CommitPolicy::Lazy)
            .expect("a guest buffer");
        mem.write_bytes(buffer, b"h\xc3\xa9", Blame::new("test", 0, 0)).expect("written");
        let JniReturn::Word(array) = call_slot(&jni, &mem, "NewByteArray", &[5]).expect("an array") else {
            panic!("a reference");
        };
        call_slot(&jni, &mem, "SetByteArrayRegion", &[array, 1, 3, buffer as u64])
            .expect("SetByteArrayRegion is answered");
        match jni.state().handles.object("test", 0, array).expect("live") {
            Object::ByteArray(bytes) => assert_eq!(bytes, &vec![0, b'h' as i8, 0xc3_u8 as i8, 0xa9_u8 as i8, 0]),
            other => panic!("{}", other.kind_name()),
        }
        let error = call_slot(&jni, &mem, "SetByteArrayRegion", &[array, 3, 3, buffer as u64])
            .expect_err("three bytes from index 3 of five");
        assert!(matches!(error, AbiError::JniRefused { .. }), "{error:?}");
    }

    /// Call `java.util.List.<member>` on `list` the way the engine does, through its declared answer.
    fn call_list(jni: &Jni, list: u64, member: &str, descriptor: &str, arguments: &[Value]) -> AbiResult<Value> {
        let mut state = jni.state();
        let class = state.registry.find("java/util/List").expect("declared");
        let method = state.registry.method(class, member, descriptor, false).expect("declared");
        let member = state.registry.member(method).expect("a member").clone();
        let receiver = state.handles.resolve_id("test", 0, list).expect("a live list");
        evaluate(&mut state, "CallIntMethodV", 0, class, &member, Some(receiver), arguments)
    }

    /// Call `member` as looked up on `class`, on `receiver`, the way `Call*MethodV` does.
    fn call_on(
        jni: &Jni,
        class: &str,
        receiver: ObjectId,
        member: &str,
        descriptor: &str,
        arguments: &[Value],
    ) -> AbiResult<Value> {
        let mut state = jni.state();
        let class = state.registry.find(class).expect("declared");
        let method = state.registry.method(class, member, descriptor, false).expect("declared");
        let member = state.registry.member(method).expect("a member").clone();
        evaluate(&mut state, "CallObjectMethodV", 0, class, &member, Some(receiver), arguments)
    }

    /// **The engine's one preferences write lands in the store it named**: `getSharedPreferences`
    /// looked up on `Context` and called on the `Application`, `edit()` and two `putString`s
    /// through the classes `GetObjectClass` answers, then `apply()`. Before `apply` nothing is in
    /// the store; a second editor's writes join the first's; a world-readable mode, a null value
    /// and a receiver that is not an editor are refused.
    #[test]
    fn the_engines_preferences_write_lands_in_the_named_store() {
        let (jni, _mem) = slot_fixture();
        let text = |value: &str| Value::Long(jni.new_string(value).expect("a string") as i64);
        let application = {
            let handle = jni.new_object("android/app/Application").expect("an Application");
            jni.state().handles.resolve_id("test", 0, handle).expect("live")
        };
        let object = |value: Value| match value {
            Value::Object(Some(id)) => id,
            other => panic!("{other:?}"),
        };
        let prefs = object(
            call_on(
                &jni,
                "android/content/Context",
                application,
                "getSharedPreferences",
                "(Ljava/lang/String;I)Landroid/content/SharedPreferences;",
                &[text("app_update"), Value::Int(0)],
            )
            .expect("getSharedPreferences"),
        );
        let editor = object(
            call_on(&jni, "android/app/SharedPreferencesImpl", prefs, "edit", "()Landroid/content/SharedPreferences$Editor;", &[])
                .expect("edit"),
        );
        let put = "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/SharedPreferences$Editor;";
        let editor_class = "android/app/SharedPreferencesImpl$EditorImpl";
        let returned = call_on(&jni, editor_class, editor, "putString", put, &[text("channel"), text("production")])
            .expect("putString");
        assert_eq!(returned, Value::Object(Some(editor)), "the editor returns itself");
        call_on(&jni, editor_class, editor, "putString", put, &[text("moduleName"), text("engine")]).expect("putString");
        assert_eq!(jni.shared_preferences("app_update"), None, "nothing is in the store before apply");
        assert_eq!(call_on(&jni, editor_class, editor, "apply", "()V", &[]).expect("apply"), Value::Void);
        let second = object(
            call_on(&jni, "android/app/SharedPreferencesImpl", prefs, "edit", "()Landroid/content/SharedPreferences$Editor;", &[])
                .expect("edit"),
        );
        call_on(&jni, editor_class, second, "putString", put, &[text("channel"), text("beta")]).expect("putString");
        call_on(&jni, editor_class, second, "apply", "()V", &[]).expect("apply");
        let stored = jni.shared_preferences("app_update").expect("applied");
        assert_eq!(stored.get("channel").map(String::as_str), Some("beta"), "a later apply overwrites");
        assert_eq!(stored.get("moduleName").map(String::as_str), Some("engine"), "and keeps the rest");

        assert!(call_on(
            &jni,
            "android/content/Context",
            application,
            "getSharedPreferences",
            "(Ljava/lang/String;I)Landroid/content/SharedPreferences;",
            &[text("x"), Value::Int(1)],
        )
        .is_err(), "MODE_WORLD_READABLE throws on the Android this layer presents");
        assert!(call_on(&jni, editor_class, editor, "putString", put, &[text("k"), Value::Long(0)]).is_err());
        assert!(call_on(&jni, editor_class, application, "apply", "()V", &[]).is_err(), "not an editor");
    }

    /// **The exit list is what the embedding recorded, and the engine reads it as a device's**:
    /// none recorded is the empty list (`size` 0, and `get(0)` refuses where Java throws); one
    /// recorded is one `ApplicationExitInfoCpp` whose fields carry the record -- the reason text
    /// `jk.l2.b` cuts out of `toString()`, `USER REQUESTED`, the pid the run had, the signal,
    /// the importance. A list answering the old constant 0 fails the second half; one whose
    /// element lacks the reason text fails the field reads.
    #[test]
    fn previous_exits_reach_the_engine_as_the_java_side_builds_them() {
        let (jni, _mem) = slot_fixture();
        let empty = jni.previous_exit_reasons().expect("an empty list");
        assert_eq!(call_list(&jni, empty, "size", "()I", &[]).expect("size"), Value::Int(0));
        assert!(call_list(&jni, empty, "get", "(I)Ljava/lang/Object;", &[Value::Int(0)]).is_err());

        jni.set_previous_exits(vec![crate::jni::ExitRecord {
            pid: 40204,
            reason: crate::jni::ExitRecord::REASON_USER_REQUESTED,
            status: crate::jni::ExitRecord::SIGKILL,
            timestamp_ms: 1_790_139_658_000,
            importance: crate::jni::ExitRecord::IMPORTANCE_CACHED,
        }]);
        let list = jni.previous_exit_reasons().expect("a list");
        assert_eq!(call_list(&jni, list, "size", "()I", &[]).expect("size"), Value::Int(1));
        assert_eq!(call_list(&jni, list, "isEmpty", "()Z", &[]).expect("isEmpty"), Value::Boolean(false));
        let Value::Object(Some(exit)) =
            call_list(&jni, list, "get", "(I)Ljava/lang/Object;", &[Value::Int(0)]).expect("get(0)")
        else {
            panic!("get(0) answers the record");
        };
        assert!(call_list(&jni, list, "get", "(I)Ljava/lang/Object;", &[Value::Int(1)]).is_err());
        let state = jni.state();
        let class = state.registry.find("com/roblox/engine/jni/model/ApplicationExitInfoCpp").expect("declared");
        let read = |name: &str, descriptor: &str| {
            let field = state.registry.field(class, name, descriptor, false).expect("declared");
            instance_field(&state, "GetObjectField", 0, exit, field).expect("set")
        };
        let text = |value: Value| match value {
            Value::Object(Some(id)) => match state.handles.object_of(id) {
                Some(Object::String(text)) => text.to_string_lossy(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        assert_eq!(text(read("mExitReason", "Ljava/lang/String;")), "USER REQUESTED");
        assert_eq!(text(read("mExitSubreason", "Ljava/lang/String;")), "UNKNOWN");
        assert_eq!(read("mPid", "I"), Value::Int(40204));
        assert_eq!(read("mSignal", "I"), Value::Int(9));
        assert_eq!(read("mTimestamp", "J"), Value::Long(1_790_139_658_000));
        assert_eq!(read("mImportance", "I"), Value::Int(400));
        drop(state);
        let Value::Object(Some(array)) = call_list(&jni, list, "toArray", "()[Ljava/lang/Object;", &[]).expect("toArray")
        else {
            panic!("an array");
        };
        match jni.state().handles.object_of(array) {
            Some(Object::ObjectArray { elements, .. }) => assert_eq!(elements, &vec![Some(exit)]),
            other => panic!("{other:?}"),
        }

        let unknown = crate::jni::ExitRecord { reason: 3, ..crate::jni::ExitRecord {
            pid: 1, reason: 0, status: 0, timestamp_ms: 0, importance: 0 } };
        assert!(unknown.reason_name().is_err(), "a reason this layer does not name is refused");
    }

    /// A JNI instance and the guest memory its slots read, for calling a slot directly.
    fn slot_fixture() -> (std::sync::Arc<Jni>, GuestMem) {
        let space = std::sync::Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(std::sync::Arc::clone(&space)).expect("a JNI instance");
        (jni, GuestMem::new(space))
    }

    /// Call the `JNIEnv` slot `name` with `x` as its arguments after the `JNIEnv*`, the way
    /// the run loop does once it has read that pointer.
    fn call_slot(jni: &Jni, mem: &GuestMem, name: &'static str, x: &[u64]) -> AbiResult<JniReturn> {
        struct Registers(Vec<u64>);
        impl crate::abi::ArgSource for Registers {
            fn x(&self, index: u32) -> u64 {
                self.0.get(index as usize).copied().unwrap_or(0)
            }
            fn v(&self, _index: u32) -> u128 {
                0
            }
            fn sp(&self) -> GuestAddr {
                0
            }
        }
        let registers = Registers(x.to_vec());
        let mut args = Args::new(&registers, mem, Blame::new(name, 0, 1));
        env_call(jni, 0, name, 0, &mut args, mem)
    }

    /// The refusal for an unimplemented slot has to carry the slot's own name, because the whole
    /// value of having 233 addresses instead of one is that the message says which.
    #[test]
    fn an_unimplemented_slot_refuses_with_its_own_name() {
        let error = refuse_slot("GetPrimitiveArrayCritical", 0x1234);
        match error {
            AbiError::JniRefused { function, address, detail } => {
                assert_eq!(function, "JNIEnv::GetPrimitiveArrayCritical");
                assert_eq!(address, 0x1234);
                assert!(detail.contains("GetPrimitiveArrayCritical"), "{detail}");
            }
            other => panic!("{other:?}"),
        }
    }

    const FMOD: &str = "org/fmod/FMOD";
    const CONTEXT: &str = "Landroid/content/Context;";
    const ACTIVITY: &str = "com/roblox/client/startup/MainGameActivity";

    /// `FMOD.checkInit()`, called the way the engine calls it.
    fn check_init(jni: &Jni) -> Value {
        let mut state = jni.state();
        let class = state.registry.find(FMOD).expect("declared");
        let method = state.registry.method(class, "checkInit", "()Z", true).expect("declared");
        let member = state.registry.member(method).expect("a member").clone();
        evaluate(&mut state, "CallStaticBooleanMethodV", 0, class, &member, None, &[])
            .expect("checkInit is answered")
    }

    /// `FMOD.gContext`, read the way `GetStaticObjectField` reads it.
    fn g_context(jni: &Jni) -> Value {
        let mut state = jni.state();
        let class = state.registry.find(FMOD).expect("declared");
        let field = state.registry.field(class, "gContext", CONTEXT, true).expect("declared");
        let member = state.registry.field_member(field).expect("a member").clone();
        static_field(&mut state, "GetStaticObjectField", 0, field, &member).expect("a read")
    }

    /// **`FMOD.checkInit()` is `gContext != null`, answered from what `gContext` holds.**
    ///
    /// The three wrong answers each fail a different line: a constant `true` (the device's answer
    /// hard-coded) fails the first; the refusal the gate died on, or a store that is not kept,
    /// fails the second; a field that is not anchored fails after the host's local goes; and a
    /// check that asked whether the field is *declared* rather than *set* fails the first and the
    /// last. The read and the test agree because they are one function.
    #[test]
    fn fmod_check_init_answers_whether_a_context_has_been_assigned() {
        let space = std::sync::Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(space).expect("a JNI instance");
        assert_eq!(check_init(&jni), Value::Boolean(false), "no FMOD.init has run: gContext is null");
        assert_eq!(g_context(&jni), Value::Object(None));

        let activity = jni.new_object(ACTIVITY).expect("an activity");
        jni.put_static_object(FMOD, "gContext", CONTEXT, activity).expect("FMOD.init's store");
        assert_eq!(check_init(&jni), Value::Boolean(true), "FMOD.init has run");
        let expected = jni.state().handles.resolve_id("test", 0, activity).expect("live");
        assert_eq!(g_context(&jni), Value::Object(Some(expected)), "the very object stored");

        // The host's local goes; the static's anchor keeps the object, as a Java static would.
        jni.state().handles.delete("DeleteLocalRef", 0, RefKind::Local, activity).expect("deleted");
        assert_eq!(check_init(&jni), Value::Boolean(true));
        assert_eq!(g_context(&jni), Value::Object(Some(expected)));

        // `FMOD.close()`'s `sput-object null`, and the anchor it held is released with it.
        let (references_before, objects_before, _) = jni.reference_stats();
        jni.put_static_object(FMOD, "gContext", CONTEXT, 0).expect("cleared");
        assert_eq!(check_init(&jni), Value::Boolean(false));
        assert_eq!(g_context(&jni), Value::Object(None));
        let (references_after, objects_after, _) = jni.reference_stats();
        assert_eq!(references_after + 1, references_before, "the anchor is released, not leaked");
        assert_eq!(objects_after + 1, objects_before, "and with it the last thing holding the object");
    }

    /// **A Java-assigned static takes what its type admits, and only a Java-assigned static can
    /// be written.** A `java/util/List` in a `Context` field is a state the verifier would never
    /// have let the app reach; `PlatformSystemDialogHandler.INSTANCE` is answered by its
    /// `<clinit>` and must not be overwritten by a statement; an undeclared field is not there.
    #[test]
    fn a_java_assigned_static_takes_only_what_its_type_admits() {
        let space = std::sync::Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(space).expect("a JNI instance");
        let list = jni.new_object("java/util/List").expect("a list");
        let error = jni.put_static_object(FMOD, "gContext", CONTEXT, list).expect_err("a List");
        assert!(matches!(error, AbiError::JniRefused { .. }), "{error:?}");
        assert_eq!(check_init(&jni), Value::Boolean(false), "a refused store stores nothing");

        let handler = jni.new_object(HANDLER).expect("a handler");
        let own = format!("L{HANDLER};");
        let error =
            jni.put_static_object(HANDLER, "INSTANCE", &own, handler).expect_err("StaticInstance");
        assert!(matches!(error, AbiError::JniRefused { .. }), "{error:?}");

        let activity = jni.new_object(ACTIVITY).expect("an activity");
        assert!(jni.put_static_object(FMOD, "gNoSuchField", CONTEXT, activity).is_err());
        assert!(jni.put_static_object(FMOD, "gContext", "Ljava/lang/Object;", activity).is_err());
        // And the declared chain is what admits the activity: `MainGameActivity` ->
        // `GameActivity` -> `Context`.
        jni.put_static_object(FMOD, "gContext", CONTEXT, activity).expect("an activity is a Context");
    }

    /// A primitive read of a Java-assigned object field is refused rather than answered with
    /// the object's handle as an `int`.
    #[test]
    fn a_java_assigned_static_is_not_read_as_a_primitive() {
        let space = std::sync::Arc::new(omni_mem::GuestSpace::new().expect("a guest space"));
        let jni = Jni::new(space).expect("a JNI instance");
        let mut state = jni.state();
        let class = state.registry.find(FMOD).expect("declared");
        let field = state.registry.field(class, "gContext", CONTEXT, true).expect("declared");
        let member = state.registry.field_member(field).expect("a member").clone();
        assert_eq!(member.answer, Answer::Assigned);
        let error = static_field(&mut state, "GetStaticIntField", 0, field, &member)
            .expect_err("an object field read as an int");
        assert!(matches!(error, AbiError::JniRefused { .. }), "{error:?}");
    }

    /// One marshaller for both paths, and this is what says a `jlong` keeps all 64 bits where an
    /// `i32` return would sign-extend 32.
    #[test]
    fn a_long_return_keeps_every_bit() {
        assert_eq!(JniReturn::Long(i64::MIN), JniReturn::Long(i64::MIN));
        assert_ne!(JniReturn::Long(-1), JniReturn::Int(-1));
        // `JNI_VERSION_1_6` as an `int` return, which is what `GetEnv` and `JNI_OnLoad` answer.
        assert_eq!(JniReturn::Int(JNI_VERSION_1_6), JniReturn::Int(0x0001_0006));
        assert_eq!(JNI_ERR, -1);
        assert_eq!(JNI_EDETACHED, -2);
        assert_eq!(JNI_COMMIT, 1);
        assert_eq!(JNI_ABORT, 2);
    }
}
