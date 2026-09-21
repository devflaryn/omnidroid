//! The class registry: the 104 classes and 409 members, as a table of what the host answers.
//!
//! # Why this is not a JVM, stated where it will be read
//!
//! D7 says no JVM, no ART, no dex interpreter, and 104 classes with 409 members looks at a glance
//! like the thing D7 forbids. It is not, and the reason is structural rather than a matter of
//! degree. `jni-surface.md` §6 searched for every mechanism that would force dex execution and
//! found **all of them absent**: no `java/lang/reflect/*`, no `Class.forName`, no
//! `dalvik/system/*`, no `DexClassLoader`, and `JNIEnv::DefineClass` is **never dereferenced**.
//! Nothing here loads code. What the engine does is *look up members by name and descriptor and
//! call them*, and 90% of what it looks up is Roblox's own thin Kotlin shell — a set of getters
//! and notification sinks **whose behaviour Omnidroid gets to define**. Defining them is not
//! interpreting them.
//!
//! The one entry that reads as a D7 violation and is not: **`java/lang/ClassLoader`**
//! (`loadClass`, `findClass`, `getClassLoader`). `jni-surface.md` §6 identifies it as the
//! canonical *"cache the app `ClassLoader` in `JNI_OnLoad` so `FindClass` resolves app classes on
//! threads attached later"* pattern, reached from `NativeObjectManager.getClassLoader()` and from
//! `JvmClassLoaderHelper`. It is a **name resolver**, not a code loader, and this layer implements
//! it as one: `loadClass(name)` answers with the same `jclass` [`Registry::find`] would. No bytes
//! are read, no class is defined, and `DefineClass` stays a refusal that names itself.
//!
//! # A missing member is fatal, so a missing member is recorded
//!
//! §3.1's Tier 0 members are `CHECK_NOT_NULL` aborts — `!gGameActivityClassInfo.finish` and its
//! four siblings are assertion strings in `.rodata`, and a null there kills the process rather
//! than degrading it. But §3.1's Tier X is the opposite: `DeviceUtils` and five `signalVideo*`
//! methods have **no declaring class anywhere in the 26,620 dex classes**, so `libroblox.so` will
//! look them up on a real device and get null, and its own code tolerates that.
//!
//! Both are true at once, so the registry cannot choose one policy. What it does instead:
//!
//! * a lookup that misses returns **null with a pending exception**, which is what ART does and
//!   what the Tier X path is written against;
//! * **and every miss is recorded** in [`Registry::misses`], with the class, name and descriptor
//!   the engine asked for. A Tier 0 miss is then a fact the host can read *before* the abort
//!   rather than a `CHECK_NOT_NULL` three thousand instructions later, and the list is the
//!   measurement M5 needs to know what step 13 will ask for.

use std::collections::BTreeMap;

use crate::error::{AbiError, AbiResult};

use super::values::Value;

/// A class in the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClassId(pub u16);

/// A method of a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MethodId {
    /// Its class.
    pub class: ClassId,
    /// Its index within that class's method list.
    pub member: u16,
}

/// A field of a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FieldId {
    /// Its class.
    pub class: ClassId,
    /// Its index within that class's field list.
    pub member: u16,
}

/// How load-bearing a class is, from `jni-surface.md` §3.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The process cannot start without it. A missing member here is a `CHECK_NOT_NULL` abort.
    Zero,
    /// Needed to get past engine bootstrap and produce a surface.
    One,
    /// Input and text: needed for an interactive frame, not the first one.
    Two,
    /// Lazily initialised and skippable for a first frame.
    Three,
    /// Not in §3.1 — declared here because this layer needs it to answer something else.
    Support,
}

