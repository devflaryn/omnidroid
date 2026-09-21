//! Java values, Java strings, and the descriptor grammar — the three things every JNI call is
//! written in.
//!
//! # Why a Java string is UTF-16 here and not a Rust `String`
//!
//! `GetStringLength` is defined as the number of **UTF-16 code units**, `GetStringChars` hands
//! back those units, and `NewString` takes them. A `String` would answer `GetStringLength` in
//! Rust `char`s or in bytes — both of which are *plausible* and both of which are wrong for any
//! string outside the Basic Multilingual Plane, which is the failure shape Global Constraint 1
//! names. So the model is what the ABI is defined over: `Vec<u16>`.
//!
//! # Modified UTF-8 is not UTF-8, and the difference is reachable
//!
//! `NewStringUTF` (84 call sites) and `GetStringUTFChars` (27) are defined over **modified
//! UTF-8**, which differs from UTF-8 in exactly two places:
//!
//! * `U+0000` encodes as the two bytes `C0 80`, never as a zero byte — which is the whole reason
//!   a JNI string can be NUL-terminated at all;
//! * a character outside the BMP encodes as its **two UTF-16 surrogates**, each as a separate
//!   three-byte sequence (six bytes), where UTF-8 uses one four-byte sequence.
//!
//! Treating the two as interchangeable is a silent wrong answer for any emoji the engine passes
//! through, and Roblox passes user-entered text. Both directions are implemented here and
//! `modified_utf8_round_trips_the_two_cases_that_differ_from_utf8` is the test that would fail if
//! somebody replaced either with `str::from_utf8`.

use crate::error::{AbiError, AbiResult};

/// A `java.lang.String`: UTF-16 code units, which is what the JNI string calls are defined over.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JavaString {
    units: Vec<u16>,
}

impl JavaString {
    /// From Rust text.
    #[must_use]
    pub fn from_str(text: &str) -> Self {
        Self { units: text.encode_utf16().collect() }
    }

    /// From raw UTF-16 code units, which is what `NewString` is handed.
    #[must_use]
    pub fn from_units(units: Vec<u16>) -> Self {
        Self { units }
    }

    /// The code units.
    #[must_use]
    pub fn units(&self) -> &[u16] {
        &self.units
    }

    /// `GetStringLength`: the number of UTF-16 code units.
    #[must_use]
    pub fn len(&self) -> usize {
        self.units.len()
    }

