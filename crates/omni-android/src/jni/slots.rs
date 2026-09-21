//! The two function tables, in `jni.h` declaration order — **the specification, as a table**.
//!
//! `ARCHITECTURE.md` section 5's rule is that the import list is the specification. One level up,
//! for JNI, the *slot* list is: `libroblox.so` reaches a JNI function by loading
//! `functions->slot` out of a table whose layout is fixed by the NDK's
//! `struct JNINativeInterface`, so the layout is not a choice this layer makes and getting one
//! entry out of order silently routes every call past it to the wrong handler.
//!
//! # Why the whole table is here and not only the 59 that are used
//!
//! `jni-surface.md` §0: **59 of 233 slots are ever dereferenced** and 170 never are (4 more are
//! `reserved` and uncallable by definition). The unused ones are still *present* in the table the
//! guest loads from, so each needs an address — and the only honest address is one whose call
//! refuses **naming itself**. Leaving a slot null would turn a call the analysis said cannot
//! happen into a branch to zero with nothing attached to it, which is exactly the failure shape
//! the thunk region exists to replace (see [`crate::region`]).
//!
//! So: 233 slots, 233 thunk addresses, 233 names. The ones this layer implements are serviced;
//! the rest refuse with their own name in the message, and [`ENV_USED`] is the golden list that
//! says which is which.
//!
//! # The offsets validate themselves
//!
//! `jni-surface.md` §1 step 5 records that the analysis's offset→function mapping validated
//! itself: every one of the 943 detected call sites that carries a literal string argument lands
//! on exactly one of six offsets, and each of those six has the argument *shape* a string
//! belongs in. [`ENV_USED`] carries the offsets that analysis reported, and
//! `the_env_table_agrees_with_the_measured_offsets` compares them against this array's own
//! indices. A transposition anywhere in the 233 moves at least one of the 59 and fails.

/// Bytes per function-table entry. A guest pointer.
pub const SLOT_BYTES: usize = 8;

/// `JNI_VERSION_1_6`, which is what `JNI_OnLoad` must return and what the engine asks `GetEnv`
/// for.
///
/// VERIFIED at `0x2174dac`: `mov w2,#6; movk w2,#1,lsl#16` (`jni-surface.md` §2.2).
pub const JNI_VERSION_1_6: i32 = 0x0001_0006;

/// `JNI_OK`.
pub const JNI_OK: i32 = 0;
/// `JNI_ERR`.
pub const JNI_ERR: i32 = -1;
/// `JNI_EDETACHED` — the value `GetEnv` answers on a thread that has never attached, and the one
/// the scoped-attach helper at `0x2174c04` tests for with `cmn w0,#2`.
pub const JNI_EDETACHED: i32 = -2;
/// `JNI_EVERSION`.
pub const JNI_EVERSION: i32 = -3;

/// `JNI_FALSE` / `JNI_TRUE`, as the `jboolean` the guest reads out of `W0`.
pub const JNI_FALSE: u32 = 0;
/// See [`JNI_FALSE`].
pub const JNI_TRUE: u32 = 1;

/// `Release<Type>ArrayElements` mode: copy back and free the buffer.
pub const JNI_RELEASE_COPY_BACK: i32 = 0;
/// `JNI_COMMIT` — copy back, keep the buffer.
pub const JNI_COMMIT: i32 = 1;
/// `JNI_ABORT` — free the buffer, discard whatever the guest wrote into it.
pub const JNI_ABORT: i32 = 2;