/// What the host answers when the engine calls a member.
///
/// **Every variant is a decision, and [`Answer::Unanswered`] is the one that refuses.** There is
/// no variant that returns a believable value the host has not chosen: Global Constraint 1's
/// failure shape is a plausible wrong answer, and a Java getter is exactly where one would hide.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Answer {
    /// A notification sink: record the call and its arguments, return void.
    ///
    /// Correct rather than a stub for the `gameActivity_*` callbacks and the `on*` notifications,
    /// which on a device update Java-side UI state this runtime does not have. What the host owes
    /// them is to *observe* them, and [`super::Jni::calls`] is where they are observed.
    Sink,
    /// `boolean`.
    Bool(bool),
    /// `int`.
    Int(i32),
    /// `long`.
    Long(i64),
    /// `float`.
    Float(f32),
    /// `double`.
    Double(f64),
    /// A `java.lang.String` the host defines.
    Text(&'static str),
    /// A reference the host defines as Java `null`.
    ///
    /// Distinct from [`Unanswered`](Answer::Unanswered): this is the host *choosing* null, which
    /// several of these genuinely are on a device with no account signed in.
    Null,
    /// `<init>`: construct an instance of the declaring class with no fields set.
    NewInstance,
    /// Construct an instance of **another** declared class and return it.
    ///
    /// What a factory getter is: `ActivityThread.currentApplication()`,
    /// `Context.getResources()`, `Resources.getDisplayMetrics()`. The engine walks that chain to
    /// reach five `DisplayMetrics` fields, and each link is a real object rather than a pretend
    /// one — the fields at the end are what the host defines.
    NewInstanceOf(&'static str),
    /// `ClassLoader.loadClass(String)` / `findClass(String)`: resolve argument 0 as a class name
    /// and return the `jclass`, or Java `null` when nothing declares it.
    ///
    /// **This is the member that reads like a D7 violation and is not**, and it is a variant of
    /// its own so that the reading is unavoidable: it resolves a *name* against this registry.
    /// It reads no bytes, defines no class, and `DefineClass` remains a refusal that names
    /// itself. See the module docs.
    ResolveClass,
    /// `String.getBytes(String charset)`: the receiver's text, encoded.
    StringBytes,
    /// An `Object[0]`.
    ///
    /// `List.toArray()` on the empty list this layer hands the engine. Correct rather than a
    /// stub: the list really has no elements, and an empty array is what `toArray` returns for
    /// one.
    EmptyObjectArray,
    /// Read the named field of the receiver and return it.
    Field(&'static str),
    /// The member is on the surface and the host has **not** decided what it answers.
    ///
    /// Calling it is [`AbiError::JniRefused`] naming the class, member and descriptor. Looking it
    /// *up* still succeeds, because a `GetMethodID` that fails is a `CHECK_NOT_NULL` abort for
    /// every Tier 0 member and the point is to fail where the answer is needed, not where the id
    /// is taken.
    Unanswered,
    /// The Java method is itself `native`: dispatch back into guest code.
    ///
    /// Nothing on steps 6-12 takes this path — the engine calls Java, and Java on this path is
    /// all host-defined — but `RegisterNatives` records function pointers and a later milestone
    /// will call one.
    Native,
}

/// One declared member.
#[derive(Debug, Clone)]
pub struct Member {
    /// Its name.
    pub name: String,
    /// Its JNI descriptor.
    pub descriptor: String,
    /// Whether it is `static`.
    pub is_static: bool,
    /// What the host answers.
    pub answer: Answer,
    /// Set by `RegisterNatives`: the guest function bound to this member.
    pub bound_native: Option<omni_mem::GuestAddr>,
}

/// A declared class.
#[derive(Debug, Clone)]
pub struct Class {
    /// Its JNI name, slashes not dots: `com/roblox/client/startup/NativeHelper`.
    pub name: String,
    /// How load-bearing it is.
    pub tier: Tier,
    /// Its methods, in declaration order.
    pub methods: Vec<Member>,
    /// Its fields, in declaration order.
    pub fields: Vec<Member>,
}

/// A lookup the registry could not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Miss {
    /// Which JNI function asked.
    pub function: String,
    /// The class, or the name the engine passed to `FindClass`.
    pub class: String,
    /// The member name, empty for a `FindClass` miss.
    pub member: String,
    /// The descriptor the engine asked for, empty for a `FindClass` miss.
    pub descriptor: String,
}

/// The classes and members this layer answers for.
#[derive(Debug)]
pub struct Registry {
    classes: Vec<Class>,
    by_name: BTreeMap<String, ClassId>,
    misses: Vec<Miss>,
}

impl Registry {
    /// A registry holding [`DECLARED`].
    ///
    /// # Panics
    ///
    /// Never at run time: the declarations are a `static` in this module and
    /// `every_declared_descriptor_parses` checks all of them in the suite. The `expect` is here
    /// rather than an error return because a malformed declaration is this crate's own defect and
    /// cannot be produced by any caller or any guest.
    #[must_use]
    pub fn with_declared() -> Self {
        let mut registry = Self { classes: Vec::new(), by_name: BTreeMap::new(), misses: Vec::new() };
        for spec in DECLARED {
            registry.declare(spec).expect("this crate's own class declarations are well formed");
        }
        // **The generated surface goes in second, and that order is the policy.** `extend_with`
        // adds a class that is not declared and, for one that is, only the members it does not
        // already have -- so every decided answer above survives and every *other* member of a
        // class the engine can name still resolves. See `surface`'s own header.
        for spec in super::surface::DEX_SURFACE {
            registry.extend_with(spec);
        }
        registry
    }

    /// Declare `spec`, or add to an existing class only the members it does not already have.
    ///
    /// **A hand-written declaration always wins.** A member already present keeps its
    /// [`Answer`]; one that is not present arrives with whatever `spec` gives it, which for the
    /// generated surface is [`Answer::Unanswered`].
    ///
    /// Returns how many members were added.
    pub fn extend_with(&mut self, spec: &ClassSpec) -> usize {
        let Some(id) = self.find(spec.name) else {
            let before = self.member_count();
            // The only failures are a duplicate name, excluded by the `find` above, and an id
            // that does not fit in 16 bits. The second is this crate's own problem and is
            // asserted by `the_whole_declared_surface_fits_the_id_encoding`; a registry that is
            // short is better than a panic reachable from a host that declared its own classes.
            let _ = self.declare(spec);
            return self.member_count() - before;
        };
        let mut added = 0;
        for member in spec.methods {
            if self.method(id, member.name, member.descriptor, member.is_static).is_none() {
                let class = &mut self.classes[usize::from(id.0)];
                if class.methods.len() < usize::from(u16::MAX) {
                    class.methods.push(Member {
                        name: member.name.to_string(),
                        descriptor: member.descriptor.to_string(),
                        is_static: member.is_static,
                        answer: member.answer,
                        bound_native: None,
                    });
                    added += 1;
                }
            }
        }
        for member in spec.fields {
            if self.field(id, member.name, member.descriptor, member.is_static).is_none() {
                let class = &mut self.classes[usize::from(id.0)];
                if class.fields.len() < usize::from(u16::MAX) {
                    class.fields.push(Member {
                        name: member.name.to_string(),
                        descriptor: member.descriptor.to_string(),
                        is_static: member.is_static,
                        answer: member.answer,
                        bound_native: None,
                    });
                    added += 1;
                }
            }
        }
        added
    }

    /// Add a class.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] if the name is already declared or a descriptor does not parse.
    pub fn declare(&mut self, spec: &ClassSpec) -> AbiResult<ClassId> {
        if self.by_name.contains_key(spec.name) {
            return Err(AbiError::JniRefused {
                function: "Registry::declare".to_string(),
                address: 0,
                detail: format!("`{}` is already declared", spec.name),
            });
        }
        let id = ClassId(u16::try_from(self.classes.len()).map_err(|_| AbiError::JniRefused {
            function: "Registry::declare".to_string(),
            address: 0,
            detail: "more than 65,536 classes; the id encoding holds 16 bits".to_string(),
        })?);
        let member = |m: &MemberSpec| Member {
            name: m.name.to_string(),
            descriptor: m.descriptor.to_string(),
            is_static: m.is_static,
            answer: m.answer,
            bound_native: None,
        };
        let methods: Vec<Member> = spec.methods.iter().map(member).collect();
        let fields: Vec<Member> = spec.fields.iter().map(member).collect();
        if methods.len() > usize::from(u16::MAX) || fields.len() > usize::from(u16::MAX) {
            return Err(AbiError::JniRefused {
                function: "Registry::declare".to_string(),
                address: 0,
                detail: format!("`{}` has more members than the id encoding holds", spec.name),
            });
        }
        self.classes.push(Class {
            name: spec.name.to_string(),
            tier: spec.tier,
            methods,
            fields,
        });
        self.by_name.insert(spec.name.to_string(), id);
        Ok(id)
    }

    /// How many classes are declared.
    #[must_use]
    pub fn class_count(&self) -> usize {
        self.classes.len()
    }

    /// How many members are declared, methods and fields together.
    #[must_use]
    pub fn member_count(&self) -> usize {
        self.classes.iter().map(|c| c.methods.len() + c.fields.len()).sum()
    }

    /// The classes, in declaration order.
    pub fn classes(&self) -> impl Iterator<Item = &Class> {
        self.classes.iter()
    }

    /// A class by JNI name.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<ClassId> {
        self.by_name.get(name).copied()
    }

    /// A class by id.
    ///
    /// # Panics
    ///
    /// Never for an id this registry issued; a forged [`ClassId`] cannot reach here, because
    /// every one the guest can name comes out of [`super::refs::Handles::decode_method`] or
    /// `decode_field`, which check the instance cookie first, and out of a `jclass` handle, which
    /// checks the reference table.
    #[must_use]
    pub fn class(&self, id: ClassId) -> Option<&Class> {
        self.classes.get(usize::from(id.0))
    }

    /// The name of a class, for a message.
    #[must_use]
    pub fn class_name(&self, id: ClassId) -> &str {
        self.class(id).map_or("<unknown class>", |c| c.name.as_str())
    }

    /// Resolve a method by name and descriptor.
    #[must_use]
    pub fn method(&self, class: ClassId, name: &str, descriptor: &str, is_static: bool) -> Option<MethodId> {
        let declared = self.class(class)?;
        declared
            .methods
            .iter()
            .position(|m| m.name == name && m.descriptor == descriptor && m.is_static == is_static)
            .map(|member| MethodId { class, member: member as u16 })
    }

    /// Resolve a field by name and descriptor.
    #[must_use]
    pub fn field(&self, class: ClassId, name: &str, descriptor: &str, is_static: bool) -> Option<FieldId> {
        let declared = self.class(class)?;
        declared
            .fields
            .iter()
            .position(|f| f.name == name && f.descriptor == descriptor && f.is_static == is_static)
            .map(|member| FieldId { class, member: member as u16 })
    }

    /// The member behind a [`MethodId`].
    #[must_use]
    pub fn member(&self, id: MethodId) -> Option<&Member> {
        self.class(id.class)?.methods.get(usize::from(id.member))
    }

    /// The member behind a [`MethodId`], mutably — what `RegisterNatives` writes through.
    pub fn member_mut(&mut self, id: MethodId) -> Option<&mut Member> {
        self.classes
            .get_mut(usize::from(id.class.0))?
            .methods
            .get_mut(usize::from(id.member))
    }

    /// The member behind a [`FieldId`].
    #[must_use]
    pub fn field_member(&self, id: FieldId) -> Option<&Member> {
        self.class(id.class)?.fields.get(usize::from(id.member))
    }

    /// The member behind a [`FieldId`], mutably, so a host can decide what it answers.
    #[must_use]
    pub fn field_mut(&mut self, id: FieldId) -> Option<&mut Member> {
        self.classes
            .get_mut(usize::from(id.class.0))?
            .fields
            .get_mut(usize::from(id.member))
    }

    /// Record a lookup nothing here answers. See the module docs for why this exists.
    pub fn record_miss(&mut self, miss: Miss) {
        // Bounded, because a guest in a loop looking up a member that does not exist would
        // otherwise grow this without limit. The bound is generous: the whole declared surface is
        // 409 members, so a thousand distinct misses is already a different problem.
        if self.misses.len() < MAX_MISSES && !self.misses.contains(&miss) {
            self.misses.push(miss);
        }
    }

    /// Every lookup this registry could not answer, in the order they first happened.
    #[must_use]
    pub fn misses(&self) -> &[Miss] {
        &self.misses
    }

    /// What a member's [`Answer`] evaluates to, for the answers that need nothing but themselves.
    ///
    /// The ones that do need something — [`Answer::Field`], [`Answer::NewInstance`],
    /// [`Answer::Native`] — are handled by the caller, which has the handle table and the CPU.
    #[must_use]
    pub fn simple_answer(answer: Answer) -> Option<Value> {
        Some(match answer {
            Answer::Sink => Value::Void,
            Answer::Bool(value) => Value::Boolean(value),
            Answer::Int(value) => Value::Int(value),
            Answer::Long(value) => Value::Long(value),
            Answer::Float(value) => Value::Float(value),
            Answer::Double(value) => Value::Double(value),
            Answer::Text(text) => Value::Text(text.to_string()),
            Answer::Null => Value::Object(None),
            Answer::Field(_)
            | Answer::NewInstance
            | Answer::NewInstanceOf(_)
            | Answer::ResolveClass
            | Answer::StringBytes
            | Answer::EmptyObjectArray
            | Answer::Native
            | Answer::Unanswered => return None,
        })
    }
}

