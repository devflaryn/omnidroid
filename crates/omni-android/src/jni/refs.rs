//! Handles: `jobject`, `jclass`, `jstring`, `jarray`, `jmethodID`, `jfieldID` — and what makes
//! one the guest hands back trustworthy.
//!
//! # Every one of these is untrusted input
//!
//! Global Constraint 11, and the brief's own words: the APK is cheat-injected (D6) and every
//! `jobject`, `jstring`, `jmethodID` and array index the guest hands back is a 64-bit value this
//! layer did not necessarily produce. JNI's own types are opaque pointers, so there is nothing in
//! the ABI that constrains what arrives. A handle is therefore **not** a pointer here: it is an
//! encoded, checked index into a table this module owns, and a value that does not decode is a
//! typed error naming the JNI function — never a plausible wrong answer.
//!
//! # What the encoding does and does not buy, stated exactly
//!
//! A handle is `tag | kind | check | index`:
//!
//! | bits | holds |
//! |---|---|
//! | 56-63 | [`REF_TAG`], [`METHOD_TAG`] or [`FIELD_TAG`] — which of the three tables this is for |
//! | 32-55 | a **check word**: the slot's generation mixed with a per-instance [`cookie`](Handles::cookie) |
//! | 0-31 | the slot index |
//!
//! * **Out of range is caught**, because the index is bounds-checked against the table.
//! * **Stale is caught**, because a slot's generation advances every time it is freed and reused,
//!   so a handle the guest deleted and used again does not match. This is the one that matters:
//!   `DeleteLocalRef` has 45 call sites and `DeleteGlobalRef` 6, so use-after-delete is a shape
//!   the engine can actually produce.
//! * **A value the guest invented is caught with probability `1 - 2^-24` per attempt**, because
//!   the check word mixes in a cookie drawn once per instance. That is a nuisance bound, not a
//!   security boundary, and it is written here rather than implied.
//! * **What it does NOT buy**, said plainly: a guest that already holds a valid handle to object
//!   A and forges one for object B of the same instance gains nothing it did not have — every
//!   object in the table is one this layer created and defined, none of them is host memory, and
//!   resolving one yields a host-side value rather than a pointer. The property being defended is
//!   "no panic, no out-of-bounds, no fabricated answer", not isolation between two objects of the
//!   same guest.
//!
//! # No collector, and therefore no weak reference that goes null
//!
//! There is no garbage collector here and nothing reclaims a `NewWeakGlobalRef`'s object while
//! the instance lives, so a weak global reference **never becomes null**. That is a deliberate
//! difference from ART and it is the safe direction: the engine's five `NewWeakGlobalRef` sites
//! cache `jclass` values, and a weak class reference going null under ART is what
//! `IsSameObject(ref, NULL)` is tested for. Answering "still alive" is the answer a device gives
//! for a class the app still references.

use std::collections::BTreeMap;

use omni_mem::GuestAddr;

use crate::error::{AbiError, AbiResult};

use super::classes::{ClassId, FieldId, MethodId};
use super::values::JavaString;

/// The top byte of a `jobject`-family handle.
pub const REF_TAG: u64 = 0xa5;
/// The top byte of a `jmethodID`.
pub const METHOD_TAG: u64 = 0xb6;
/// The top byte of a `jfieldID`.
pub const FIELD_TAG: u64 = 0xc7;

/// Bits of the check word, which is the generation mixed with the instance cookie.
const CHECK_BITS: u32 = 24;
const CHECK_MASK: u64 = (1 << CHECK_BITS) - 1;

/// How many references one instance can have live at once.
///
/// **A policy number.** The startup path holds tens: `JNI_OnLoad` caches one class per interface
/// and the registration helpers add one `jmethodID` each (those are not references at all). The
/// cap exists because `NewGlobalRef` has 35 call sites and `NewStringUTF` 84, and a guest that
/// leaks one per frame must hit a refusal naming the function rather than growing a host `Vec`
/// until the process dies. A table that fills is [`AbiError::JniRefused`], never a recycled slot.
pub const MAX_REFERENCES: usize = 65_536;

