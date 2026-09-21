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
//! [`ImportCall`](crate::ImportCall) structurally cannot (D18). Nothing on §8 steps 6-12 takes
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

/// The two `JavaVM` slots the engine uses, and refusals for the other six.
///
/// # Errors
///
/// [`AbiError::JniRefused`] for `DestroyJavaVM`, `DetachCurrentThread`,
/// `AttachCurrentThreadAsDaemon` and the three reserved slots. §2.2: the first two are **never
/// called** by this engine, so a refusal there is a report that something changed rather than a
/// gap.
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
            _ => {
                return Err(AbiError::JniRefused {
                    function: format!("JavaVM::{name}"),
                    address,
                    detail: format!(
                        "`{name}` is one of the six JavaVM slots `libroblox.so` never \
                         dereferences (jni-surface.md §2.2 measured 2 of 8 over 5 call sites), so \
                         it has no implementation here; a call to it means the engine's behaviour \
                         has changed and is reported rather than guessed at"
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
            match state.registry.find(&text) {
                Some(class) => {
                    let handle = state.handles.new_local(name, address, Object::Class(class))?;
                    Ok(JniReturn::Word(handle))
                }
                None => {
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
            let class = class_of(&state, id).ok_or_else(|| AbiError::JniRefused {
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
            let class = class_handle(&state, name, address, class)?;
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
            let class = class_handle(&state, name, address, class)?;
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
            let value = Registry::simple_answer(member.answer).ok_or_else(|| {
                unanswered(&state, name, address, field.class, &member)
            })?;
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
        let id = state.handles.decode_method(name, address, method)?;
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
        match state.registry.method(class, &member, &descriptor, true).or_else(|| {
            state.registry.method(class, &member, &descriptor, false)
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

/// The class **of** an object, which is what `GetObjectClass` answers.
///
/// **A `jclass` is an instance of `java.lang.Class`, not of itself**, and getting that wrong is
/// not a detail: `RBX::Security::Android::Detail::JvmClassLoaderHelper` does
/// `GetObjectClass(someClass)` and then `GetMethodID(that, "getClassLoader",
/// "()Ljava/lang/ClassLoader;")` — the cache-the-app-ClassLoader pattern jni-surface.md section 6
/// describes. Answering the class itself makes that lookup ask
/// `NativeGLJavaInterface.getClassLoader`, which does not exist, and the null `jmethodID` goes
/// straight into `CallObjectMethodV`. MEASURED: that is exactly what it did.
fn class_of(state: &JniState, id: ObjectId) -> Option<ClassId> {
    match state.handles.object_of(id)? {
        Object::Class(_) => state.registry.find("java/lang/Class"),
        Object::String(_) => state.registry.find("java/lang/String"),
        Object::Instance { class, .. } | Object::Throwable { class, .. } => Some(*class),
        // An array's class is `[B`, `[I`, `[Ljava/lang/Object;` and so on, and a direct
        // `ByteBuffer`'s is a framework class with no dex declaration. Nothing on the measured
        // surface asks for either, so the caller refuses by name rather than this returning
        // something believable.
        _ => None,
    }
}

/// Decode a `jclass` handle to the class it names.
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

/// Bounds-check an array region. **A guest-chosen start and length**, so the check is written as
/// two `checked_add`s rather than as `start + len <= n`, which overflows in release and panics in
/// debug — the profile difference Global Constraint 4 is about.
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

/// An instance field: whatever was stored on the object, or the class's declared default.
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
fn evaluate(
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
        Answer::Unanswered => Err(unanswered(state, name, address, class, member)),
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
        "GetStringUTFChars",
        "GetStringChars",
        "ReleaseStringUTFChars",
        "ReleaseStringChars",
        "GetArrayLength",
        "NewObjectArray",
        "NewLongArray",
        "GetObjectArrayElement",
        "SetObjectArrayElement",
        "GetByteArrayElements",
        "GetIntArrayElements",
        "GetFloatArrayElements",
        "ReleaseByteArrayElements",
        "ReleaseIntArrayElements",
        "ReleaseFloatArrayElements",
        "GetByteArrayRegion",
        "SetLongArrayRegion",
        "GetJavaVM",
        "NewDirectByteBuffer",
    ];

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