/// How many distinct misses one registry records before it stops. See [`Registry::record_miss`].
pub const MAX_MISSES: usize = 1024;

/// A member, as the static declarations spell one.
#[derive(Debug, Clone, Copy)]
pub struct MemberSpec {
    /// Its name.
    pub name: &'static str,
    /// Its JNI descriptor.
    pub descriptor: &'static str,
    /// Whether it is `static`.
    pub is_static: bool,
    /// What the host answers.
    pub answer: Answer,
}

/// A class, as the static declarations spell one.
#[derive(Debug, Clone, Copy)]
pub struct ClassSpec {
    /// Its JNI name.
    pub name: &'static str,
    /// Its tier in `jni-surface.md` §3.1.
    pub tier: Tier,
    /// Its methods.
    pub methods: &'static [MemberSpec],
    /// Its fields.
    pub fields: &'static [MemberSpec],
}

/// An instance method.
const fn m(name: &'static str, descriptor: &'static str, answer: Answer) -> MemberSpec {
    MemberSpec { name, descriptor, is_static: false, answer }
}

/// A static method.
const fn s(name: &'static str, descriptor: &'static str, answer: Answer) -> MemberSpec {
    MemberSpec { name, descriptor, is_static: true, answer }
}

/// An instance field.
const fn f(name: &'static str, descriptor: &'static str, answer: Answer) -> MemberSpec {
    MemberSpec { name, descriptor, is_static: false, answer }
}

const NONE: &[MemberSpec] = &[];

// ------------------------------------------------------------------- Tier 0, §3.1

/// `com/google/androidgamesdk/GameActivity` — five `CHECK_NOT_NULL` members.
static GAME_ACTIVITY: &[MemberSpec] = &[
    m("finish", "()V", Answer::Sink),
    m("setWindowFlags", "(II)V", Answer::Sink),
    m("getWindowInsets", "(I)Landroidx/core/graphics/Insets;",
        Answer::NewInstanceOf("androidx/core/graphics/Insets")),
    m("getWaterfallInsets", "()Landroidx/core/graphics/Insets;",
        Answer::NewInstanceOf("androidx/core/graphics/Insets")),
    m("setImeEditorInfoFields", "(III)V", Answer::Sink),
];

/// `androidx/core/view/WindowInsetsCompat$Type` — the nine static `()I` mask bits.
///
/// The values are AndroidX's own, which are a fixed bit per inset type. They are **declared
/// here** rather than answered `Unanswered` because the engine ORs them into a mask it then
/// passes to `getWindowInsets(I)`, and a mask is only meaningful if the bits are distinct.
static WINDOW_INSETS_TYPE: &[MemberSpec] = &[
    s("statusBars", "()I", Answer::Int(1 << 0)),
    s("navigationBars", "()I", Answer::Int(1 << 1)),
    s("captionBar", "()I", Answer::Int(1 << 2)),
    s("ime", "()I", Answer::Int(1 << 3)),
    s("systemGestures", "()I", Answer::Int(1 << 4)),
    s("mandatorySystemGestures", "()I", Answer::Int(1 << 5)),
    s("tappableElement", "()I", Answer::Int(1 << 6)),
    s("displayCutout", "()I", Answer::Int(1 << 7)),
    s("systemBars", "()I", Answer::Int((1 << 0) | (1 << 1) | (1 << 2))),
];

/// `android/content/res/Configuration` — the 18 fields plus `getLocales()`.
///
/// **A contradiction with `jni-surface.md` §3.1, recorded rather than smoothed over.** §3.1 calls
/// these "**18 int fields**" and lists `fontScale` among them. `fontScale` is a `public float` on
/// every Android release, and Section D of the lists file shows the analysis could not resolve
/// **any** of these descriptors (`<unresolved>`), so "18 int" is the analyst's summary and not a
/// measurement. It is declared `F` here. If the engine asks for `fontScale` as `I` the lookup
/// misses, and the miss is recorded with the descriptor it asked for — which is the measurement
/// that settles it, and is why a miss is recorded rather than silently answered.
/// `android.view.MotionEvent`, as the GameActivity glue reads it — **twenty-two methods,
/// MEASURED**.
///
/// Every entry was read out of M5's gate's own miss log: the class was declared empty, the run
/// asked for a member, `Jni::misses` recorded it *with the descriptor it asked for*, and the
/// entry was written from that. **Not transcribed from the Android API**, which would be a claim
/// about a surface nobody measured — and which would have got `getClassification` and
/// `getActionButton` wrong, since both are API-29-and-later and a transcription from an older
/// reference would have omitted them.
///
/// It is independent confirmation of `apk-analysis.md` §4.4's finding that the **buffered** input
/// model is in use: the glue reads these twenty-two off the *Java* `MotionEvent` and packs them
/// into its own `GameActivityMotionEvent`, which is why `AMotionEvent_*` is absent from the whole
/// APK.
///
/// **All `Unanswered`, deliberately.** Step 13 only *resolves* these; nothing calls one until an
/// input event arrives, which is M8. A `GetMethodID` that missed would be a null `jmethodID`
/// handed straight to `CallIntMethodV` — which is how this list was found — so the lookup must
/// succeed. Calling one refuses by name, which is where the answer will have to be decided.
static MOTION_EVENT: &[MemberSpec] = &[
    m("getDeviceId", "()I", Answer::Unanswered),
    m("getSource", "()I", Answer::Unanswered),
    m("getAction", "()I", Answer::Unanswered),
    m("getEventTime", "()J", Answer::Unanswered),
    m("getDownTime", "()J", Answer::Unanswered),
    m("getFlags", "()I", Answer::Unanswered),
    m("getMetaState", "()I", Answer::Unanswered),
    m("getActionButton", "()I", Answer::Unanswered),
    m("getButtonState", "()I", Answer::Unanswered),
    m("getClassification", "()I", Answer::Unanswered),
    m("getEdgeFlags", "()I", Answer::Unanswered),
    m("getHistorySize", "()I", Answer::Unanswered),
    m("getHistoricalEventTime", "(I)J", Answer::Unanswered),
    m("getPointerCount", "()I", Answer::Unanswered),
    m("getPointerId", "(I)I", Answer::Unanswered),
    m("getToolType", "(I)I", Answer::Unanswered),
    m("getRawX", "(I)F", Answer::Unanswered),
    m("getRawY", "(I)F", Answer::Unanswered),
    m("getXPrecision", "()F", Answer::Unanswered),
    m("getYPrecision", "()F", Answer::Unanswered),
    m("getAxisValue", "(II)F", Answer::Unanswered),
    m("getHistoricalAxisValue", "(III)F", Answer::Unanswered),
];