/// Every `JNINativeInterface` member, in declaration order. Index × 8 is the guest offset.
///
/// 233 entries; `233 * 8 = 0x748`, which is the table size `jni-surface.md` §1 step 5 reports.
pub static ENV_SLOTS: [&str; 233] = [
    "reserved0",
    "reserved1",
    "reserved2",
    "reserved3",
    "GetVersion",
    "DefineClass",
    "FindClass",
    "FromReflectedMethod",
    "FromReflectedField",
    "ToReflectedMethod",
    "GetSuperclass",
    "IsAssignableFrom",
    "ToReflectedField",
    "Throw",
    "ThrowNew",
    "ExceptionOccurred",
    "ExceptionDescribe",
    "ExceptionClear",
    "FatalError",
    "PushLocalFrame",
    "PopLocalFrame",
    "NewGlobalRef",
    "DeleteGlobalRef",
    "DeleteLocalRef",
    "IsSameObject",
    "NewLocalRef",
    "EnsureLocalCapacity",
    "AllocObject",
    "NewObject",
    "NewObjectV",
    "NewObjectA",
    "GetObjectClass",
    "IsInstanceOf",
    "GetMethodID",
    "CallObjectMethod",
    "CallObjectMethodV",
    "CallObjectMethodA",
    "CallBooleanMethod",
    "CallBooleanMethodV",
    "CallBooleanMethodA",
    "CallByteMethod",
    "CallByteMethodV",
    "CallByteMethodA",
    "CallCharMethod",
    "CallCharMethodV",
    "CallCharMethodA",
    "CallShortMethod",
    "CallShortMethodV",
    "CallShortMethodA",
    "CallIntMethod",
    "CallIntMethodV",
    "CallIntMethodA",
    "CallLongMethod",
    "CallLongMethodV",
    "CallLongMethodA",
    "CallFloatMethod",
    "CallFloatMethodV",
    "CallFloatMethodA",
    "CallDoubleMethod",
    "CallDoubleMethodV",
    "CallDoubleMethodA",
    "CallVoidMethod",
    "CallVoidMethodV",
    "CallVoidMethodA",
    "CallNonvirtualObjectMethod",
    "CallNonvirtualObjectMethodV",
    "CallNonvirtualObjectMethodA",
    "CallNonvirtualBooleanMethod",
    "CallNonvirtualBooleanMethodV",
    "CallNonvirtualBooleanMethodA",
    "CallNonvirtualByteMethod",
    "CallNonvirtualByteMethodV",
    "CallNonvirtualByteMethodA",
    "CallNonvirtualCharMethod",
    "CallNonvirtualCharMethodV",
    "CallNonvirtualCharMethodA",
    "CallNonvirtualShortMethod",
    "CallNonvirtualShortMethodV",
    "CallNonvirtualShortMethodA",
    "CallNonvirtualIntMethod",
    "CallNonvirtualIntMethodV",
    "CallNonvirtualIntMethodA",
    "CallNonvirtualLongMethod",
    "CallNonvirtualLongMethodV",
    "CallNonvirtualLongMethodA",
    "CallNonvirtualFloatMethod",
    "CallNonvirtualFloatMethodV",
    "CallNonvirtualFloatMethodA",
    "CallNonvirtualDoubleMethod",
    "CallNonvirtualDoubleMethodV",
    "CallNonvirtualDoubleMethodA",
    "CallNonvirtualVoidMethod",
    "CallNonvirtualVoidMethodV",
    "CallNonvirtualVoidMethodA",
    "GetFieldID",
    "GetObjectField",
    "GetBooleanField",
    "GetByteField",
    "GetCharField",
    "GetShortField",
    "GetIntField",
    "GetLongField",
    "GetFloatField",
    "GetDoubleField",
    "SetObjectField",
    "SetBooleanField",
    "SetByteField",
    "SetCharField",
    "SetShortField",
    "SetIntField",
    "SetLongField",
    "SetFloatField",
    "SetDoubleField",
    "GetStaticMethodID",
    "CallStaticObjectMethod",
    "CallStaticObjectMethodV",
    "CallStaticObjectMethodA",
    "CallStaticBooleanMethod",
    "CallStaticBooleanMethodV",
    "CallStaticBooleanMethodA",
    "CallStaticByteMethod",
    "CallStaticByteMethodV",
    "CallStaticByteMethodA",
    "CallStaticCharMethod",
    "CallStaticCharMethodV",
    "CallStaticCharMethodA",
    "CallStaticShortMethod",
    "CallStaticShortMethodV",
    "CallStaticShortMethodA",
    "CallStaticIntMethod",
    "CallStaticIntMethodV",
    "CallStaticIntMethodA",
    "CallStaticLongMethod",
    "CallStaticLongMethodV",
    "CallStaticLongMethodA",
    "CallStaticFloatMethod",
    "CallStaticFloatMethodV",
    "CallStaticFloatMethodA",
    "CallStaticDoubleMethod",
    "CallStaticDoubleMethodV",
    "CallStaticDoubleMethodA",
    "CallStaticVoidMethod",
    "CallStaticVoidMethodV",
    "CallStaticVoidMethodA",
    "GetStaticFieldID",
    "GetStaticObjectField",
    "GetStaticBooleanField",
    "GetStaticByteField",
    "GetStaticCharField",
    "GetStaticShortField",
    "GetStaticIntField",
    "GetStaticLongField",
    "GetStaticFloatField",
    "GetStaticDoubleField",
    "SetStaticObjectField",
    "SetStaticBooleanField",
    "SetStaticByteField",
    "SetStaticCharField",
    "SetStaticShortField",
    "SetStaticIntField",
    "SetStaticLongField",
    "SetStaticFloatField",
    "SetStaticDoubleField",
    "NewString",
    "GetStringLength",
    "GetStringChars",
    "ReleaseStringChars",
    "NewStringUTF",
    "GetStringUTFLength",
    "GetStringUTFChars",
    "ReleaseStringUTFChars",
    "GetArrayLength",
    "NewObjectArray",
    "GetObjectArrayElement",
    "SetObjectArrayElement",
    "NewBooleanArray",
    "NewByteArray",
    "NewCharArray",
    "NewShortArray",
    "NewIntArray",
    "NewLongArray",
    "NewFloatArray",
    "NewDoubleArray",
    "GetBooleanArrayElements",
    "GetByteArrayElements",
    "GetCharArrayElements",
    "GetShortArrayElements",
    "GetIntArrayElements",
    "GetLongArrayElements",
    "GetFloatArrayElements",
    "GetDoubleArrayElements",
    "ReleaseBooleanArrayElements",
    "ReleaseByteArrayElements",
    "ReleaseCharArrayElements",
    "ReleaseShortArrayElements",
    "ReleaseIntArrayElements",
    "ReleaseLongArrayElements",
    "ReleaseFloatArrayElements",
    "ReleaseDoubleArrayElements",
    "GetBooleanArrayRegion",
    "GetByteArrayRegion",
    "GetCharArrayRegion",
    "GetShortArrayRegion",
    "GetIntArrayRegion",
    "GetLongArrayRegion",
    "GetFloatArrayRegion",
    "GetDoubleArrayRegion",
    "SetBooleanArrayRegion",
    "SetByteArrayRegion",
    "SetCharArrayRegion",
    "SetShortArrayRegion",
    "SetIntArrayRegion",
    "SetLongArrayRegion",
    "SetFloatArrayRegion",
    "SetDoubleArrayRegion",
    "RegisterNatives",
    "UnregisterNatives",
    "MonitorEnter",
    "MonitorExit",
    "GetJavaVM",
    "GetStringRegion",
    "GetStringUTFRegion",
    "GetPrimitiveArrayCritical",
    "ReleasePrimitiveArrayCritical",
    "GetStringCritical",
    "ReleaseStringCritical",
    "NewWeakGlobalRef",
    "DeleteWeakGlobalRef",
    "ExceptionCheck",
    "NewDirectByteBuffer",
    "GetDirectBufferAddress",
    "GetDirectBufferCapacity",
    "GetObjectRefType",
];