/// How many objects one instance can have live at once. See [`MAX_REFERENCES`]; an object is
/// freed when the last non-weak reference to it goes.
pub const MAX_OBJECTS: usize = 65_536;

/// Which reference table a handle came out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefKind {
    /// A local reference: `FindClass`, `NewStringUTF`, `GetObjectField`, a returned object.
    Local,
    /// A global reference: `NewGlobalRef`.
    Global,
    /// A weak global reference: `NewWeakGlobalRef`.
    Weak,
}

impl RefKind {
    /// The nibble stored in a handle.
    const fn code(self) -> u64 {
        match self {
            RefKind::Local => 1,
            RefKind::Global => 2,
            RefKind::Weak => 3,
        }
    }

    fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(RefKind::Local),
            2 => Some(RefKind::Global),
            3 => Some(RefKind::Weak),
            _ => None,
        }
    }

    /// What `GetObjectRefType` would answer, and what an error message calls it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            RefKind::Local => "a local reference",
            RefKind::Global => "a global reference",
            RefKind::Weak => "a weak global reference",
        }
    }
}

/// An object this layer created, identified by its slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectId {
    index: u32,
    generation: u32,
}

impl ObjectId {
    /// The slot index, for a diagnostic.
    #[must_use]
    pub fn index(self) -> u32 {
        self.index
    }
}

/// A Java object, as this layer models one.
///
/// Every variant is host-defined data. **None of them is or contains a host pointer**, which is
/// what makes a forged handle a nuisance rather than a memory-safety problem: the worst outcome
/// of resolving one is reading another object of the same guest instance.
#[derive(Debug, Clone)]
pub enum Object {
    /// A `jclass`.
    Class(ClassId),
    /// A `java.lang.String`, held as UTF-16 because that is what `GetStringChars` and
    /// `GetStringLength` are defined over.
    String(JavaString),
    /// An instance of a declared class, with whatever fields the host has given it.
    Instance {
        /// Its class.
        class: ClassId,
        /// Field values, by field.
        fields: BTreeMap<FieldId, super::values::Value>,
    },
    /// `jbyteArray`.
    ByteArray(Vec<i8>),
    /// `jintArray`.
    IntArray(Vec<i32>),
    /// `jlongArray`.
    LongArray(Vec<i64>),
    /// `jfloatArray`.
    FloatArray(Vec<f32>),
    /// `jobjectArray`: the element class and the elements, each of which may be null.
    ObjectArray {
        /// The array's element class.
        element: ClassId,
        /// Its elements.
        elements: Vec<Option<ObjectId>>,
    },
    /// What `NewDirectByteBuffer` handed back: a window onto guest memory the **guest** owns.
    ///
    /// The address is kept as a number and never dereferenced by this module. Anything that reads
    /// through it goes via [`GuestMem`](crate::mem::GuestMem), which checks.
    DirectByteBuffer {
        /// The guest address the engine passed.
        address: GuestAddr,
        /// Its capacity in bytes, as the engine stated it.
        capacity: i64,
    },
    /// A `java.lang.Throwable`: what `ThrowNew` and a failed lookup produce.
    Throwable {
        /// The exception class.
        class: ClassId,
        /// Its message.
        message: String,
    },
}

impl Object {
    /// What an error message calls this, so a type mismatch names both sides.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Object::Class(_) => "a jclass",
            Object::String(_) => "a java.lang.String",
            Object::Instance { .. } => "an object",
            Object::ByteArray(_) => "a byte[]",
            Object::IntArray(_) => "an int[]",
            Object::LongArray(_) => "a long[]",
            Object::FloatArray(_) => "a float[]",
            Object::ObjectArray { .. } => "an Object[]",
            Object::DirectByteBuffer { .. } => "a direct java.nio.ByteBuffer",
            Object::Throwable { .. } => "a java.lang.Throwable",
        }
    }
}