    /// Whether it is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// For a message or a host-side comparison. Unpaired surrogates become `U+FFFD`, which is why
    /// this is `_lossy` and why nothing that has to round-trip uses it.
    #[must_use]
    pub fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(&self.units)
    }

    /// Decode modified UTF-8 — what `NewStringUTF` is handed.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] naming `function` for a sequence that is not valid modified
    /// UTF-8. **Not** a replacement character: a string the engine will hand to its own logic is
    /// not somewhere to substitute an answer, and JNI has no way to report a bad encoding, so the
    /// honest outcome is the typed error.
    pub fn from_modified_utf8(
        function: &str,
        address: omni_mem::GuestAddr,
        bytes: &[u8],
    ) -> AbiResult<Self> {
        let refuse = |at: usize, why: &str| AbiError::JniRefused {
            function: function.to_string(),
            address,
            detail: format!("byte {at} of the modified-UTF-8 string is invalid: {why}"),
        };
        let mut units = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                0x00 => {
                    return Err(refuse(i, "a zero byte cannot appear inside a modified-UTF-8 \
                                          string; U+0000 is encoded as C0 80"))
                }
                0x01..=0x7f => {
                    units.push(u16::from(b));
                    i += 1;
                }
                0xc0..=0xdf => {
                    let second = *bytes
                        .get(i + 1)
                        .ok_or_else(|| refuse(i, "a two-byte sequence runs off the end"))?;
                    if second & 0xc0 != 0x80 {
                        return Err(refuse(i + 1, "a continuation byte does not start with 10"));
                    }
                    units.push((u16::from(b & 0x1f) << 6) | u16::from(second & 0x3f));
                    i += 2;
                }
                0xe0..=0xef => {
                    let second = *bytes
                        .get(i + 1)
                        .ok_or_else(|| refuse(i, "a three-byte sequence runs off the end"))?;
                    let third = *bytes
                        .get(i + 2)
                        .ok_or_else(|| refuse(i, "a three-byte sequence runs off the end"))?;
                    if second & 0xc0 != 0x80 || third & 0xc0 != 0x80 {
                        return Err(refuse(i + 1, "a continuation byte does not start with 10"));
                    }
                    units.push(
                        (u16::from(b & 0x0f) << 12)
                            | (u16::from(second & 0x3f) << 6)
                            | u16::from(third & 0x3f),
                    );
                    i += 3;
                }
                _ => {
                    return Err(refuse(
                        i,
                        "modified UTF-8 has no four-byte form and no lead byte in this range",
                    ))
                }
            }
        }
        Ok(Self { units })
    }

    /// Encode as modified UTF-8, **without** a terminator — what `GetStringUTFChars` copies out.
    #[must_use]
    pub fn to_modified_utf8(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.units.len() + 1);
        for unit in &self.units {
            let unit = *unit;
            match unit {
                0x0001..=0x007f => out.push(unit as u8),
                // U+0000 takes the two-byte form, which is what keeps the result NUL-terminatable.
                0x0000 | 0x0080..=0x07ff => {
                    out.push(0xc0 | (unit >> 6) as u8);
                    out.push(0x80 | (unit & 0x3f) as u8);
                }
                _ => {
                    // Every remaining unit, surrogates included, takes the three-byte form. A
                    // surrogate pair therefore becomes six bytes, not four.
                    out.push(0xe0 | (unit >> 12) as u8);
                    out.push(0x80 | ((unit >> 6) & 0x3f) as u8);
                    out.push(0x80 | (unit & 0x3f) as u8);
                }
            }
        }
        out
    }

    /// `GetStringUTFLength`: the length of [`to_modified_utf8`](JavaString::to_modified_utf8).
    #[must_use]
    pub fn modified_utf8_len(&self) -> usize {
        self.units
            .iter()
            .map(|unit| match unit {
                0x0001..=0x007f => 1,
                0x0000 | 0x0080..=0x07ff => 2,
                _ => 3,
            })
            .sum()
    }
}

/// A Java value, as a field holds one or a method returns one.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `void`.
    Void,
    /// `boolean`.
    Boolean(bool),
    /// `byte`.
    Byte(i8),
    /// `char`.
    Char(u16),
    /// `short`.
    Short(i16),
    /// `int`.
    Int(i32),
    /// `long`.
    Long(i64),
    /// `float`.
    Float(f32),
    /// `double`.
    Double(f64),
    /// A reference, as a handle-table id. `None` is Java's `null`.
    Object(Option<super::refs::ObjectId>),
    /// A string the host wants to hand back, turned into an object when the call returns.
    ///
    /// A convenience for host-defined method bodies: nearly every Roblox getter returns a
    /// `String`, and making each one create its own object first would put the handle table in
    /// every declaration.
    Text(String),
}

impl Value {
    /// The descriptor letter this value belongs to, for an error that has to say what did not
    /// match.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Void => "void",
            Value::Boolean(_) => "boolean",
            Value::Byte(_) => "byte",
            Value::Char(_) => "char",
            Value::Short(_) => "short",
            Value::Int(_) => "int",
            Value::Long(_) => "long",
            Value::Float(_) => "float",
            Value::Double(_) => "double",
            Value::Object(_) | Value::Text(_) => "a reference",
        }
    }
}

/// One parameter or return type of a descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTag {
    /// `V`.
    Void,
    /// `Z`.
    Boolean,
    /// `B`.
    Byte,
    /// `C`.
    Char,
    /// `S`.
    Short,
    /// `I`.
    Int,
    /// `J`.
    Long,
    /// `F`.
    Float,
    /// `D`.
    Double,
    /// `L…;` or `[…`.
    Object,
}

impl TypeTag {
    /// Whether a variadic argument of this type is read out of the floating-point save area.
    ///
    /// **The variadic promotion, which is where a `va_list` walk goes silently wrong.** A `float`
    /// argument to a variadic function is promoted to `double`, so `CallVoidMethodV` reading a
    /// `float` parameter must take a `double` out of the SIMD save area and narrow it — reading
    /// four bytes gets `0.0`, which is [`crate::varargs`]'s own recorded failure one level down.
    #[must_use]
    pub fn is_floating(self) -> bool {
        matches!(self, TypeTag::Float | TypeTag::Double)
    }
}