/// Every `JNIInvokeInterface` member, in declaration order. Index × 8 is the guest offset.
pub static VM_SLOTS: [&str; 8] = [
    "reserved0",
    "reserved1",
    "reserved2",
    "DestroyJavaVM",
    "AttachCurrentThread",
    "DetachCurrentThread",
    "GetEnv",
    "AttachCurrentThreadAsDaemon",
];

/// The 59 `JNINativeInterface` slots `libroblox.so` actually dereferences, with the **guest byte
/// offset and call-site count the analysis measured** (`jni-surface-lists.txt` Section A1).
///
/// This is golden data, not a derived list: the offsets came out of the binary and the indices in
/// [`ENV_SLOTS`] come out of `jni.h`'s declaration order, so comparing them checks the one thing
/// that cannot be checked by reading either alone.
pub static ENV_USED: [(usize, &str, u32); 59] = [
    (0x30, "FindClass", 64),
    (0x68, "Throw", 1),
    (0x70, "ThrowNew", 2),
    (0x78, "ExceptionOccurred", 8),
    (0x80, "ExceptionDescribe", 9),
    (0x88, "ExceptionClear", 25),
    (0xa8, "NewGlobalRef", 35),
    (0xb0, "DeleteGlobalRef", 6),
    (0xb8, "DeleteLocalRef", 45),
    (0xc0, "IsSameObject", 11),
    (0xc8, "NewLocalRef", 1),
    (0xe8, "NewObjectV", 1),
    (0xf8, "GetObjectClass", 92),
    (0x108, "GetMethodID", 161),
    (0x118, "CallObjectMethodV", 1),
    (0x130, "CallBooleanMethodV", 1),
    (0x190, "CallIntMethodV", 1),
    (0x1a8, "CallLongMethodV", 1),
    (0x1c0, "CallFloatMethodV", 1),
    (0x1d8, "CallDoubleMethodV", 1),
    (0x1f0, "CallVoidMethodV", 1),
    (0x2f0, "GetFieldID", 69),
    (0x2f8, "GetObjectField", 56),
    (0x300, "GetBooleanField", 9),
    (0x320, "GetIntField", 17),
    (0x328, "GetLongField", 15),
    (0x330, "GetFloatField", 3),
    (0x338, "GetDoubleField", 4),
    (0x388, "GetStaticMethodID", 84),
    (0x398, "CallStaticObjectMethodV", 1),
    (0x410, "CallStaticIntMethodV", 1),
    (0x470, "CallStaticVoidMethodV", 1),
    (0x480, "GetStaticFieldID", 4),
    (0x488, "GetStaticObjectField", 4),
    (0x518, "NewString", 1),
    (0x520, "GetStringLength", 1),
    (0x528, "GetStringChars", 1),
    (0x530, "ReleaseStringChars", 1),
    (0x538, "NewStringUTF", 84),
    (0x548, "GetStringUTFChars", 27),
    (0x550, "ReleaseStringUTFChars", 13),
    (0x558, "GetArrayLength", 11),
    (0x560, "NewObjectArray", 3),
    (0x568, "GetObjectArrayElement", 8),
    (0x570, "SetObjectArrayElement", 3),
    (0x5a0, "NewLongArray", 1),
    (0x5c0, "GetByteArrayElements", 4),
    (0x5d8, "GetIntArrayElements", 1),
    (0x5e8, "GetFloatArrayElements", 2),
    (0x600, "ReleaseByteArrayElements", 4),
    (0x618, "ReleaseIntArrayElements", 1),
    (0x628, "ReleaseFloatArrayElements", 2),
    (0x640, "GetByteArrayRegion", 1),
    (0x6a0, "SetLongArrayRegion", 1),
    (0x6b8, "RegisterNatives", 2),
    (0x6d8, "GetJavaVM", 1),
    (0x710, "NewWeakGlobalRef", 5),
    (0x720, "ExceptionCheck", 28),
    (0x728, "NewDirectByteBuffer", 1),
];