#[derive(Debug)]
struct ObjectSlot {
    generation: u32,
    /// `None` once the slot is free; the generation still advances so a stale id is caught.
    object: Option<Object>,
    /// Non-weak references to it. Weak ones deliberately do not count — see the module docs.
    strong: u32,
}

#[derive(Debug)]
struct RefSlot {
    generation: u32,
    live: Option<(RefKind, ObjectId)>,
}

/// The three handle tables: objects, references to them, and the free lists.
#[derive(Debug)]
pub struct Handles {
    objects: Vec<ObjectSlot>,
    free_objects: Vec<u32>,
    refs: Vec<RefSlot>,
    free_refs: Vec<u32>,
    cookie: u64,
    /// The high-water mark of live references, which is what says whether the cap is close.
    peak_refs: usize,
}

impl Handles {
    /// A fresh set of tables with `cookie` mixed into every handle.
    #[must_use]
    pub fn new(cookie: u64) -> Self {
        Self {
            objects: Vec::new(),
            free_objects: Vec::new(),
            refs: Vec::new(),
            free_refs: Vec::new(),
            cookie: cookie & CHECK_MASK,
            peak_refs: 0,
        }
    }

    /// The per-instance mixing word. See the module docs for what it is and is not for.
    #[must_use]
    pub fn cookie(&self) -> u64 {
        self.cookie
    }

    /// How many references are live.
    #[must_use]
    pub fn live_references(&self) -> usize {
        self.refs.iter().filter(|slot| slot.live.is_some()).count()
    }

    /// The most references that have been live at once.
    #[must_use]
    pub fn peak_references(&self) -> usize {
        self.peak_refs
    }

    /// How many objects are live.
    #[must_use]
    pub fn live_objects(&self) -> usize {
        self.objects.iter().filter(|slot| slot.object.is_some()).count()
    }

    fn check_word(&self, generation: u32) -> u64 {
        (u64::from(generation) ^ self.cookie) & CHECK_MASK
    }

    /// Put `object` in the table and hand back a **local** reference to it.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] if either table is at its cap, naming `function` and the cap.
    pub fn new_local(
        &mut self,
        function: &str,
        address: GuestAddr,
        object: Object,
    ) -> AbiResult<u64> {
        let id = self.create(function, address, object)?;
        self.reference_to(function, address, RefKind::Local, id)
    }

    /// Put `object` in the table with **no** reference to it yet.
    ///
    /// The caller owes it a reference before it returns: an object with no references is not
    /// freed (nothing collects here), so leaving one is a leak rather than a dangling id. Used by
    /// the two-step paths, where the object is created and the reference is made by whichever
    /// return marshaller runs — which is what stops those paths creating **two** references and
    /// leaking one of them.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] if the object table is at its cap.
    pub fn create(
        &mut self,
        function: &str,
        address: GuestAddr,
        object: Object,
    ) -> AbiResult<ObjectId> {
        self.insert_object(function, address, object)
    }

    fn insert_object(
        &mut self,
        function: &str,
        address: GuestAddr,
        object: Object,
    ) -> AbiResult<ObjectId> {
        let index = match self.free_objects.pop() {
            Some(index) => index,
            None => {
                if self.objects.len() >= MAX_OBJECTS {
                    return Err(AbiError::JniRefused {
                        function: function.to_string(),
                        address,
                        detail: format!(
                            "this instance already holds {MAX_OBJECTS} live Java objects, which \
                             is the cap; a guest that keeps creating them without deleting the \
                             references gets this refusal rather than an unbounded host \
                             allocation"
                        ),
                    });
                }
                self.objects.push(ObjectSlot { generation: 1, object: None, strong: 0 });
                u32::try_from(self.objects.len() - 1).expect("MAX_OBJECTS fits in u32")
            }
        };
        let slot = &mut self.objects[index as usize];
        slot.object = Some(object);
        slot.strong = 0;
        Ok(ObjectId { index, generation: slot.generation })
    }