/// A parsed JNI method descriptor: `(ILjava/lang/String;)V` and the like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    parameters: Vec<TypeTag>,
    returns: TypeTag,
}

impl Descriptor {
    /// Parse a method descriptor.
    ///
    /// # Errors
    ///
    /// [`AbiError::JniRefused`] naming `function` and the descriptor, for anything that is not one.
    /// A descriptor arrives as a guest string, so this is untrusted input.
    pub fn parse(
        function: &str,
        address: omni_mem::GuestAddr,
        descriptor: &str,
    ) -> AbiResult<Self> {
        let refuse = |why: &str| AbiError::JniRefused {
            function: function.to_string(),
            address,
            detail: format!("`{descriptor}` is not a method descriptor: {why}"),
        };
        let bytes = descriptor.as_bytes();
        if bytes.first() != Some(&b'(') {
            return Err(refuse("it does not start with `(`"));
        }
        let mut parameters = Vec::new();
        let mut i = 1;
        loop {
            match bytes.get(i) {
                None => return Err(refuse("it has no closing `)`")),
                Some(b')') => {
                    i += 1;
                    break;
                }
                Some(_) => {
                    let (tag, next) = Self::one(&refuse, bytes, i)?;
                    if tag == TypeTag::Void {
                        return Err(refuse("`V` is not a parameter type"));
                    }
                    parameters.push(tag);
                    i = next;
                }
            }
        }
        let (returns, next) = Self::one(&refuse, bytes, i)?;
        if next != bytes.len() {
            return Err(refuse("there is more after the return type"));
        }
        Ok(Self { parameters, returns })
    }

    fn one(
        refuse: &impl Fn(&str) -> AbiError,
        bytes: &[u8],
        at: usize,
    ) -> AbiResult<(TypeTag, usize)> {
        match bytes.get(at) {
            None => Err(refuse("a type runs off the end")),
            Some(b'V') => Ok((TypeTag::Void, at + 1)),
            Some(b'Z') => Ok((TypeTag::Boolean, at + 1)),
            Some(b'B') => Ok((TypeTag::Byte, at + 1)),
            Some(b'C') => Ok((TypeTag::Char, at + 1)),
            Some(b'S') => Ok((TypeTag::Short, at + 1)),
            Some(b'I') => Ok((TypeTag::Int, at + 1)),
            Some(b'J') => Ok((TypeTag::Long, at + 1)),
            Some(b'F') => Ok((TypeTag::Float, at + 1)),
            Some(b'D') => Ok((TypeTag::Double, at + 1)),
            Some(b'[') => {
                // An array of anything is a reference; the element type still has to parse, so
                // that `[Q` is refused rather than silently accepted as "some array".
                let (_, next) = Self::one(refuse, bytes, at + 1)?;
                Ok((TypeTag::Object, next))
            }
            Some(b'L') => {
                let end = bytes[at..]
                    .iter()
                    .position(|b| *b == b';')
                    .ok_or_else(|| refuse("a class type has no closing `;`"))?;
                if end == 1 {
                    return Err(refuse("a class type has an empty name"));
                }
                Ok((TypeTag::Object, at + end + 1))
            }
            Some(other) => Err(refuse(&format!("`{}` is not a type letter", *other as char))),
        }
    }

    /// The parameter types, in order.
    #[must_use]
    pub fn parameters(&self) -> &[TypeTag] {
        &self.parameters
    }