/// The 2 `JNIInvokeInterface` slots the engine dereferences, with their measured offsets and call
/// counts (`jni-surface-lists.txt` Section A2).
pub static VM_USED: [(usize, &str, u32); 2] =
    [(0x20, "AttachCurrentThread", 2), (0x30, "GetEnv", 3)];

/// The index of a member of [`ENV_SLOTS`], or `None`.
#[must_use]
pub fn env_index(name: &str) -> Option<usize> {
    ENV_SLOTS.iter().position(|slot| *slot == name)
}

/// The index of a member of [`VM_SLOTS`], or `None`.
#[must_use]
pub fn vm_index(name: &str) -> Option<usize> {
    VM_SLOTS.iter().position(|slot| *slot == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// **The assertion this module exists for.** The offsets in [`ENV_USED`] were read out of
    /// `libroblox.so`; the indices in [`ENV_SLOTS`] come from `jni.h`. If the declaration order
    /// here is wrong by one anywhere at or before a used slot, that slot's offset moves and this
    /// fails — which is the only way to catch a transposition without an NDK header on this
    /// machine.
    #[test]
    fn the_env_table_agrees_with_the_measured_offsets() {
        for (offset, name, _) in ENV_USED {
            assert_eq!(offset % SLOT_BYTES, 0, "{name} at {offset:#x} is not slot-aligned");
            let index = offset / SLOT_BYTES;
            assert_eq!(
                ENV_SLOTS[index], name,
                "slot {index} ({offset:#x}) is `{}` here and `{name}` in the binary",
                ENV_SLOTS[index]
            );
        }
        for (offset, name, _) in VM_USED {
            assert_eq!(VM_SLOTS[offset / SLOT_BYTES], name);
        }
    }

    /// `jni-surface.md` §0: 59 of 233, and the table is `0x748` bytes.
    #[test]
    fn the_headline_numbers_are_what_the_tables_hold() {
        assert_eq!(ENV_SLOTS.len(), 233);
        assert_eq!(ENV_SLOTS.len() * SLOT_BYTES, 0x748);
        assert_eq!(ENV_USED.len(), 59);
        assert_eq!(VM_SLOTS.len(), 8);
        assert_eq!(VM_USED.len(), 2);
        // 170 never touched, 4 reserved: 59 + 170 + 4 = 233.
        assert_eq!(ENV_SLOTS.len() - ENV_USED.len() - 4, 170);
    }

    /// A duplicate name would make [`env_index`] answer for the wrong slot, and every handler
    /// dispatches through it.
    #[test]
    fn every_slot_name_is_distinct() {
        let distinct: BTreeSet<&str> = ENV_SLOTS.iter().copied().collect();
        assert_eq!(distinct.len(), ENV_SLOTS.len());
        let distinct: BTreeSet<&str> = VM_SLOTS.iter().copied().collect();
        assert_eq!(distinct.len(), VM_SLOTS.len());
    }

    /// The 943 figure in `jni-surface.md` §0 is the sum of Section A1's call-site counts. Summing
    /// them here is a check on the transcription: a mistyped count that still summed to 943 would
    /// need a compensating second error.
    #[test]
    fn the_call_site_counts_sum_to_the_headline_943() {
        let total: u32 = ENV_USED.iter().map(|(_, _, sites)| sites).sum();
        assert_eq!(total, 943);
        let total: u32 = VM_USED.iter().map(|(_, _, sites)| sites).sum();
        assert_eq!(total, 5, "§2.2: 2 distinct JavaVM slots over 5 sites");
    }

    /// The four reserved slots are uncallable by definition and the analysis found **zero** hits
    /// on them (`jni-surface.md` §1: the hand decoder's 9 false positives there were what proved
    /// it wrong). Nothing in [`ENV_USED`] may name one.
    #[test]
    fn no_used_slot_is_a_reserved_one() {
        for (offset, name, _) in ENV_USED {
            assert!(offset / SLOT_BYTES >= 4, "{name} lands on a reserved slot");
        }
    }
}