    /// Make a reference of `kind` to an object already in the table.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] if the reference table is at its cap.
    pub fn reference_to(
        &mut self,
        function: &str,
        address: GuestAddr,
        kind: RefKind,
        id: ObjectId,
    ) -> AbiResult<u64> {
        let index = match self.free_refs.pop() {
            Some(index) => index,
            None => {
                if self.refs.len() >= MAX_REFERENCES {
                    return Err(AbiError::JniRefused {
                        function: function.to_string(),
                        address,
                        detail: format!(
                            "this instance already holds {MAX_REFERENCES} live JNI references, \
                             which is the cap"
                        ),
                    });
                }
                self.refs.push(RefSlot { generation: 1, live: None });
                u32::try_from(self.refs.len() - 1).expect("MAX_REFERENCES fits in u32")
            }
        };
        let generation = self.refs[index as usize].generation;
        self.refs[index as usize].live = Some((kind, id));
        if kind != RefKind::Weak {
            self.objects[id.index as usize].strong += 1;
        }
        let live = self.live_references();
        self.peak_refs = self.peak_refs.max(live);
        Ok(REF_TAG << 56 | kind.code() << 52 | self.check_word(generation) << 28 | u64::from(index))
    }

    /// Make another reference of `kind` to whatever `handle` names.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniBadHandle`] if `handle` is not one this table issued; [`AbiError::JniRefused`]
    /// if the table is full.
    pub fn duplicate(
        &mut self,
        function: &str,
        address: GuestAddr,
        kind: RefKind,
        handle: u64,
    ) -> AbiResult<u64> {
        let id = self.resolve_id(function, address, handle)?;
        self.reference_to(function, address, kind, id)
    }

    /// Decode `handle` to the object it names.
    ///
    /// A null handle is **not** decoded here: every caller that tolerates null has to say so,
    /// because `FindClass` returning null is `jni-surface.md` §8.1's third-ranked failure mode and
    /// a helper that silently accepted it would put that failure back.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniBadHandle`], naming the function, the raw value and why it did not decode.
    pub fn resolve_id(
        &self,
        function: &str,
        address: GuestAddr,
        handle: u64,
    ) -> AbiResult<ObjectId> {
        let bad = |why: &str| AbiError::JniBadHandle {
            function: function.to_string(),
            address,
            kind: "jobject",
            handle,
            why: why.to_string(),
        };
        if handle == 0 {
            return Err(bad("it is null, and this call has no defined behaviour for a null object"));
        }
        if handle >> 56 != REF_TAG {
            return Err(bad(
                "its top byte is not the reference tag, so it was never issued by this instance",
            ));
        }
        if RefKind::from_code((handle >> 52) & 0xf).is_none() {
            return Err(bad("its kind field is not one of local, global or weak"));
        }
        let index = (handle & 0xfff_ffff) as usize;
        let slot = self
            .refs
            .get(index)
            .ok_or_else(|| bad("its index is past the end of the reference table"))?;
        if (handle >> 28) & CHECK_MASK != self.check_word(slot.generation) {
            return Err(bad(
                "its check word does not match that slot's generation: the reference was deleted \
                 and the slot reused, or the value was not issued by this instance",
            ));
        }
        let (_, id) = slot
            .live
            .ok_or_else(|| bad("that reference slot is free: the reference has been deleted"))?;
        let object = self
            .objects
            .get(id.index as usize)
            .ok_or_else(|| bad("its object slot is past the end of the object table"))?;
        if object.generation != id.generation || object.object.is_none() {
            return Err(bad("the object it names has been freed"));
        }
        Ok(id)
    }

    /// Decode `handle`, allowing null, which becomes `None`.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniBadHandle`] for anything that is neither null nor a live reference.
    pub fn resolve_nullable(
        &self,
        function: &str,
        address: GuestAddr,
        handle: u64,
    ) -> AbiResult<Option<ObjectId>> {
        if handle == 0 {
            return Ok(None);
        }
        self.resolve_id(function, address, handle).map(Some)
    }