/// `android.view.KeyEvent` — **eleven methods, MEASURED** the same way as [`MOTION_EVENT`], from
/// the same run's miss log.
static KEY_EVENT: &[MemberSpec] = &[
    m("getDeviceId", "()I", Answer::Unanswered),
    m("getSource", "()I", Answer::Unanswered),
    m("getAction", "()I", Answer::Unanswered),
    m("getEventTime", "()J", Answer::Unanswered),
    m("getDownTime", "()J", Answer::Unanswered),
    m("getFlags", "()I", Answer::Unanswered),
    m("getMetaState", "()I", Answer::Unanswered),
    m("getModifiers", "()I", Answer::Unanswered),
    m("getRepeatCount", "()I", Answer::Unanswered),
    m("getKeyCode", "()I", Answer::Unanswered),
    m("getScanCode", "()I", Answer::Unanswered),
    m("getUnicodeChar", "()I", Answer::Unanswered),
];

static CONFIGURATION: &[MemberSpec] = &[
    f("mcc", "I", Answer::Int(0)),
    f("mnc", "I", Answer::Int(0)),
    f("orientation", "I", Answer::Int(2)),
    f("touchscreen", "I", Answer::Int(3)),
    f("keyboard", "I", Answer::Int(1)),
    f("keyboardHidden", "I", Answer::Int(1)),
    f("hardKeyboardHidden", "I", Answer::Int(2)),
    f("navigation", "I", Answer::Int(1)),
    f("navigationHidden", "I", Answer::Int(1)),
    f("screenLayout", "I", Answer::Int(0x24)),
    f("uiMode", "I", Answer::Int(0x11)),
    f("screenWidthDp", "I", Answer::Int(0)),
    f("screenHeightDp", "I", Answer::Int(0)),
    f("smallestScreenWidthDp", "I", Answer::Int(0)),
    f("densityDpi", "I", Answer::Int(0)),
    f("colorMode", "I", Answer::Int(5)),
    f("fontScale", "F", Answer::Float(1.0)),
    f("fontWeightAdjustment", "I", Answer::Int(0)),
];

// -------------------------------------------------------------------------- the table

