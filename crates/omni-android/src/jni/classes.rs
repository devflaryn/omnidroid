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
    /// A static field that holds **the one instance of its own class** -- a Kotlin `object`'s
    /// `INSTANCE`, which `<clinit>` creates with `new-instance` and stores with `sput-object`,
    /// once.
    ///
    /// Made on the first read and the **same object on every read after it**, held by the JNI
    /// instance for its life the way a class holds its statics: a fresh object per read would
    /// tell `IsSameObject` two reads of one field differ, and Djinni's proxy cache -- the
    /// engine's reason for reading `PlatformSystemDialogHandler.INSTANCE` -- is keyed on
    /// identity. Its fields are unset and its methods answer what they are declared to, so the
    /// first thing the engine asks of it refuses by name rather than being invented.
    ///
    /// Only for a static field whose descriptor is `L<its own class>;`, which is checked where it
    /// is read: this variant claims the class's own `<clinit>` stored a fresh instance of the
    /// class, and anywhere else that claim is false.
    StaticInstance,
    /// A static object field **the app's own Java code assigns** with `sput-object`: Java `null`
    /// until the scripted Java statement that assigns it has run
    /// ([`super::script::JavaStatement`], through [`super::Jni::put_static_object`]), and the
    /// object it stored from then on.
    ///
    /// Not [`StaticInstance`](Answer::StaticInstance), whose value the class's own `<clinit>`
    /// makes on first read: this one is written by a statement the host executes at the point of
    /// the startup sequence where the app's bytecode executes it, so what it answers is a
    /// function of **which steps have run** -- which is the whole reason it exists. A constant
    /// here would be claiming the step had run whether it had or not.
    Assigned,
    /// `static boolean m() { return <field> != null; }` -- a static method whose entire body tests
    /// the named static object field of its own class, answered from what that field holds
    /// **now**, read exactly as `GetStaticObjectField` would read it.
    ///
    /// `org.fmod.FMOD.checkInit()` is this and nothing else (`sget-object gContext; if-eqz`), so
    /// its answer follows `FMOD.init(Context)` having run rather than being chosen.
    StaticIsSet(&'static str),
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
    /// `System.identityHashCode(Object)`: [`ObjectId::identity_hash`](super::refs::ObjectId::identity_hash)
    /// of argument 0, and `0` for `null` (its javadoc). A function of identity, not a value
    /// chosen to look like one: the same object answers the same number for its whole life,
    /// which is the entire contract.
    IdentityHash,
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
    /// Its nearest **declared** ancestor, if [`EXTENDS`] names one.
    ///
    /// Filled after every class is declared, because an edge may name a class that is declared
    /// later in [`DECLARED`] or only by the generated surface.
    pub superclass: Option<ClassId>,
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
        // **Last**, because an edge may name a class the generated surface is what declares.
        registry.link_superclasses();
        registry
    }

    /// Resolve [`EXTENDS`] into [`Class::superclass`].
    ///
    /// An edge whose either end is not declared is **dropped silently and deliberately**: the
    /// table is a fact about the APK, not a requirement on this registry, and a host that
    /// declared a narrower surface should not be refused for it. Nothing can be resolved *by* a
    /// dropped edge, so a mistake here surfaces as the miss it already was.
    fn link_superclasses(&mut self) {
        for (subclass, superclass) in EXTENDS {
            let (Some(sub), Some(sup)) = (self.find(subclass), self.find(superclass)) else {
                continue;
            };
            if sub == sup {
                continue;
            }
            self.classes[usize::from(sub.0)].superclass = Some(sup);
        }
    }

    /// `class` and every declared ancestor of it, nearest first.
    ///
    /// **Bounded by the number of classes**, so a cycle in [`EXTENDS`] ends the walk instead of
    /// hanging a guest thread inside a `GetMethodID`. `the_superclass_chain_is_acyclic` asserts
    /// there is no cycle; this bound is what makes that assertion a statement about the data
    /// rather than the only thing standing between a typo and a hang.
    fn ancestry(&self, class: ClassId) -> impl Iterator<Item = ClassId> + '_ {
        let mut next = Some(class);
        let mut budget = self.classes.len() + 1;
        std::iter::from_fn(move || {
            let current = next?;
            budget = budget.checked_sub(1)?;
            next = self.class(current).and_then(|declared| declared.superclass);
            Some(current)
        })
    }

    /// Whether `class` is `ancestor` or has it on its **declared** superclass chain -- Java's
    /// assignability, as far as [`EXTENDS`] states it.
    ///
    /// What a store into a field checks before it lets an object in: a field typed
    /// `Landroid/content/Context;` can hold a `MainGameActivity` because the chain says so, and a
    /// `java/util/List` would be a value the Java verifier would never have let through.
    #[must_use]
    pub fn extends(&self, class: ClassId, ancestor: ClassId) -> bool {
        self.ancestry(class).any(|at| at == ancestor)
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
            if self.declared_method(id, member.name, member.descriptor, member.is_static).is_none() {
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
            if self.declared_field(id, member.name, member.descriptor, member.is_static).is_none() {
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
            superclass: None,
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

    /// Resolve a method by name and descriptor, **walking the superclass chain**.
    ///
    /// JNI's `GetMethodID` and `GetStaticMethodID` both search the superclasses, and the engine
    /// depends on it: §8 row 23's helpers do `GetObjectClass(activity->javaGameActivity)` once
    /// and then ask that one `jclass` for members declared at three different levels. See
    /// [`EXTENDS`]. The returned [`MethodId`] names the class that **declares** the member, which
    /// is what decides the [`Answer`].
    #[must_use]
    pub fn method(&self, class: ClassId, name: &str, descriptor: &str, is_static: bool) -> Option<MethodId> {
        self.ancestry(class).find_map(|at| self.declared_method(at, name, descriptor, is_static))
    }

    /// Resolve a method **without** walking the superclass chain.
    ///
    /// `RegisterNatives` and [`extend_with`](Registry::extend_with) use this rather than
    /// [`method`](Registry::method): JNI requires `RegisterNatives` to name a method of the class
    /// it is given, and a merge that saw an inherited member as "already present" would refuse to
    /// add the subclass's own declaration of it.
    #[must_use]
    pub fn declared_method(&self, class: ClassId, name: &str, descriptor: &str, is_static: bool) -> Option<MethodId> {
        let declared = self.class(class)?;
        declared
            .methods
            .iter()
            .position(|m| m.name == name && m.descriptor == descriptor && m.is_static == is_static)
            .map(|member| MethodId { class, member: member as u16 })
    }

    /// Resolve a field by name and descriptor, walking the superclass chain. As
    /// [`method`](Registry::method).
    #[must_use]
    pub fn field(&self, class: ClassId, name: &str, descriptor: &str, is_static: bool) -> Option<FieldId> {
        self.ancestry(class).find_map(|at| self.declared_field(at, name, descriptor, is_static))
    }

    /// Resolve a field without walking the chain. As
    /// [`declared_method`](Registry::declared_method).
    #[must_use]
    pub fn declared_field(&self, class: ClassId, name: &str, descriptor: &str, is_static: bool) -> Option<FieldId> {
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
            | Answer::StaticInstance
            | Answer::Assigned
            | Answer::StaticIsSet(_)
            | Answer::NewInstanceOf(_)
            | Answer::ResolveClass
            | Answer::StringBytes
            | Answer::IdentityHash
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

/// The superclass edges this layer models: `(subclass, its nearest declared ancestor)`.
///
/// # Why a registry that is "a flat `(class, name, descriptor)` registry" needs a chain at all
///
/// §6's recommended architecture is a flat registry, and it was flat until M5's gate reached
/// `NativeEngine::initializing`. **MEASURED there, n = 1 run:** §8 row 23's helpers take the
/// `jobject` at `NativeCode + 0x18` — §5.2 step 8's `activity->javaGameActivity` — do
/// `GetObjectClass` on it **once**, and then ask that single `jclass` for members declared at
/// three different levels of the Java hierarchy:
///
/// | guest pc | `GetMethodID` asks for | declared by |
/// |---|---|---|
/// | `0x02bdad0c` | `getResources` `()Landroid/content/res/Resources;` | `android/content/Context` |
/// | `0x02bd8b80` | `getNativeHelper` `()Lcom/roblox/client/startup/NativeHelper;` | `com/roblox/client/startup/MainGameActivity` |
///
/// Neither is a member of `com/google/androidgamesdk/GameActivity`, and the lists file agrees —
/// it attributes them to `android/content/Context` and `com/roblox/client/startup/MainGameActivity`
/// respectively, and gives `GameActivity` exactly five members. **No flat class can answer both**,
/// so a flat registry could only be made to pass by declaring members on a class that does not
/// have them, which is the plausible-stub shape Global Constraint 1 forbids one level up.
///
/// Each of these is a `GetObjectClass` → `GetMethodID` → `CallObjectMethod` with **no `cbz` in
/// between** — the ids are never tested, which is `jni-surface.md` §8.1's third failure mode, and
/// it surfaces as `CallObjectMethodV` being handed `0x0`.
///
/// # The two edges, and the evidence for each
///
/// * **`MainGameActivity extends GameActivity`** — VERIFIED, `apk-analysis.md`'s activity table
///   and §5.3: "`MainGameActivity extends com.google.androidgamesdk.GameActivity`". It is also
///   why `"com/roblox/client/startup/MainGameActivity"` appears in **zero** `.rodata` string
///   literals of `libroblox.so` (Section B lists all 128 class-name literals and it is not among
///   them) while five of its members are looked up: the engine never `FindClass`es it, it only
///   ever reaches it through `GetObjectClass` of the activity it was handed.
/// * **`GameActivity extends android/content/Context`** — the *nearest declared* ancestor, not
///   the immediate one. §5.3 measured the real chain as `GameActivity` → `Lj/b;`
///   (AppCompatActivity) → `Activity` → `ContextThemeWrapper` → `ContextWrapper` → `Context`.
///   None of those four intermediates is on the measured surface — no string literal, no member
///   looked up — so declaring them would be four claims about a surface nobody measured. The edge
///   records the relation that is load-bearing and the doc records the elision.
///
/// An edge whose either end is undeclared is dropped by
/// [`link_superclasses`](Registry::link_superclasses).
pub static EXTENDS: &[(&str, &str)] = &[
    ("com/roblox/client/startup/MainGameActivity", "com/google/androidgamesdk/GameActivity"),
    ("com/google/androidgamesdk/GameActivity", "android/content/Context"),
];

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

/// A static field.
const fn sf(name: &'static str, descriptor: &'static str, answer: Answer) -> MemberSpec {
    MemberSpec { name, descriptor, is_static: true, answer }
}

const NONE: &[MemberSpec] = &[];

// ------------------------------------------------------------------- Tier 0, §3.1

/// `com/google/androidgamesdk/GameActivity` — five `CHECK_NOT_NULL` members.
///
/// It has no `getResources`, and that is correct: see [`EXTENDS`], which is where the engine's
/// `activity.getResources()` is answered from.
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

/// The lowest `Build.VERSION.SDK_INT` at which `org.fmod.FMOD.supportsAAudio()` answers true.
///
/// Read out of `classes2.dex`, where it is the method's whole body:
///
/// ```text
/// FMOD.supportsAAudio()Z:
///   0000: sget v0, Landroid/os/Build$VERSION;->SDK_INT:I
///   0002: const/16 v1, #27
///   0004: if-lt v0, v1, -> 0008
///   0006: const/4 v0, #1 ; return v0
///   0008: const/4 v0, #0 ; return v0
/// ```
pub const FMOD_AAUDIO_MIN_SDK: i32 = 27;

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
            // **Signed out on a fresh install, as the APK's own code answers it -- DECODED, and
            // four of these were wrong until it was.** The comment here used to say every value
            // was "what a device answers for a signed-out app"; for `getUserId` (0), `getIsUnder13`
            // (false), `getTheme` ("Dark") and `getPlatformName` ("Android") it was not, and the
            // engine said so: with a user id of 0 it took `SingleSurfaceApp::userDidLogin` and
            // dereferenced null at `0x2256548`, on the first run that started the Lua app.
            //
            // The chain, from `classes2.dex`: every one of these delegates to `sImplementation`,
            // which `ej.b.m` sets to a `wi.c`, which reads the session singleton `ok.c` -- and
            // `ok.c.<init>` sets the user id to **-1** and under-13 to **true**; only `ok.c.v(J)`,
            // the login path, changes the id. The username and display name are `null` there and
            // answered as ""; membership 0; subscription false; the theme is `ok.c.k`, which
            // `<clinit>` sets to `wl.a.LIGHT`, whose `toString()` is "Light"; the alternate name
            // is `wi.c.a()`'s literal ""; and `getPlatformName` is `bl.a.d()`, which `wi.c` does
            // not override, returning "".
            s("getUserId", "()J", Answer::Long(-1)),
            s("getUsername", "()Ljava/lang/String;", Answer::Text("")),
            s("getDisplayName", "()Ljava/lang/String;", Answer::Text("")),
            s("getAlternateName", "()Ljava/lang/String;", Answer::Text("")),
            s("getIsUnder13", "()Z", Answer::Bool(true)),
            s("getMembershipType", "()I", Answer::Int(0)),
            s("getTheme", "()Ljava/lang/String;", Answer::Text("Light")),
            s("getPlatformName", "()Ljava/lang/String;", Answer::Text("")),
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
            // `Integer.toString(Build.VERSION.SDK_INT)` in the dex -- see `ANDROID_SDK_INT`.
            f("osVersion", "Ljava/lang/String;", Answer::Text(super::script::ANDROID_SDK_INT)),
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
            // `Integer.toString(Build.VERSION.SDK_INT)` in the dex -- see `ANDROID_SDK_INT`.
            f("osVersion", "Ljava/lang/String;", Answer::Text(super::script::ANDROID_SDK_INT)),
            f("socModel", "Ljava/lang/String;", Answer::Text("omnidroid-host")),
            f("testDeviceName", "Ljava/lang/String;", Answer::Text("")),
        ],
    },
    ClassSpec {
        name: "com/roblox/engine/jni/model/PlatformParams",
        tier: Tier::One,
        methods: &[m("<init>", "()V", Answer::NewInstance)],
        fields: &[
            // Both builders (`fi.o.f`, `fi.h0.p`) take it from `vk.b.m().n()` -- the same call
            // that produces what `nativeSetAssetPath` is handed, so the same constant.
            f("assetFolderPath", "Ljava/lang/String;", Answer::Text(super::script::ASSET_PATH)),
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
            // **Unanswered until an embedding defines them**: they describe the host's display, and
            // the zeros these used to answer were a device with no pixels and no density -- which
            // MEASURED, the renderer divided by (a 0x0 light-grid texture and its own HardAssert).
            f("density", "F", Answer::Unanswered),
            f("widthPixels", "I", Answer::Unanswered),
            f("heightPixels", "I", Answer::Unanswered),
            f("xdpi", "F", Answer::Unanswered),
            f("ydpi", "F", Answer::Unanswered),
        ],
    },
    ClassSpec {
        name: "android/os/Build",
        tier: Tier::One,
        // **Declared because every device has it.** MEASURED: two workers asked
        // `FindClass("android/os/Build")`, got the null an undeclared class answers, and passed it
        // to `GetStaticMethodID(.., "isDebuggerConnected", "()Z")` -- a null `jclass`, which ART
        // aborts on. On a device the class is found and **the method is not** (it is
        // `android.os.Debug`'s, not `Build`'s), so the lookup answers null with
        // `NoSuchMethodError` pending, which the engine handles; declaring the class with no such
        // method gives exactly that. FMOD reads `MANUFACTURER` from it (`0x4fc0118`): the
        // hardware's maker, an embedding's to state, so it refuses until one does.
        methods: NONE,
        fields: &[sf("MANUFACTURER", "Ljava/lang/String;", Answer::Unanswered)],
    },
    ClassSpec {
        name: "android/os/Build$VERSION",
        tier: Tier::One,
        // MEASURED: with `Build` found, the same workers' next `FindClass` was this, and its null
        // again reached `GetStaticMethodID(.., "isDebuggerConnected")` -- the engine's lookups
        // short-circuit on the first that fails. Every device has it. `SDK_INT` is
        // `script::ANDROID_SDK_LEVEL`, the one figure every other SDK answer here reads
        // (`ro.build.version.sdk`, `DeviceStaticParams.osVersion`, `FMOD.supportsAAudio`), and
        // FMOD reads this field itself (`0x4fc0088`..`0x4fc00ac`).
        methods: NONE,
        fields: &[sf("SDK_INT", "I", Answer::Int(super::script::ANDROID_SDK_LEVEL))],
    },
    ClassSpec {
        name: "android/os/Debug",
        tier: Tier::One,
        // MEASURED: a worker asks `Debug.isDebuggerConnected()` (the null-class death that named
        // it, once `FindClass` had answered null for the undeclared class). **`false` is a fact of
        // this runtime, not a default**: a Java debugger attaches through JDWP to a VM, and there
        // is no VM here and no JDWP agent -- nothing can be connected. A native debugger on the
        // host process is not what this method reports on a device either.
        methods: &[s("isDebuggerConnected", "()Z", Answer::Bool(false))],
        fields: NONE,
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
    // ---- FMOD's Android glue: whether the app has handed it a Context, and AAudio -----------
    //
    // **MEASURED, gate run 62**: after the logged-out landing screen reloaded its patch, guest
    // thread 6 died on `CallStaticBooleanMethodV` of `checkInit()Z`, called from `0x4fc0284` --
    // the first JNI call FMOD makes in the whole process. The chain is Roblox's
    // `FmodManager::initializeOnce` (`0x2f0421c`) -> `FMOD::System_Create` (`0x2f04330` ->
    // `0x4f47984`) -> the global init (`0x4f578c0`, first reference only) -> FMOD's Android OS
    // init (`0x4fbc6c4`) -> `0x4fc0218`.
    //
    // **What `checkInit` is, from `classes2.dex`**: `sget-object gContext; if-eqz -> false;
    // true`. `gContext` is written in exactly one place, `FMOD.init(Context)`'s first
    // instruction (`sput-object v2, gContext`), and `<clinit>` does not touch it. The app calls
    // `FMOD.init` **unconditionally** from `NativeHelper.Q` at `0x0023` -- with
    // `NativeHelper.a`, the `MainGameActivity` -- and again from `fi.e.E` at `0x003f`, right
    // before `nativeGameGlobalInit`. Both are methods the scripted startup already emulates
    // (step 11's rows name `NativeHelper.Q`; row 22 names `fi.e.E`). So on a device the answer
    // is `true` **because a step ran**, and this layer answers it the same way: `gContext` is
    // [`Answer::Assigned`], written by the step-11 statement `script::FMOD_INIT`, and
    // `checkInit` reads it. Before that step it answers `false`, as the Java would.
    //
    // **What FMOD does with the answer, decoded.** At `0x4fc0218`, `true` registers FMOD's
    // `file:///android_asset/` reader and then `dlopen("libandroid.so", RTLD_LAZY)`
    // (`0x4fc02cc`); only if that succeeds does it `dlsym` six `AAsset*` symbols and call
    // `getAssetManager()` (`0x4fc0364`-`0x4fc0398`). This layer's `dlopen` answers NULL for a
    // library outside the guest's own `DT_VERNEED` (`bionic::dl`), so the function returns
    // `FMOD_ERR_FILE_NOTFOUND` (`0x12`) -- a result its caller `0x4fbc6f8` **discards** -- and
    // `getAssetManager` is not reached. It stays unanswered.
    //
    // Then `System::init` -> `SystemI::init` (`0x4f62930`) picks an output with
    // `FMOD_OS_Output_GetDefault` (`0x4fbc4fc`), which reads four FMOD override ints Roblox fills
    // from FFlags (`0x2f11294` -> `0x4f3f148`, table `0x66acea8` -> `0x6cd5dc8 + 4*i`):
    // index 0 `DebugFmodUseAndroidAudioTrack` and index 1 `DebugFmodUseAndroidOpenSl`, both
    // zero-initialised `.bss` with no static writer and absent from the empty settings document.
    // With both clear it goes straight to `supportsAAudio()` (`0x4fbc630`) and, when that is
    // true, returns output type `0x14`, whose plugin is "FMOD AAudio Output" (its description's
    // type word at `+0xb8`, `0x21b588`). `supportsLowLatency` is called only on the paths those
    // two flags or a `false` from `supportsAAudio` open, so it stays unanswered.
    //
    // **The AAudio output needs `libaaudio.so`, and this runtime supplies none.** Its driver-info
    // and init functions both start with `dlopen("libaaudio.so")` (`0x4fbf3dc`) and answer
    // `FMOD_ERR_OUTPUT_INIT` (`0x33`) when it is NULL, before any other JNI call. `SystemI::init`
    // returns that, and **Roblox is written for it**: at `0x2f04a90` `FmodManager` records
    // `FmodInitError-<reason>`, calls `System::setOutput(FMOD_OUTPUTTYPE_NOSOUND)` (`0x2f04aa4`,
    // type 2) and initialises again. That is the NOSOUND fallback a device without the library
    // would get, and it is the honest one here: there is no audio output behind this layer.
    ClassSpec {
        name: "org/fmod/FMOD",
        tier: Tier::Support,
        methods: &[
            s("checkInit", "()Z", Answer::StaticIsSet("gContext")),
            // Its whole body compares `SDK_INT` with 27. The SDK this host presents is
            // `script::ANDROID_SDK_INT`, the one figure every other `SDK_INT` answer reads, so
            // the answer is computed from it rather than written down beside it. What makes the
            // engine silent afterwards is `dlopen("libaaudio.so")` answering NULL -- a fact about
            // this runtime -- not a `false` here, which would be a claim about Android 13.
            s(
                "supportsAAudio",
                "()Z",
                Answer::Bool(super::script::ANDROID_SDK_LEVEL >= FMOD_AAUDIO_MIN_SDK),
            ),
        ],
        fields: &[sf("gContext", "Landroid/content/Context;", Answer::Assigned)],
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
    // ---- what Djinni's proxy cache asks of the JVM ----------------------------------------
    //
    // **MEASURED, M6's gate**: with `PlatformSystemDialogHandler.INSTANCE` answered, the same
    // thread reached Djinni's `JavaProxyCache` (`0x22592ec`), whose `jniFindClass` asked for
    // `java/lang/System` (recorded as `MISS FindClass java/lang/System`), got null, and went down
    // Djinni's "FindClass returned null" assertion (`0x22590dc`) into a `ThrowNew` on a null
    // class, which killed it. The cache is keyed on `System.identityHashCode` and compared with
    // `IsSameObject` -- Djinni's `JavaIdentityHash`/`JavaIdentityEquals`.
    ClassSpec {
        name: "java/lang/System",
        tier: Tier::Support,
        methods: &[s("identityHashCode", "(Ljava/lang/Object;)I", Answer::IdentityHash)],
        fields: NONE,
    },
    // ---- §8 row 23: what `nativeAppBridgeV2StartAppWithParams` reads ----------------------
    //
    // **Three accessors, decoded, and no more**: the native at `0x258b144` calls `surface()`,
    // `platformParams()` and `vrContext()` through its accessor helper (`0x2335e34`: GetObjectClass,
    // GetMethodID, Call) and nothing else -- `appStarterPlace` and its six siblings are not even
    // strings in `libroblox.so`. The Java side (`fi.e.F`, from the record `fi.o.b` fills) sets
    // them; the engine does not read them here, so they are left to the generated surface's
    // Unanswered rather than given values nothing consumes.
    //
    // `surface()` and `platformParams()` answer what the host stored in the instance -- the same
    // `Surface` the window came from, which is the point -- and `vrContext()` is `null`, which is
    // what `fi.e.F` passes on a device that is not VR (`bh.x0.D0()` false skips `setVrContext`).
    ClassSpec {
        name: "com/roblox/engine/jni/autovalue/StartAppParams",
        tier: Tier::One,
        methods: &[
            m("surface", "()Landroid/view/Surface;", Answer::Field("surface")),
            m(
                "platformParams",
                "()Lcom/roblox/engine/jni/model/PlatformParams;",
                Answer::Field("platformParams"),
            ),
            m("vrContext", "()Landroid/app/Activity;", Answer::Null),
        ],
        fields: &[
            f("surface", "Landroid/view/Surface;", Answer::Null),
            f("platformParams", "Lcom/roblox/engine/jni/model/PlatformParams;", Answer::Null),
        ],
    },
    // ---- the platform dialog handler the settings success path registers -----------------
    //
    // **MEASURED, M6's gate, 2 of 2 runs**: the thread carrying the client-settings fetch logged
    // `getFlags: success`, then read this field through `GetStaticFieldID`/`GetStaticObjectField`
    // at link `0x2258b3c`-`0x2258b74` -- called from `0x2bd58a4`, *before*
    // `continueAfterFlagsLoaded_` at `0x2bd59f8` -- and died on the refusal. So the byte the
    // surface path is gated on was never written, and every window the gate delivered was
    // dropped with `Flags-Not-Received`.
    //
    // What the engine does with it, decoded from `0x2258b78` on: `NewGlobalRef`, then
    // `GetObjectClass` and `IsSameObject` against a cached class -- Djinni asking whether this is
    // one of *its* C++ proxies, whose `nativeRef` it would unwrap with `GetLongField` -- and, when
    // it is not, `0x22592ec`: Djinni's `JavaProxyCache`, wrapping a Java implementation.
    //
    // What the field holds, read out of the APK rather than chosen: `classes2.dex`'s `<clinit>`
    // for this class is `new-instance v0, PlatformSystemDialogHandler; invoke-direct <init>()V;
    // sput-object v0, INSTANCE` -- a Kotlin `object`. Only `INSTANCE` is declared here; the
    // other statics `<clinit>` sets (a coroutine scope, a mutex, two `AtomicReference`s, a
    // queue) have no measured reader, and the generated surface keeps every method Unanswered.
    ClassSpec {
        name: "com/roblox/protocols/systemdialog/PlatformSystemDialogHandler",
        tier: Tier::Support,
        methods: NONE,
        fields: &[sf(
            "INSTANCE",
            "Lcom/roblox/protocols/systemdialog/PlatformSystemDialogHandler;",
            Answer::StaticInstance,
        )],
    },
    // **MEASURED, M6's gate**: once the engine was on Vulkan, a worker died on
    // `CallStaticBooleanMethodV` of `isSystemThemeAvailable()Z`. In `classes2.dex` it is
    // `Build.VERSION.SDK_INT >= 29` and nothing else, so on the SDK this host presents
    // (`script::ANDROID_SDK_INT`, 33) it is true. `getSystemTheme()` beside it reads the night-mode
    // bits of the Context's `Configuration.uiMode` -- a fact about the host's theme -- and stays
    // unanswered until a run reaches it and a host seam supplies it.
    ClassSpec {
        name: "com/roblox/universalapp/systemtheme/SystemThemeProtocol",
        tier: Tier::Support,
        methods: &[s("isSystemThemeAvailable", "()Z", Answer::Bool(true))],
        fields: NONE,
    },
    // **MEASURED, M6's gate**: once the Lua app's renderer was being created, a worker died on
    // `CallStaticBooleanMethodV` of `hevcHardwareEncodingSupported(III)Z`. In `classes2.dex` it is
    // a walk of `new MediaCodecList(REGULAR_CODECS).getCodecInfos()` for a hardware `video/hevc`
    // encoder that supports the size and rate asked -- and `getVideoCodecs()` is the same list,
    // described. This runtime gives the app **no** `MediaCodec`: there is no Android media stack
    // behind it, so the list the app would walk is empty, and "no such encoder" and "no codecs"
    // are the true answers rather than chosen ones. If a media stack is ever provided, these two
    // must be answered from it.
    ClassSpec {
        name: "com/roblox/engine/jni/video/MediaCodecInfoUtils",
        tier: Tier::Support,
        methods: &[
            s("hevcHardwareEncodingSupported", "(III)Z", Answer::Bool(false)),
            s(
                "getVideoCodecs",
                "()[Lcom/roblox/engine/jni/video/VideoCodecCapability;",
                Answer::EmptyObjectArray,
            ),
        ],
        fields: NONE,
    },
    // **MEASURED, M6's gate**: once the engine settings reached a live engine and `initEngine_`
    // ran, the game thread died on `GetStaticObjectField` of this `INSTANCE`. The same shape as
    // `PlatformSystemDialogHandler` above, read from `classes2.dex`: `<clinit>` is `new-instance;
    // invoke-direct <init>()V; sput-object INSTANCE` -- a Kotlin `object`.
    //
    // `setListener(J)V` is `Long.valueOf` then `sput-object nativeListenerPtr` and nothing else;
    // the only readers of that static are this class's own Java methods (`startInquiry`,
    // `onComplete`, `onError`, `onCancel`), which this runtime does not execute. So the host's
    // duty is to observe it: a sink. `startInquiry` is the start of a facial-age-estimation flow
    // through a third-party SDK and stays unanswered -- refused by name if it is ever reached.
    ClassSpec {
        name: "com/roblox/universalapp/facialageestimation/FacialAgeEstimationProtocol",
        tier: Tier::Support,
        methods: &[m("setListener", "(J)V", Answer::Sink)],
        fields: &[sf(
            "INSTANCE",
            "Lcom/roblox/universalapp/facialageestimation/FacialAgeEstimationProtocol;",
            Answer::StaticInstance,
        )],
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
        assert_eq!(registry.member(method).expect("declared").answer, Answer::Long(-1));
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

    /// Every [`EXTENDS`] edge resolves, and the chain has no cycle.
    ///
    /// An edge naming a class nobody declares is dropped silently, which is right for a host that
    /// narrowed the surface and wrong for a typo in **this** table — so the table's own ends are
    /// checked here rather than at run time. The acyclicity assertion is what makes
    /// `Registry::ancestry`'s budget a belt rather than the only brace.
    #[test]
    fn every_superclass_edge_resolves_and_the_chain_is_acyclic() {
        let registry = Registry::with_declared();
        for (subclass, superclass) in EXTENDS {
            let sub = registry.find(subclass).unwrap_or_else(|| panic!("{subclass} is not declared"));
            let sup =
                registry.find(superclass).unwrap_or_else(|| panic!("{superclass} is not declared"));
            assert_ne!(sub, sup, "{subclass} cannot extend itself");
            assert_eq!(
                registry.class(sub).expect("declared").superclass,
                Some(sup),
                "{subclass} did not get linked to {superclass}"
            );
        }
        for id in 0..registry.class_count() {
            let start = ClassId(u16::try_from(id).expect("the id encoding holds it"));
            let mut seen = BTreeSet::new();
            for at in registry.ancestry(start) {
                assert!(seen.insert(at), "`{}` is on a cycle", registry.class_name(at));
            }
        }
    }

    /// `method` walks the chain and `declared_method` does not, which is the distinction
    /// `RegisterNatives` and the generated-surface merge both rest on.
    ///
    /// Without it, a `RegisterNatives` naming an inherited member would bind a guest function
    /// pointer onto the **superclass's** member, where every other subclass would then call it.
    #[test]
    fn only_the_walking_lookup_crosses_a_superclass_edge() {
        let registry = Registry::with_declared();
        let activity = registry.find("com/roblox/client/startup/MainGameActivity").expect("declared");
        let descriptor = "()Landroid/content/res/Resources;";
        assert!(
            registry.method(activity, "getResources", descriptor, false).is_some(),
            "`method` must walk to `android/content/Context`"
        );
        assert!(
            registry.declared_method(activity, "getResources", descriptor, false).is_none(),
            "`declared_method` must not walk"
        );
        // And the class that really declares it answers both ways.
        let context = registry.find("android/content/Context").expect("declared");
        assert!(registry.declared_method(context, "getResources", descriptor, false).is_some());
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

    /// **`android.os.Build` is found, as on every device, and has no `isDebuggerConnected`** -- so
    /// the engine's lookup of that method on it answers null with `NoSuchMethodError`, which is
    /// what a device answers -- and its `MANUFACTURER` refuses until an embedding states it.
    #[test]
    fn build_is_found_without_debuggers_method_and_its_maker_is_the_embeddings() {
        let registry = Registry::with_declared();
        let id = registry.find("android/os/Build").expect("declared, so FindClass answers it");
        let build = registry.class(id).expect("just found");
        assert!(
            !build.methods.iter().any(|m| m.name == "isDebuggerConnected"),
            "Debug's method, not Build's"
        );
        let maker = build
            .fields
            .iter()
            .find(|f| f.name == "MANUFACTURER" && f.descriptor == "Ljava/lang/String;")
            .expect("MANUFACTURER is declared");
        assert!(maker.is_static);
        assert_eq!(maker.answer, Answer::Unanswered, "the embedding's to state");
    }

    /// **`Build.VERSION.SDK_INT` is the one SDK level** every other answer uses.
    #[test]
    fn build_version_sdk_int_is_the_one_sdk_level() {
        let registry = Registry::with_declared();
        let id = registry.find("android/os/Build$VERSION").expect("declared, so FindClass finds it");
        let sdk = registry
            .class(id)
            .expect("just found")
            .fields
            .iter()
            .find(|f| f.name == "SDK_INT" && f.descriptor == "I")
            .expect("SDK_INT is declared");
        assert!(sdk.is_static);
        assert_eq!(sdk.answer, Answer::Int(super::super::script::ANDROID_SDK_LEVEL));
        assert_eq!(sdk.answer, Answer::Int(33), "Android 13");
    }

    /// **`android.os.Debug.isDebuggerConnected()` is `false`**, declared on its class so that
    /// `FindClass` finds it: there is no JDWP agent in this runtime to be connected to.
    #[test]
    fn no_java_debugger_is_connected_to_a_runtime_with_no_jdwp() {
        let registry = Registry::with_declared();
        let id = registry.find("android/os/Debug").expect("declared, so FindClass answers it");
        let method = registry
            .class(id)
            .expect("just found")
            .methods
            .iter()
            .find(|m| m.name == "isDebuggerConnected" && m.descriptor == "()Z")
            .expect("isDebuggerConnected()Z is declared");
        assert!(method.is_static, "a static method, as GetStaticMethodID asks for it");
        assert_eq!(method.answer, Answer::Bool(false));
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
        // Both are answers **from state**, and a constant for either would be the plausible
        // wrong one: `checkInit` true whether or not `FMOD.init` ran.
        assert!(Registry::simple_answer(Answer::Assigned).is_none());
        assert!(Registry::simple_answer(Answer::StaticIsSet("gContext")).is_none());
        assert_eq!(Registry::simple_answer(Answer::Sink), Some(Value::Void));
        assert_eq!(Registry::simple_answer(Answer::Int(7)), Some(Value::Int(7)));
    }

    /// **FMOD's Android statics: two decided, the rest refusing by name.**
    ///
    /// `supportsAAudio` is the dex's `SDK_INT >= 27` evaluated at the SDK this host presents, 33,
    /// so it is `true` -- a `false` "because this host has no `libaaudio.so`" would be a false
    /// statement about Android 13, and the absence is answered where it is a fact, by `dlopen`.
    /// `checkInit` reads `gContext`, which is Java-assigned. Every other static is unreached on
    /// the decoded path (see the declaration) and must still refuse: an invented sample rate or
    /// block size here would be FMOD sizing its mixer on a number nothing measured.
    #[test]
    fn fmod_answers_only_what_its_decoded_path_reaches() {
        let registry = Registry::with_declared();
        let id = registry.find("org/fmod/FMOD").expect("declared");
        let answer = |name: &str, descriptor: &str| {
            let method = registry.method(id, name, descriptor, true).expect("declared");
            registry.member(method).expect("a member").answer
        };
        assert_eq!(super::super::script::ANDROID_SDK_LEVEL, 33, "the SDK every SDK_INT answer uses");
        assert_eq!(answer("supportsAAudio", "()Z"), Answer::Bool(true));
        assert_eq!(answer("checkInit", "()Z"), Answer::StaticIsSet("gContext"));
        let field = registry.field(id, "gContext", "Landroid/content/Context;", true).expect("declared");
        assert_eq!(registry.field_member(field).expect("a member").answer, Answer::Assigned);
        for (name, descriptor) in [
            ("supportsLowLatency", "()Z"),
            ("getOutputSampleRate", "()I"),
            ("getOutputBlockSize", "()I"),
            ("getAssetManager", "()Landroid/content/res/AssetManager;"),
            ("lowLatencyFlag", "()Z"),
            ("proAudioFlag", "()Z"),
            ("isBluetoothOn", "()Z"),
            ("close", "()V"),
        ] {
            assert_eq!(answer(name, descriptor), Answer::Unanswered, "FMOD.{name}{descriptor}");
        }
    }

    /// **The host's display is the embedding's to describe**: every `DisplayMetrics` field the
    /// engine reads refuses until one defines it. MEASURED what the zeros they used to answer did:
    /// the renderer divided by the density and sized a texture 0x0 from the infinity.
    #[test]
    fn the_display_metrics_are_unanswered_until_an_embedding_describes_the_display() {
        let registry = Registry::with_declared();
        let id = registry.find("android/util/DisplayMetrics").expect("declared");
        let fields = &registry.class(id).expect("just found").fields;
        for name in ["density", "widthPixels", "heightPixels", "xdpi", "ydpi"] {
            let field = fields
                .iter()
                .find(|member| member.name == name)
                .unwrap_or_else(|| panic!("DisplayMetrics.{name} is declared"));
            assert_eq!(field.answer, Answer::Unanswered, "DisplayMetrics.{name}");
        }
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