    /// Which table a live handle came from.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniBadHandle`].
    pub fn kind_of(&self, function: &str, address: GuestAddr, handle: u64) -> AbiResult<RefKind> {
        self.resolve_id(function, address, handle)?;
        let index = (handle & 0xfff_ffff) as usize;
        Ok(self.refs[index].live.expect("resolve_id accepted it").0)
    }

    /// The object behind a handle.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniBadHandle`].
    pub fn object(&self, function: &str, address: GuestAddr, handle: u64) -> AbiResult<&Object> {
        let id = self.resolve_id(function, address, handle)?;
        Ok(self.objects[id.index as usize].object.as_ref().expect("resolve_id accepted it"))
    }

    /// The object behind an id that has already been resolved.
    #[must_use]
    pub fn object_of(&self, id: ObjectId) -> Option<&Object> {
        let slot = self.objects.get(id.index as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        slot.object.as_ref()
    }

    /// The object behind an id, mutably.
    pub fn object_of_mut(&mut self, id: ObjectId) -> Option<&mut Object> {
        let slot = self.objects.get_mut(id.index as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        slot.object.as_mut()
    }

    /// The object behind a handle, mutably.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniBadHandle`].
    pub fn object_mut(
        &mut self,
        function: &str,
        address: GuestAddr,
        handle: u64,
    ) -> AbiResult<&mut Object> {
        let id = self.resolve_id(function, address, handle)?;
        Ok(self.objects[id.index as usize].object.as_mut().expect("resolve_id accepted it"))
    }

    /// Drop a reference of `expected` kind.
    ///
    /// `DeleteLocalRef` on a global reference is a *guest* mistake, and it is reported rather than
    /// performed: the two have different lifetimes and silently deleting the wrong one is how a
    /// later call finds a handle it was entitled to keep.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniBadHandle`].
    pub fn delete(
        &mut self,
        function: &str,
        address: GuestAddr,
        expected: RefKind,
        handle: u64,
    ) -> AbiResult<()> {
        let actual = self.kind_of(function, address, handle)?;
        if actual != expected {
            return Err(AbiError::JniBadHandle {
                function: function.to_string(),
                address,
                kind: "jobject",
                handle,
                why: format!(
                    "it is {} and this call deletes {}",
                    actual.name(),
                    expected.name()
                ),
            });
        }
        let index = (handle & 0xfff_ffff) as usize;
        let (kind, id) = self.refs[index].live.take().expect("kind_of accepted it");
        self.refs[index].generation = self.refs[index].generation.wrapping_add(1);
        self.free_refs.push(index as u32);
        if kind != RefKind::Weak {
            let slot = &mut self.objects[id.index as usize];
            slot.strong = slot.strong.saturating_sub(1);
            if slot.strong == 0 {
                slot.object = None;
                slot.generation = slot.generation.wrapping_add(1);
                self.free_objects.push(id.index);
            }
        }
        Ok(())
    }

    /// Encode a `jmethodID`.
    #[must_use]
    pub fn method_id(&self, method: MethodId) -> u64 {
        METHOD_TAG << 56
            | self.cookie << 32
            | u64::from(method.class.0) << 16
            | u64::from(method.member)
    }

    /// Decode a `jmethodID`.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniBadHandle`].
    pub fn decode_method(
        &self,
        function: &str,
        address: GuestAddr,
        handle: u64,
    ) -> AbiResult<MethodId> {
        let bad = |why: &str| AbiError::JniBadHandle {
            function: function.to_string(),
            address,
            kind: "jmethodID",
            handle,
            why: why.to_string(),
        };
        if handle == 0 {
            return Err(bad(
                "it is null: a lookup that failed was not checked, and the method cannot be named",
            ));
        }
        if handle >> 56 != METHOD_TAG {
            return Err(bad("its top byte is not the method-id tag"));
        }
        if (handle >> 32) & CHECK_MASK != self.cookie {
            return Err(bad("its check word is not this instance's, so it was never issued here"));
        }
        Ok(MethodId {
            class: ClassId(((handle >> 16) & 0xffff) as u16),
            member: (handle & 0xffff) as u16,
        })
    }

    /// Encode a `jfieldID`.
    #[must_use]
    pub fn field_id(&self, field: FieldId) -> u64 {
        FIELD_TAG << 56 | self.cookie << 32 | u64::from(field.class.0) << 16 | u64::from(field.member)
    }

    /// Decode a `jfieldID`.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniBadHandle`].
    pub fn decode_field(
        &self,
        function: &str,
        address: GuestAddr,
        handle: u64,
    ) -> AbiResult<FieldId> {
        let bad = |why: &str| AbiError::JniBadHandle {
            function: function.to_string(),
            address,
            kind: "jfieldID",
            handle,
            why: why.to_string(),
        };
        if handle == 0 {
            return Err(bad(
                "it is null: a lookup that failed was not checked, and the field cannot be named",
            ));
        }
        if handle >> 56 != FIELD_TAG {
            return Err(bad("its top byte is not the field-id tag"));
        }
        if (handle >> 32) & CHECK_MASK != self.cookie {
            return Err(bad("its check word is not this instance's, so it was never issued here"));
        }
        Ok(FieldId {
            class: ClassId(((handle >> 16) & 0xffff) as u16),
            member: (handle & 0xffff) as u16,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jni::values::JavaString;

    fn handles() -> Handles {
        Handles::new(0x00ab_cdef)
    }

    fn string(text: &str) -> Object {
        Object::String(JavaString::from_str(text))
    }

    #[test]
    fn a_reference_round_trips_and_names_its_object() {
        let mut h = handles();
        let r = h.new_local("NewStringUTF", 0x1000, string("hello")).expect("a reference");
        assert_eq!(r >> 56, REF_TAG);
        match h.object("GetStringLength", 0x1000, r).expect("live") {
            Object::String(text) => assert_eq!(text.to_string_lossy(), "hello"),
            other => panic!("{other:?}"),
        }
        assert_eq!(h.live_references(), 1);
        assert_eq!(h.live_objects(), 1);
    }

    /// **The hostile case.** Four shapes of a value the guest could hand back, each of which must
    /// be a typed error naming the function rather than an answer.
    #[test]
    fn a_handle_this_table_did_not_issue_is_refused_by_name() {
        let mut h = handles();
        let good = h.new_local("FindClass", 0x2000, string("x")).expect("a reference");
        for (value, why) in [
            (0u64, "null"),
            (0x1234_5678_9abc_def0, "a wrong tag"),
            (REF_TAG << 56 | 1 << 52, "a valid tag with an index past the end"),
            (good ^ 0x1_0000_0000, "a valid handle with its check word disturbed"),
        ] {
            let error = h.resolve_id("GetObjectClass", 0x2000, value).expect_err(why);
            match error {
                AbiError::JniBadHandle { function, handle, .. } => {
                    assert_eq!(function, "GetObjectClass");
                    assert_eq!(handle, value);
                }
                other => panic!("{why}: {other:?}"),
            }
        }
    }

    /// Use-after-delete. `DeleteLocalRef` has 45 call sites, so this is a shape the engine
    /// produces in the ordinary course of running.
    #[test]
    fn a_deleted_reference_does_not_come_back_when_its_slot_is_reused() {
        let mut h = handles();
        let first = h.new_local("NewStringUTF", 0x3000, string("first")).expect("a reference");
        h.delete("DeleteLocalRef", 0x3000, RefKind::Local, first).expect("deleted");
        let second = h.new_local("NewStringUTF", 0x3000, string("second")).expect("a reference");
        assert_ne!(first, second, "the reused slot must not hand back the same handle");
        let error = h.resolve_id("GetStringLength", 0x3000, first).expect_err("stale");
        assert!(matches!(error, AbiError::JniBadHandle { .. }), "{error:?}");
    }

    /// Deleting a global reference through `DeleteLocalRef` is reported, not performed.
    #[test]
    fn deleting_a_reference_of_the_wrong_kind_is_refused() {
        let mut h = handles();
        let local = h.new_local("FindClass", 0x4000, string("c")).expect("a reference");
        let global = h
            .duplicate("NewGlobalRef", 0x4000, RefKind::Global, local)
            .expect("a global reference");
        let error =
            h.delete("DeleteLocalRef", 0x4000, RefKind::Local, global).expect_err("wrong kind");
        assert!(matches!(error, AbiError::JniBadHandle { .. }), "{error:?}");
        h.delete("DeleteGlobalRef", 0x4000, RefKind::Global, global).expect("right kind");
    }

    /// The object outlives the local reference exactly as long as a global one holds it, which is
    /// what `NewGlobalRef` is for and what `JNI_OnLoad` does with every class it caches.
    #[test]
    fn a_global_reference_keeps_the_object_after_the_local_one_goes() {
        let mut h = handles();
        let local = h.new_local("FindClass", 0x5000, string("c")).expect("a reference");
        let global = h.duplicate("NewGlobalRef", 0x5000, RefKind::Global, local).expect("global");
        h.delete("DeleteLocalRef", 0x5000, RefKind::Local, local).expect("deleted");
        assert_eq!(h.live_objects(), 1, "the global reference still holds it");
        h.object("GetObjectClass", 0x5000, global).expect("still resolvable");
        h.delete("DeleteGlobalRef", 0x5000, RefKind::Global, global).expect("deleted");
        assert_eq!(h.live_objects(), 0);
    }

    /// The module's own claim, asserted rather than left in prose: a weak reference does not keep
    /// its object alive, and with no collector the object is still there while a strong reference
    /// is — so the weak one resolves for as long as the engine's cached class does.
    #[test]
    fn a_weak_reference_holds_no_strong_count_and_still_resolves() {
        let mut h = handles();
        let local = h.new_local("FindClass", 0x6000, string("c")).expect("a reference");
        let weak = h.duplicate("NewWeakGlobalRef", 0x6000, RefKind::Weak, local).expect("weak");
        let global = h.duplicate("NewGlobalRef", 0x6000, RefKind::Global, local).expect("global");
        h.delete("DeleteLocalRef", 0x6000, RefKind::Local, local).expect("deleted");
        h.object("IsSameObject", 0x6000, weak).expect("the weak reference still resolves");
        h.delete("DeleteGlobalRef", 0x6000, RefKind::Global, global).expect("deleted");
        // The last strong reference has gone, so the object has; the weak handle now reports the
        // object is gone rather than resolving to somebody else's.
        let error = h.resolve_id("IsSameObject", 0x6000, weak).expect_err("collected");
        assert!(matches!(error, AbiError::JniBadHandle { .. }), "{error:?}");
    }

    #[test]
    fn method_and_field_ids_round_trip_and_reject_each_other() {
        let h = handles();
        let method = MethodId { class: ClassId(7), member: 3 };
        let encoded = h.method_id(method);
        assert_eq!(h.decode_method("GetMethodID", 0, encoded).expect("decodes"), method);
        let error = h.decode_field("GetFieldID", 0, encoded).expect_err("a method id is not a field id");
        assert!(matches!(error, AbiError::JniBadHandle { kind: "jfieldID", .. }), "{error:?}");
        let field = FieldId { class: ClassId(9), member: 2 };
        let encoded = h.field_id(field);
        assert_eq!(h.decode_field("GetFieldID", 0, encoded).expect("decodes"), field);
        let error = h.decode_method("GetMethodID", 0, encoded).expect_err("and the reverse");
        assert!(matches!(error, AbiError::JniBadHandle { kind: "jmethodID", .. }), "{error:?}");
    }

    /// A null `jmethodID` is the exact shape of "a `GetMethodID` that returned null was not
    /// checked". It must name itself rather than dispatch to member zero of class zero.
    #[test]
    fn a_null_method_id_says_the_lookup_was_not_checked() {
        let h = handles();
        let error = h.decode_method("CallVoidMethodV", 0x7000, 0).expect_err("null");
        match error {
            AbiError::JniBadHandle { why, .. } => assert!(why.contains("not checked"), "{why}"),
            other => panic!("{other:?}"),
        }
    }
}