/// Every class this layer declares, with what it answers for each member.
///
/// Ordered as `jni-surface.md` §3.1 ranks them: Tier 0 first, then the Tier 1 classes the three
/// `JNI_OnLoad` registration helpers batch-resolve (§8 step 6b), then the support classes the
/// startup script needs.
pub static DECLARED: &[ClassSpec] = &[
    // ---- Tier 0 ---------------------------------------------------------------------------
    ClassSpec {
        name: "com/google/androidgamesdk/GameActivity",
        tier: Tier::Zero,
        methods: GAME_ACTIVITY,
        fields: NONE,
    },
    ClassSpec {
        name: "androidx/core/graphics/Insets",
        tier: Tier::Zero,
        methods: NONE,
        fields: &[
            f("left", "I", Answer::Int(0)),
            f("top", "I", Answer::Int(0)),
            f("right", "I", Answer::Int(0)),
            f("bottom", "I", Answer::Int(0)),
        ],
    },
    ClassSpec {
        name: "androidx/core/view/WindowInsetsCompat$Type",
        tier: Tier::Zero,
        methods: WINDOW_INSETS_TYPE,
        fields: NONE,
    },
    // **`MotionEvent` and `KeyEvent`, which §3.1's Tier 0 does not name.**
    //
    // MEASURED by M5's gate: step 13 does `FindClass("android/view/MotionEvent")`, gets null
    // because nothing declared it, and hands the null straight to `GetMethodID` — which is §8.1's
    // **third** failure mode happening for real, one class further on than §3.1 predicted. On a
    // device the null would be a `CHECK_NOT_NULL` abort or a JNI warning and a crash; here it is a
    // refusal naming the call, which is how it was found.
    //
    // They are the GameActivity glue's, not Roblox's: `GameActivity_onCreate` fills all 21
    // callback slots and the glue converts Java input events into its own
    // `GameActivityMotionEvent`/`GameActivityKeyEvent` buffers. `apk-analysis.md` §4.4 records
    // that `AMotionEvent_*` and `AKeyEvent_*` are absent from the whole APK, which is independent
    // confirmation of that buffered model — the glue reads the *Java* objects through JNI rather
    // than the NDK's input API.
    //
    // Declared with the members the run asks for and **nothing else**: a member nobody looks up is
    // a claim about a surface this layer has not measured.
    ClassSpec {
        name: "android/view/MotionEvent",
        tier: Tier::Zero,
        methods: MOTION_EVENT,
        fields: NONE,
    },
    ClassSpec {
        name: "android/view/KeyEvent",
        tier: Tier::Zero,
        methods: KEY_EVENT,
        fields: NONE,
    },
    // **`AssetManager` has no members and that is the whole of it.** §8 step 13 receives one as an
    // argument, takes a global reference to it and hands it to `AAssetManager_fromJava`; nothing
    // on the startup path calls a method on it from native code. It is declared so that the host
    // can *build* one — `Jni::new_object` refuses a class nobody declared — and so that
    // `AAssetManager_fromJava` can check that what it was given really is one, rather than turning
    // a wrong argument into an asset manager that answers null for every asset.
    ClassSpec {
        name: "android/content/res/AssetManager",
        tier: Tier::Zero,
        methods: NONE,
        fields: NONE,
    },
    // **`Surface` has no members and that is the whole of it**, for `AssetManager`'s reason one
    // object along. §8 row 17 receives one as an argument to `onSurfaceCreatedNative` and hands it
    // straight to `ANativeWindow_fromSurface`; nothing on the startup path calls a method on it
    // from native code — row 24's `PlatformParams.surface()` returns one rather than reading it.
    // It is declared so that the host can *build* one — `Jni::new_object` refuses a class nobody
    // declared — and so that `ANativeWindow_fromSurface` can check that what it was given really
    // is one, rather than turning a wrong argument into a window that answers nonsense thousands
    // of instructions from the mistake.
    ClassSpec { name: "android/view/Surface", tier: Tier::Zero, methods: NONE, fields: NONE },
    ClassSpec {
        name: "android/content/res/Configuration",
        tier: Tier::Zero,
        methods: &[m(
            "getLocales",
            "()Landroid/os/LocaleList;",
            Answer::NewInstanceOf("android/os/LocaleList"),
        )],
        fields: CONFIGURATION,
    },
    ClassSpec {
        name: "java/lang/String",
        tier: Tier::Zero,
        methods: &[
            m("getBytes", "(Ljava/lang/String;)[B", Answer::StringBytes),
            m("onSetCookie", "([Ljava/lang/String;Ljava/lang/String;)V", Answer::Sink),
        ],
        fields: NONE,
    },
    // **The engine fetches its own `Context`** (§3.1): it does not wait to be handed one, so
    // these three must answer or it has none.
    ClassSpec {
        name: "android/app/ActivityThread",
        tier: Tier::Zero,
        methods: &[
            s("currentActivityThread", "()Landroid/app/ActivityThread;", Answer::NewInstance),
            s("currentApplication", "()Landroid/app/Application;",
                Answer::NewInstanceOf("android/app/Application")),
            m("getApplication", "()Landroid/app/Application;",
                Answer::NewInstanceOf("android/app/Application")),
        ],
        fields: NONE,
    },
    // The name resolver, **not** a code loader. See the module docs.
    ClassSpec {
        name: "java/lang/ClassLoader",
        tier: Tier::Zero,
        methods: &[
            m("loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", Answer::ResolveClass),
            m("findClass", "(Ljava/lang/String;)Ljava/lang/Class;", Answer::ResolveClass),
            m("getClassLoader", "()Ljava/lang/ClassLoader;",
                Answer::NewInstanceOf("java/lang/ClassLoader")),
        ],
        fields: NONE,
    },
    // ---- step 6a: the one class `JNI_OnLoad` itself resolves ------------------------------
    ClassSpec {
        name: "com/roblox/universalapp/logging/LoggingProtocol",
        tier: Tier::One,
        methods: &[s("getProcessTimestamp", "()J", Answer::Unanswered)],
        fields: NONE,
    },
    // ---- Tier 1: what step 6b's three registration helpers batch-resolve ------------------
    ClassSpec {
        name: "com/roblox/engine/jni/NativeGLJavaInterface",
        tier: Tier::One,
        methods: &[
            s("exitGameWithError", "(I)V", Answer::Sink),
            s("gameDidLeave", "()V", Answer::Sink),
            s("gameLoadedCallback", "(J)V", Answer::Sink),
            s(
                "getDeviceStaticParams",
                "()Lcom/roblox/engine/jni/model/DeviceStaticParams;",
                Answer::NewInstanceOf("com/roblox/engine/jni/model/DeviceStaticParams"),
            ),
            s("getMobileAdvertisingId", "()V", Answer::Sink),
            s("getWebViewUserAgent", "()V", Answer::Sink),
            s("hideKeyboard", "()V", Answer::Sink),
            s("listenToMotionEvents", "(Ljava/lang/String;)V", Answer::Sink),
            s("onAppBridgeNotification", "(Ljava/lang/String;Ljava/lang/String;)V", Answer::Sink),
            s("onAppShellReloadNeeded", "()V", Answer::Sink),
            s(
                "onDataModelNotificationCallback",
                "(Ljava/lang/String;Ljava/lang/String;)V",
                Answer::Sink,
            ),
            s("onExtendedAnalyticsRecvCallback", "([BI)V", Answer::Sink),
            s("onLuaTextBoxChangedCallback", "(Ljava/lang/String;)V", Answer::Sink),
            s("onLuaTextBoxPropertyChangedCallback", "()V", Answer::Sink),
            s("onVrSessionStateUpdate", "(I)V", Answer::Sink),
            s("openNativeOverlay", "(Ljava/lang/String;Ljava/lang/String;)V", Answer::Sink),
            s("promptNativePurchase", "(JLjava/lang/String;Ljava/lang/String;)V", Answer::Sink),
            s("promptNativePurchase", "(JLjava/lang/String;)V", Answer::Sink),
            s(
                "promptNativePurchaseWithPayload",
                "(JLjava/lang/String;Ljava/lang/String;)V",
                Answer::Sink,
            ),
            s(
                "promptNativePurchaseWithPaymentSessionId",
                "(JLjava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
                Answer::Sink,
            ),
            s(
                "promptNativePurchaseWithPaymentSessionId",
                "(JLjava/lang/String;Ljava/lang/String;)V",
                Answer::Sink,
            ),
            s("saveImageToAlbum", "(Ljava/lang/String;)V", Answer::Sink),
            s("screenOrientationChanged", "(I)V", Answer::Sink),
            s(
                "showKeyboard",
                "(JZ[BLcom/roblox/engine/jni/model/NativeTextBoxInfo;)V",
                Answer::Sink,
            ),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "com/roblox/engine/jni/user/NativeUserJavaInterface",
        tier: Tier::One,
        methods: &[
            // Signed out is the state this runtime starts in, and every one of these is what a
            // device answers for a signed-out app. Not a stub: the host defines the user, and
            // there is no user.
            s("getUserId", "()J", Answer::Long(0)),
            s("getUsername", "()Ljava/lang/String;", Answer::Text("")),
            s("getDisplayName", "()Ljava/lang/String;", Answer::Text("")),
            s("getAlternateName", "()Ljava/lang/String;", Answer::Text("")),
            s("getIsUnder13", "()Z", Answer::Bool(false)),
            s("getMembershipType", "()I", Answer::Int(0)),
            s("getTheme", "()Ljava/lang/String;", Answer::Text("Dark")),
            s("getPlatformName", "()Ljava/lang/String;", Answer::Text("Android")),
            s("getHasRobloxSubscription", "()Z", Answer::Bool(false)),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "com/roblox/engine/jni/locale/NativeLocaleJavaInterface",
        tier: Tier::One,
        methods: &[
            s("getLocale", "()Ljava/lang/String;", Answer::Text("en_us")),
            s("getGameLocale", "()Ljava/lang/String;", Answer::Text("en_us")),
            s("getRobloxLocale", "()Ljava/lang/String;", Answer::Text("en_us")),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "com/roblox/engine/jni/reporter/SessionReporterJavaInterface",
        tier: Tier::One,
        methods: &[
            s("getAppVersion", "()Ljava/lang/String;", Answer::Text("2.738.1397")),
            s("getFilesDir", "()Ljava/lang/String;", Answer::Text("/data/data/com.roblox.client/files")),
            s("getLastLoggedInUser", "()Ljava/lang/String;", Answer::Text("")),
            s("getLastLoggedInUserId", "()Ljava/lang/String;", Answer::Text("")),
            s("sendSessionReport", "(Ljava/lang/String;Ljava/lang/String;)V", Answer::Sink),
            s(
                "setEventTrackingGoogleAnalytics",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;J)V",
                Answer::Sink,
            ),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "com/roblox/engine/jni/model/ClientLocalFlags",
        tier: Tier::One,
        methods: &[
            m("<init>", "()V", Answer::NewInstance),
            m("add", "(Ljava/lang/String;Ljava/lang/String;)V", Answer::Sink),
            m("size", "()I", Answer::Int(0)),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "com/roblox/client/flags/NativeFlagsInitResult",
        tier: Tier::One,
        methods: &[
            m("<init>", "(I)V", Answer::NewInstance),
            m("addBoolean", "(Ljava/lang/String;ZZ)V", Answer::Sink),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "com/roblox/client/startup/MainGameActivity",
        tier: Tier::One,
        methods: &[
            m("getNativeHelper", "()Lcom/roblox/client/startup/NativeHelper;",
                Answer::NewInstanceOf("com/roblox/client/startup/NativeHelper")),
            m("bootstrapTheApp", "()V", Answer::Sink),
            m("syncCookiesFromEngine", "()V", Answer::Sink),
            m("openWebActivity", "(Ljava/lang/String;Ljava/lang/String;)V", Answer::Sink),
            m("showLeaveAppPrompt", "()V", Answer::Sink),
            s("getAppUpgradeKey", "()Ljava/lang/String;", Answer::Text("")),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "com/roblox/client/startup/NativeHelper",
        tier: Tier::One,
        methods: &[
            m("gameActivity_hideKeyboard", "()V", Answer::Sink),
            m("gameActivity_onAppReady", "(Ljava/lang/String;)V", Answer::Sink),
            m("gameActivity_onDidLogInReceived", "(Ljava/lang/String;)V", Answer::Sink),
            m("gameActivity_onDidLogOutReceived", "()V", Answer::Sink),
            m("gameActivity_onDidSignUp", "(Ljava/lang/String;)V", Answer::Sink),
            m("gameActivity_onDidSwitchAccountReceived", "()V", Answer::Sink),
            m("gameActivity_onEngineInitialized", "()V", Answer::Sink),
            m("gameActivity_onExperienceStart", "()V", Answer::Sink),
            m("gameActivity_onExperienceStop", "(D)V", Answer::Sink),
            m("gameActivity_onFlagsFailed", "()V", Answer::Sink),
            m("gameActivity_onFlagsLoaded", "(Ljava/nio/ByteBuffer;)V", Answer::Sink),
            m("gameActivity_onGameLoaded", "(J)V", Answer::Sink),
            m("gameActivity_onGameStreamingStatusChanged", "(Ljava/lang/String;)V", Answer::Sink),
            m("gameActivity_onLuaAppDidReturn", "()V", Answer::Sink),
            m("gameActivity_onLuaTextBoxChanged", "(Ljava/lang/String;)V", Answer::Sink),
            m("gameActivity_onLuaTextBoxPropertyChanged", "()V", Answer::Sink),
            m("gameActivity_onMotionEventListening", "(Ljava/lang/String;)V", Answer::Sink),
            m("gameActivity_onRestartLuaApp", "()V", Answer::Sink),
            m("gameActivity_onScanQrCode", "()V", Answer::Sink),
            m("gameActivity_onScreenOrientationChanged", "(IZ)V", Answer::Sink),
            m("gameActivity_onScreenshotReady", "(Ljava/lang/String;)V", Answer::Sink),
            m(
                "gameActivity_setAppUpgradeStatus",
                "(IILjava/lang/String;Ljava/lang/String;)V",
                Answer::Sink,
            ),
            m(
                "gameActivity_showKeyboard",
                "(JZ[BLcom/roblox/engine/jni/model/NativeTextBoxInfo;)V",
                Answer::Sink,
            ),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "com/snapchat/djinni/NativeObjectManager",
        tier: Tier::One,
        methods: &[m(
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
            Answer::NewInstanceOf("java/lang/ClassLoader"),
        )],
        fields: NONE,
    },
    ClassSpec {
        name: "com/roblox/engine/jni/model/NativeTextBoxInfo",
        tier: Tier::One,
        methods: &[m("<init>", "(FFFFFZIIIIIIZZZ)V", Answer::NewInstance)],
        fields: NONE,
    },
    // ---- the four parameter objects, with the member lists read out of the dex -------------
    //
    // **VERIFIED from `classes2.dex`**, not guessed. `jni-surface.md` Section D leaves these
    // lookups in its `!unresolved` group — the in-binary dataflow could not tie them to a class —
    // and the engine's own `GetFieldID` sites are the only other evidence. So the member lists
    // here were read out of the dex directly, which is what turned "`DeviceStaticParams.osVersion`
    // is missing" from one refusal per member into one list. The dex gives the **names, types and
    // order**; every *value* below is the host's own decision about the device Omnidroid presents.
    ClassSpec {
        name: "com/roblox/engine/jni/model/DeviceStaticParams",
        tier: Tier::One,
        methods: &[m("<init>", "()V", Answer::NewInstance)],
        fields: &[
            f("appBuildVariant", "Ljava/lang/String;", Answer::Text("release")),
            f("appVersion", "Ljava/lang/String;", Answer::Text("2.738.1397")),
            f("cpu64Bit", "Z", Answer::Bool(true)),
            f("deviceName", "Ljava/lang/String;", Answer::Text("Omnidroid")),
            f("deviceSku", "Ljava/lang/String;", Answer::Text("omnidroid")),
            f("manufacturer", "Ljava/lang/String;", Answer::Text("Omnidroid")),
            f("osVersion", "Ljava/lang/String;", Answer::Text("13")),
            f("socModel", "Ljava/lang/String;", Answer::Text("omnidroid-host")),
        ],
    },
    ClassSpec {
        name: "com/roblox/engine/jni/model/DeviceParams",
        tier: Tier::One,
        methods: &[m("<init>", "()V", Answer::NewInstance)],
        fields: &[
            f("appBuildVariant", "Ljava/lang/String;", Answer::Text("release")),
            f("appVersion", "Ljava/lang/String;", Answer::Text("2.738.1397")),
            f("country", "Ljava/lang/String;", Answer::Text("US")),
            f("cpu64Bit", "Z", Answer::Bool(true)),
            f("deviceName", "Ljava/lang/String;", Answer::Text("Omnidroid")),
            f("deviceSku", "Ljava/lang/String;", Answer::Text("omnidroid")),
            // 2 GiB, the same budget the gate gives `sysinfo`. Stated once there and once here
            // is already two places; if a third appears, the figure needs a constant.
            f("deviceTotalMemoryMB", "I", Answer::Int(2048)),
            f("displayPhysicalHeightPixels", "I", Answer::Int(1080)),
            f("displayPhysicalWidthPixels", "I", Answer::Int(1920)),
            f("displayResolution", "Ljava/lang/String;", Answer::Text("1920x1080")),
            f("isChrome", "Z", Answer::Bool(false)),
            f("isLowRamDevice", "Z", Answer::Bool(false)),
            f("largeMemoryClass", "I", Answer::Int(512)),
            f("lowMemoryKillerBackgroundAppThreshold", "J", Answer::Long(0)),
            f("lowMemoryKillerForegroundAppThreshold", "J", Answer::Long(0)),
            f("manufacturer", "Ljava/lang/String;", Answer::Text("Omnidroid")),
            f("memoryClass", "I", Answer::Int(256)),
            f("networkType", "Ljava/lang/String;", Answer::Text("wifi")),
            f("osVersion", "Ljava/lang/String;", Answer::Text("13")),
            f("socModel", "Ljava/lang/String;", Answer::Text("omnidroid-host")),
            f("testDeviceName", "Ljava/lang/String;", Answer::Text("")),
        ],
    },
    ClassSpec {
        name: "com/roblox/engine/jni/model/PlatformParams",
        tier: Tier::One,
        methods: &[m("<init>", "()V", Answer::NewInstance)],
        fields: &[
            f("assetFolderPath", "Ljava/lang/String;", Answer::Text("")),
            f("dpiScale", "F", Answer::Float(1.0)),
            f("isKeyboardDevice", "Z", Answer::Bool(true)),
            f("isMouseDevice", "Z", Answer::Bool(true)),
            f("isTouchDevice", "Z", Answer::Bool(false)),
            f("viewportHeightMm", "I", Answer::Int(0)),
            f("viewportWidthMm", "I", Answer::Int(0)),
        ],
    },
    // `InitParams` is an AutoValue interface: accessors, not fields. §8 step 12 names seven of
    // these (`platformParams`, `deviceParams`, `baseURL`, `userAgent`, `isTablet`, `isPotato`,
    // `isVrDevice`) from `MainGameActivity.E2`'s builder calls; the dex adds `buildVariant` and
    // `vrContext`.
    ClassSpec {
        name: "com/roblox/engine/jni/autovalue/InitParams",
        tier: Tier::One,
        methods: &[
            m("<init>", "()V", Answer::NewInstance),
            m("baseURL", "()Ljava/lang/String;", Answer::Text("https://www.roblox.com")),
            m("buildVariant", "()Ljava/lang/String;", Answer::Text("release")),
            m(
                "deviceParams",
                "()Lcom/roblox/engine/jni/model/DeviceParams;",
                Answer::NewInstanceOf("com/roblox/engine/jni/model/DeviceParams"),
            ),
            m("isPotato", "()Z", Answer::Bool(false)),
            m("isTablet", "()Z", Answer::Bool(false)),
            m("isVrDevice", "()Z", Answer::Bool(false)),
            m(
                "platformParams",
                "()Lcom/roblox/engine/jni/model/PlatformParams;",
                Answer::NewInstanceOf("com/roblox/engine/jni/model/PlatformParams"),
            ),
            m("userAgent", "()Ljava/lang/String;", Answer::Text("Roblox/Android")),
            // There is no VR activity, and `null` is what a device without one answers.
            m("vrContext", "()Landroid/app/Activity;", Answer::Null),
        ],
        fields: NONE,
    },
    // ---- support: the framework shape the engine reads a Context through -------------------
    ClassSpec {
        name: "android/content/Context",
        tier: Tier::One,
        methods: &[m(
            "getResources",
            "()Landroid/content/res/Resources;",
            Answer::NewInstanceOf("android/content/res/Resources"),
        )],
        fields: NONE,
    },
    ClassSpec {
        name: "android/app/Application",
        tier: Tier::Support,
        methods: &[m("getResources", "()Landroid/content/res/Resources;", Answer::Unanswered)],
        fields: NONE,
    },
    ClassSpec {
        name: "android/content/res/Resources",
        tier: Tier::One,
        methods: &[m(
            "getDisplayMetrics",
            "()Landroid/util/DisplayMetrics;",
            Answer::NewInstanceOf("android/util/DisplayMetrics"),
        )],
        fields: NONE,
    },
    ClassSpec {
        name: "android/util/DisplayMetrics",
        tier: Tier::One,
        methods: NONE,
        fields: &[
            f("density", "F", Answer::Float(0.0)),
            f("widthPixels", "I", Answer::Int(0)),
            f("heightPixels", "I", Answer::Int(0)),
            f("xdpi", "F", Answer::Float(0.0)),
            f("ydpi", "F", Answer::Float(0.0)),
        ],
    },
    ClassSpec {
        name: "android/os/LocaleList",
        tier: Tier::One,
        methods: &[
            m("size", "()I", Answer::Int(1)),
            m("get", "(I)Ljava/util/Locale;", Answer::NewInstanceOf("java/util/Locale")),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "java/util/Locale",
        tier: Tier::One,
        methods: &[
            m("getLanguage", "()Ljava/lang/String;", Answer::Text("en")),
            m("getCountry", "()Ljava/lang/String;", Answer::Text("US")),
            m("getScript", "()Ljava/lang/String;", Answer::Text("")),
            m("getVariant", "()Ljava/lang/String;", Answer::Text("")),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "java/util/HashMap",
        tier: Tier::One,
        methods: &[
            m("<init>", "()V", Answer::NewInstance),
            m("<init>", "(I)V", Answer::NewInstance),
            m("put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", Answer::Null),
            m("size", "()I", Answer::Int(0)),
            m("entrySet", "()Ljava/util/Set;", Answer::Unanswered),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "java/util/List",
        tier: Tier::One,
        methods: &[
            // The empty list: `size()` is 0, `get(I)` is never reached from it, and `toArray()`
            // really is an empty array. §8 step 11 hands one to
            // `nativeSetAppPreviousExitReasons`, and a device with no recorded exits hands the
            // same thing. M4's gate found `size` missing: the engine called it with a null
            // `jmethodID` it had not checked.
            m("size", "()I", Answer::Int(0)),
            m("isEmpty", "()Z", Answer::Bool(true)),
            m("get", "(I)Ljava/lang/Object;", Answer::Null),
            m("toArray", "()[Ljava/lang/Object;", Answer::EmptyObjectArray),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "java/util/Map$Entry",
        tier: Tier::Three,
        methods: &[
            m("getKey", "()Ljava/lang/Object;", Answer::Null),
            m("getValue", "()Ljava/lang/Object;", Answer::Null),
        ],
        fields: NONE,
    },
    // Djinni boxes optionals through these five.
    ClassSpec {
        name: "java/lang/Boolean",
        tier: Tier::One,
        methods: &[m("booleanValue", "()Z", Answer::Bool(false))],
        fields: NONE,
    },
    ClassSpec {
        name: "java/lang/Integer",
        tier: Tier::One,
        methods: &[m("intValue", "()I", Answer::Int(0))],
        fields: NONE,
    },
    ClassSpec {
        name: "java/lang/Long",
        tier: Tier::One,
        methods: &[
            m("<init>", "(J)V", Answer::NewInstance),
            m("longValue", "()J", Answer::Long(0)),
        ],
        fields: NONE,
    },
    ClassSpec {
        name: "java/lang/Float",
        tier: Tier::One,
        methods: &[m("floatValue", "()F", Answer::Float(0.0))],
        fields: NONE,
    },
    ClassSpec {
        name: "java/lang/Double",
        tier: Tier::One,
        methods: &[m("doubleValue", "()D", Answer::Double(0.0))],
        fields: NONE,
    },
    ClassSpec { name: "java/lang/Object", tier: Tier::Support, methods: NONE, fields: NONE },
    // `GetObjectClass(jclass)` answers with this, and `JvmClassLoaderHelper` then asks it for the
    // app ClassLoader. See `env::class_of`.
    ClassSpec {
        name: "java/lang/Class",
        tier: Tier::Zero,
        methods: &[
            m("getClassLoader", "()Ljava/lang/ClassLoader;",
                Answer::NewInstanceOf("java/lang/ClassLoader")),
            m("getName", "()Ljava/lang/String;", Answer::Unanswered),
        ],
        fields: NONE,
    },
    // ---- what `JNI_OnLoad` reaches that §3.1 ranks Tier 3 ---------------------------------
    //
    // **Every one of these was found by running M4's gate**, not predicted: `JNI_OnLoad` looks
    // them up, the lookup missed, and `Jni::misses` named them. Member lists from `classes2.dex`
    // for the two app classes; `android/util/Log`'s single member is Section D's.
    ClassSpec {
        name: "android/util/Log",
        tier: Tier::Three,
        // The engine prints `<no trace>` itself when it has no stack trace, which is what it
        // printed while this class was undeclared — so an empty string is the answer it is
        // already written to handle, not a placeholder.
        methods: &[s(
            "getStackTraceString",
            "(Ljava/lang/Throwable;)Ljava/lang/String;",
            Answer::Text(""),
        )],
        fields: NONE,
    },
    ClassSpec {
        name: "com/roblox/audio/AppRtcDeviceWrapper",
        tier: Tier::Three,
        methods: &[
            m("<init>", "(J)V", Answer::NewInstance),
            m("getSelectedAudioDeviceAsInt", "()I", Answer::Int(0)),
            m("getSelectedAudioDeviceName", "()Ljava/lang/String;", Answer::Text("")),
            // There is no audio device here, and `false` is what the engine's own code is written
            // to branch on. Answering `true` would be the believable wrong answer: it would make
            // the engine route audio at something that does not exist.
            m("isValid", "()Z", Answer::Bool(false)),
            m("wrapSetCommunicationMute", "(Z)V", Answer::Sink),
            m("wrapStartCommunication", "()V", Answer::Sink),
            m("wrapStopCommunication", "()V", Answer::Sink),
        ],
        fields: &[f("nativeReference", "J", Answer::Long(0))],
    },
    ClassSpec {
        name: "org/fmod/MediaCodec",
        tier: Tier::Three,
        methods: &[
            m("<init>", "()V", Answer::NewInstance),
            m("getChannelCount", "()I", Answer::Int(0)),
            m("getLength", "()J", Answer::Long(0)),
            m("getSampleRate", "()I", Answer::Int(0)),
            // No decoder, so initialisation fails — the same argument as `AudioDevice.init`.
            m("init", "(J)Z", Answer::Bool(false)),
            m("read", "([BI)I", Answer::Int(0)),
            m("release", "()V", Answer::Sink),
            m("seek", "(I)V", Answer::Sink),
            // The two `RegisterNatives`-only natives §4 counts for this class. Declared so that
            // the engine's own `RegisterNatives` call binds them rather than recording a miss.
            s("fmodGetSize", "(J)J", Answer::Native),
            s("fmodReadAt", "(JJ[BII)I", Answer::Native),
        ],
        fields: &[
            f("mChannelCount", "I", Answer::Int(0)),
            f("mCodecPtr", "J", Answer::Long(0)),
            f("mCurrentOutputBufferIndex", "I", Answer::Int(0)),
            f("mDataSourceProxy", "Ljava/lang/Object;", Answer::Null),
            f("mInputFinished", "Z", Answer::Bool(true)),
            f("mLength", "J", Answer::Long(0)),
            f("mOutputFinished", "Z", Answer::Bool(true)),
            f("mSampleRate", "I", Answer::Int(0)),
        ],
    },
    ClassSpec {
        name: "org/fmod/AudioDevice",
        tier: Tier::Three,
        methods: &[
            m("<init>", "()V", Answer::NewInstance),
            m("close", "()V", Answer::Sink),
            // As `isValid` above: no device, so the initialisation fails, which is a state FMOD
            // handles on a real device with no audio output.
            m("init", "(IIII)Z", Answer::Bool(false)),
            m("write", "([BI)V", Answer::Sink),
        ],
        fields: NONE,
    },
    // ---- the exceptions a failed lookup leaves pending ------------------------------------
    ClassSpec {
        name: "java/lang/ClassNotFoundException",
        tier: Tier::Support,
        methods: NONE,
        fields: NONE,
    },
    ClassSpec {
        name: "java/lang/NoSuchMethodError",
        tier: Tier::Support,
        methods: NONE,
        fields: NONE,
    },
    ClassSpec {
        name: "java/lang/NoSuchFieldError",
        tier: Tier::Support,
        methods: NONE,
        fields: NONE,
    },
    ClassSpec {
        name: "java/lang/RuntimeException",
        tier: Tier::Support,
        methods: NONE,
        fields: NONE,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::values::Descriptor;
    use std::collections::BTreeSet;

    #[test]
    fn every_declared_descriptor_parses() {
        for spec in DECLARED {
            for member in spec.methods {
                Descriptor::parse("test", 0, member.descriptor).unwrap_or_else(|error| {
                    panic!("{}.{} {}: {error}", spec.name, member.name, member.descriptor)
                });
            }
            for field in spec.fields {
                // A field descriptor is one type, not a method descriptor: wrap it so the same
                // grammar checks it.
                let wrapped = format!("(){}", field.descriptor);
                Descriptor::parse("test", 0, &wrapped).unwrap_or_else(|error| {
                    panic!("{}.{} {}: {error}", spec.name, field.name, field.descriptor)
                });
            }
        }
    }

    /// A duplicate `(name, descriptor, static)` within one class would make [`Registry::method`]
    /// answer with whichever came first, and the two `promptNativePurchase` overloads are exactly
    /// the shape that hides one.
    #[test]
    fn no_class_declares_the_same_member_twice() {
        for spec in DECLARED {
            let mut seen = BTreeSet::new();
            for member in spec.methods {
                assert!(
                    seen.insert((member.name, member.descriptor, member.is_static)),
                    "{}.{}{} is declared twice",
                    spec.name,
                    member.name,
                    member.descriptor
                );
            }
            let mut seen = BTreeSet::new();
            for field in spec.fields {
                assert!(
                    seen.insert((field.name, field.descriptor, field.is_static)),
                    "{}.{} is declared twice",
                    spec.name,
                    field.name
                );
            }
        }
    }

    /// The two tables together, and the **precedence between them**: a class the hand-written
    /// table declares keeps its decided answers after the generated surface has been merged in.
    #[test]
    fn the_registry_builds_and_finds_what_it_declared() {
        let registry = Registry::with_declared();
        // Every hand-written class is there, and the generated surface added the rest.
        assert!(registry.class_count() >= DECLARED.len());
        assert!(registry.class_count() <= DECLARED.len() + super::super::surface::DEX_CLASSES);
        for spec in DECLARED {
            assert!(registry.find(spec.name).is_some(), "{} was dropped", spec.name);
        }
        // Precedence: `NativeUserJavaInterface.getUserId` is decided by hand and the generated
        // surface declares the same member as `Unanswered`. The hand-written answer must win,
        // and this is the assertion that a merge in the wrong order would fail.
        let id = registry.find("com/roblox/engine/jni/user/NativeUserJavaInterface").expect("declared");
        let method = registry.method(id, "getUserId", "()J", true).expect("declared");
        assert_eq!(registry.member(method).expect("declared").answer, Answer::Long(0));
        // And a member only the generated surface has resolves, with no answer decided.
        let id = registry.find("org/fmod/FMOD").expect("the generated surface declares it");
        assert!(registry.class(id).expect("declared").methods.iter().any(|m| m.answer == Answer::Unanswered));
        let id = registry.find("com/google/androidgamesdk/GameActivity").expect("Tier 0");
        assert_eq!(registry.class(id).expect("declared").tier, Tier::Zero);
        assert!(registry.find("com/roblox/gloop/Loader").is_none(), "the injected payload is not declared");
    }

    /// §3.1's Tier 0 list, as a membership assertion rather than a count. A count cannot see a
    /// substitution — the lesson this project has paid for three times.
    #[test]
    fn every_tier_zero_class_of_the_spec_is_declared_with_its_members() {
        let registry = Registry::with_declared();
        let required: &[(&str, &[&str])] = &[
            (
                "com/google/androidgamesdk/GameActivity",
                &["finish", "setWindowFlags", "getWindowInsets", "getWaterfallInsets", "setImeEditorInfoFields"],
            ),
            ("androidx/core/graphics/Insets", &["left", "top", "right", "bottom"]),
            (
                "androidx/core/view/WindowInsetsCompat$Type",
                &[
                    "statusBars", "navigationBars", "captionBar", "displayCutout", "ime",
                    "mandatorySystemGestures", "systemGestures", "systemBars", "tappableElement",
                ],
            ),
            (
                "android/content/res/Configuration",
                &[
                    "mcc", "mnc", "orientation", "touchscreen", "keyboard", "keyboardHidden",
                    "hardKeyboardHidden", "navigation", "navigationHidden", "screenLayout",
                    "uiMode", "screenWidthDp", "screenHeightDp", "smallestScreenWidthDp",
                    "densityDpi", "colorMode", "fontScale", "fontWeightAdjustment", "getLocales",
                ],
            ),
            ("java/lang/String", &["getBytes"]),
            ("android/app/ActivityThread", &["currentActivityThread", "currentApplication", "getApplication"]),
            ("java/lang/ClassLoader", &["loadClass", "findClass", "getClassLoader"]),
        ];
        for (class, members) in required {
            let id = registry.find(class).unwrap_or_else(|| panic!("{class} is Tier 0 and must be declared"));
            let declared = registry.class(id).expect("just found");
            assert_eq!(declared.tier, Tier::Zero, "{class}");
            let present: BTreeSet<&str> = declared
                .methods
                .iter()
                .chain(declared.fields.iter())
                .map(|m| m.name.as_str())
                .collect();
            for member in *members {
                assert!(present.contains(member), "{class}.{member} is missing");
            }
        }
        // The 18 Configuration fields, counted as well as named, because §3.1 states the number.
        let id = registry.find("android/content/res/Configuration").expect("declared");
        assert_eq!(registry.class(id).expect("declared").fields.len(), 18);
    }

    /// The nine inset-type masks must be distinct bits, or the mask the engine builds out of them
    /// loses information. `systemBars` is deliberately the union of three and is excluded.
    #[test]
    fn the_inset_type_masks_are_distinct_bits() {
        let mut seen = 0i32;
        for member in WINDOW_INSETS_TYPE {
            let Answer::Int(bit) = member.answer else { panic!("{} is not an int", member.name) };
            if member.name == "systemBars" {
                continue;
            }
            assert_eq!(bit.count_ones(), 1, "{} is not a single bit", member.name);
            assert_eq!(seen & bit, 0, "{} repeats a bit", member.name);
            seen |= bit;
        }
    }

    /// `Answer::Unanswered` must not evaluate to anything. It is the variant that refuses, and a
    /// `simple_answer` that produced a value for it would be exactly the plausible wrong answer
    /// Global Constraint 1 is about.
    #[test]
    fn the_unanswered_answer_evaluates_to_nothing() {
        assert!(Registry::simple_answer(Answer::Unanswered).is_none());
        assert!(Registry::simple_answer(Answer::Native).is_none());
        assert!(Registry::simple_answer(Answer::Field("x")).is_none());
        assert!(Registry::simple_answer(Answer::NewInstance).is_none());
        assert_eq!(Registry::simple_answer(Answer::Sink), Some(Value::Void));
        assert_eq!(Registry::simple_answer(Answer::Int(7)), Some(Value::Int(7)));
    }

    /// A miss is recorded once per distinct lookup and the list is bounded, because a guest in a
    /// loop looking up a member that does not exist must not grow a host `Vec` without limit.
    #[test]
    fn misses_are_recorded_once_and_bounded() {
        let mut registry = Registry::with_declared();
        let miss = Miss {
            function: "GetMethodID".to_string(),
            class: "com/roblox/platform/util/DeviceUtils".to_string(),
            member: "getScreenPhysicalSizeInMillimeters".to_string(),
            descriptor: "(Landroid/content/Context;)Landroid/graphics/Point;".to_string(),
        };
        for _ in 0..10 {
            registry.record_miss(miss.clone());
        }
        assert_eq!(registry.misses().len(), 1);
        for index in 0..MAX_MISSES * 2 {
            registry.record_miss(Miss { member: index.to_string(), ..miss.clone() });
        }
        assert_eq!(registry.misses().len(), MAX_MISSES);
    }
}