    /// The return type.
    #[must_use]
    pub fn returns(&self) -> TypeTag {
        self.returns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two places modified UTF-8 differs from UTF-8, both round-tripped. Replacing either
    /// direction with `str::from_utf8`/`as_bytes` fails this and nothing else.
    #[test]
    fn modified_utf8_round_trips_the_two_cases_that_differ_from_utf8() {
        // U+0000 is `C0 80`, which is what lets the result be NUL-terminated.
        let nul = JavaString::from_units(vec![0x0000]);
        assert_eq!(nul.to_modified_utf8(), vec![0xc0, 0x80]);
        assert_eq!(nul.modified_utf8_len(), 2);
        assert_eq!(
            JavaString::from_modified_utf8("NewStringUTF", 0, &[0xc0, 0x80]).expect("decodes"),
            nul
        );

        // U+1F600 is one four-byte sequence in UTF-8 and two three-byte sequences here.
        let emoji = JavaString::from_str("\u{1f600}");
        assert_eq!(emoji.len(), 2, "one supplementary character is two UTF-16 units");
        let bytes = emoji.to_modified_utf8();
        assert_eq!(bytes.len(), 6, "six bytes, not the four UTF-8 would use");
        assert_eq!("\u{1f600}".as_bytes().len(), 4, "which is what UTF-8 does with it");
        assert_eq!(
            JavaString::from_modified_utf8("NewStringUTF", 0, &bytes).expect("decodes"),
            emoji
        );
    }

    #[test]
    fn ordinary_text_round_trips_and_lengths_agree() {
        let text = JavaString::from_str("2.738.1397");
        assert_eq!(text.len(), 10);
        assert_eq!(text.modified_utf8_len(), 10);
        assert_eq!(text.to_modified_utf8(), b"2.738.1397");
        assert_eq!(text.to_string_lossy(), "2.738.1397");
    }

    /// Hostile input: a zero byte inside the string, a truncated sequence, a bad continuation and
    /// a four-byte lead. Each names the function rather than substituting `U+FFFD`.
    #[test]
    fn invalid_modified_utf8_is_refused_by_name_rather_than_replaced() {
        for bytes in [
            &[b'a', 0x00, b'b'][..],
            &[0xe0, 0x80][..],
            &[0xc0, 0x41][..],
            &[0xf0, 0x9f, 0x98, 0x80][..],
        ] {
            let error =
                JavaString::from_modified_utf8("NewStringUTF", 0x1234, bytes).expect_err("invalid");
            match error {
                AbiError::JniRefused { function, address, .. } => {
                    assert_eq!(function, "NewStringUTF");
                    assert_eq!(address, 0x1234);
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn descriptors_parse_into_their_parameters_and_return() {
        let d = Descriptor::parse("GetMethodID", 0, "(Ljava/lang/String;ZZ)V").expect("parses");
        assert_eq!(d.parameters(), &[TypeTag::Object, TypeTag::Boolean, TypeTag::Boolean]);
        assert_eq!(d.returns(), TypeTag::Void);

        let d = Descriptor::parse("GetStaticMethodID", 0, "()J").expect("parses");
        assert!(d.parameters().is_empty());
        assert_eq!(d.returns(), TypeTag::Long);

        // `showKeyboard`, which is the widest shape on the startup path.
        let d = Descriptor::parse(
            "GetMethodID",
            0,
            "(JZ[BLcom/roblox/engine/jni/model/NativeTextBoxInfo;)V",
        )
        .expect("parses");
        assert_eq!(
            d.parameters(),
            &[TypeTag::Long, TypeTag::Boolean, TypeTag::Object, TypeTag::Object]
        );

        let d = Descriptor::parse("GetMethodID", 0, "([I[FIIII)V").expect("parses");
        assert_eq!(
            d.parameters(),
            &[
                TypeTag::Object,
                TypeTag::Object,
                TypeTag::Int,
                TypeTag::Int,
                TypeTag::Int,
                TypeTag::Int
            ]
        );
    }

    /// A descriptor is a guest string. Every one of these is refused by name rather than parsed
    /// into something plausible.
    #[test]
    fn a_malformed_descriptor_is_refused_by_name() {
        for bad in ["", "()", "(", "V)V", "(I)", "(Q)V", "(L;)V", "(Ljava/lang/String)V", "(V)V", "()VV", "([)V"] {
            let error = Descriptor::parse("GetMethodID", 0x99, bad).expect_err(bad);
            match error {
                AbiError::JniRefused { function, detail, .. } => {
                    assert_eq!(function, "GetMethodID");
                    assert!(detail.contains(bad) || bad.is_empty(), "{detail}");
                }
                other => panic!("{bad}: {other:?}"),
            }
        }
    }

    /// The variadic promotion, asserted at the level that decides it. `float` is floating, so a
    /// `CallVoidMethodV` walking a `va_list` takes it out of the SIMD save area as a `double`.
    #[test]
    fn float_and_double_are_both_floating_and_nothing_else_is() {
        assert!(TypeTag::Float.is_floating());
        assert!(TypeTag::Double.is_floating());
        for tag in [
            TypeTag::Void,
            TypeTag::Boolean,
            TypeTag::Byte,
            TypeTag::Char,
            TypeTag::Short,
            TypeTag::Int,
            TypeTag::Long,
            TypeTag::Object,
        ] {
            assert!(!tag.is_floating(), "{tag:?}");
        }
    }
}
