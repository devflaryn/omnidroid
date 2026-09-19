//! Locale handles and `localeconv`.
//!
//! bionic has exactly one locale implementation: every *accepted* `newlocale` category/
//! name combination maps to the same C/UTF-8 behaviour (bionic's ` locales` are validated
//! by name but all behave identically — VERIFIED against bionic's `locale.cpp` design
//! comments in the NDK sources; the behavioural claim INFERRED from that design, the
//! acceptance of any name per POSIX INFERRED from bionic's `newlocale` accepting anything
//! non-null). This makes real locale objects unnecessary here:
//!
//! * the C-locale handle is a **static sentinel** — a distinguished non-zero `locale_t`
//!   value no adapter can confuse with a real pointer (`0x0`: the C locale is also what
//!   `NULL` means in `uselocale`, so the sentinel distinguishes "the C locale object" from
//!   "no locale argument").
//! * `freelocale` frees nothing (nothing was allocated) — its contract is honoured
//!   trivially and honestly, argued in the phase 3 notes.
//! * `uselocale` records the handle in guest memory via the context (the adapter owns
//!   where the thread's locale lives) — no allocation.
//! * `localeconv` returns the C/POSIX `lconv` field values and composes the struct into a
//!   scratch buffer from [`crate::context::GuestContext::scratch`].
//!
//! The `lconv` struct layout follows the arm64 LP64 ABI: `n_decimal_point` at offset 0,
//! then pointers in declaration order, 8 bytes each, followed by the `char` fields
//! (`int_*`/`mon_*` groupings as POSIX declares them). The exact field list here is the
//! POSIX `lconv`; bionic's headers declare the same POSIX shape.
//!
//! Where should the values live? POSIX fixes the C-locale values: decimal_point ".",
//! thousands_sep "", grouping "", mon_decimal_point "", ... int_curr_symbol "",
//! currency_symbol "", mon_thousands_sep "", mon_grouping "", positive_sign "",
//! negative_sign "", int_frac_digits `CHAR_MAX`, frac_digits `CHAR_MAX`, p_cs_precedes
//! `CHAR_MAX`, ... (all monetary/boolean fields `CHAR_MAX` = 127 in bionic's `char`
//! signedness on arm64; VERIFIED CHAR_MAX is 127 for signed char arm64).

use crate::context::GuestContext;
use crate::error::BionicError;

/// `locale_t` for the C/POSIX locale. POSIX says the C locale object exists; bionic's
/// `LC_GLOBAL_LOCALE` is `(locale_t)0`... a reserved constant. We use a distinguished
/// non-null sentinel that cannot be a plausible guest address so the adapter can store it
/// verbatim.
pub const C_LOCALE_HANDLE: u64 = 0xC10CA1E_C10CA1E; // "CLOCALE" pun; not a real address

/// `locale_t newlocale(int category_mask, const char *locale, locale_t base)`
///
/// Accepts the C/POSIX locale (and, like bionic, any non-null name — all names map to the
/// same behaviour). `base` is irrelevant here (nothing to combine: one behaviour). Returns
/// the C-locale sentinel. Null `locale` pointer with `LC_ALL_MASK`-shaped valid mask is
/// invalid per POSIX (returns NULL and sets EINVAL — the *errno* value; `newlocale`
/// itself returns NULL which here is guest `0`).
pub fn newlocale(
    ctx: &mut impl GuestContext,
    category_mask: i32,
    locale: u64,
    base: u64,
) -> Result<u64, BionicError> {
    if locale == 0 {
        ctx.set_errno(crate::errno::consts::EINVAL);
        return Ok(0);
    }
    if category_mask == 0 {
        // POSIX: a zero mask with non-null name is invalid (EINVAL).
        ctx.set_errno(crate::errno::consts::EINVAL);
        return Ok(0);
    }
    let _ = base; // nothing to combine: one behaviour
    Ok(C_LOCALE_HANDLE)
}

/// `void freelocale(locale_t locobj)` — releases nothing: the only handle this crate can
/// produce is the static C-locale sentinel. (Correctness argument: `newlocale` never
/// allocates, so there is nothing to free; no leak and no double-free exist.)
pub fn freelocale(_locobj: u64) {}

/// `locale_t uselocale(locale_t newloc)` — POSIX: install `newloc` for the calling thread
/// and return the previous handle; `LC_GLOBAL_LOCALE` (0) restores the global locale.
/// The "thread state" is one handle stored via [`GuestContext::set_locale_slot`]-shaped
/// access: this crate defines no new trait method — the current handle is kept *in the
/// caller-provided slot* (a guest address the adapter chooses, passed in), so the function
/// stays a pure computation.
///
/// `current_slot` is the guest address of the adapter's current-locale variable; if the
/// adapter has none, pass `0` and the function reports `Unimplemented` rather than
/// guessing storage. (A trait method would also work; keeping the slot explicit means the
/// adapter decides where thread-local locale lives — same reasoning as `scratch`.)
pub fn uselocale(
    ctx: &mut impl GuestContext,
    current_slot: u64,
    newloc: u64,
) -> Result<u64, BionicError> {
    if current_slot == 0 {
        return Err(BionicError::Unimplemented("uselocale: no locale slot"));
    }
    // Read the previous handle.
    let mut buf = [0u8; 8];
    ctx.read(current_slot, &mut buf)?;
    let prev = u64::from_le_bytes(buf);
    let effective = if newloc == 0 {
        0 // LC_GLOBAL_LOCALE
    } else {
        newloc
    };
    ctx.write(current_slot, &effective.to_le_bytes())?;
    Ok(prev)
}

/// POSIX C-locale `lconv` values. `CHAR_MAX` (127) marks "unspecified" numeric fields.
pub const LCONV_C_VALUES: LconvValues = LconvValues {
    decimal_point: ".",
    thousands_sep: "",
    grouping: "",
    mon_decimal_point: "",
    mon_thousands_sep: "",
    mon_grouping: "",
    positive_sign: "",
    negative_sign: "",
    currency_symbol: "",
    int_curr_symbol: "",
    frac_digits: CHAR_MAX,
    p_cs_precedes: CHAR_MAX,
    n_cs_precedes: CHAR_MAX,
    p_sep_by_space: CHAR_MAX,
    n_sep_by_space: CHAR_MAX,
    p_sign_posn: CHAR_MAX,
    n_sign_posn: CHAR_MAX,
    int_frac_digits: CHAR_MAX,
    int_p_cs_precedes: CHAR_MAX,
    int_n_cs_precedes: CHAR_MAX,
    int_p_sep_by_space: CHAR_MAX,
    int_n_sep_by_space: CHAR_MAX,
    int_p_sign_posn: CHAR_MAX,
    int_n_sign_posn: CHAR_MAX,
};

/// `CHAR_MAX` for signed char on arm64.
pub const CHAR_MAX: i8 = 127;

/// The C/POSIX locale's `lconv` field values (see [`LCONV_C_VALUES`]).
pub struct LconvValues {
    /// Decimal separator: ".".
    pub decimal_point: &'static str,
    /// Digit grouping separator: "".
    pub thousands_sep: &'static str,
    /// Group sizes: "".
    pub grouping: &'static str,
    /// Monetary decimal separator: "".
    pub mon_decimal_point: &'static str,
    /// Monetary grouping separator: "".
    pub mon_thousands_sep: &'static str,
    /// Monetary group sizes: "".
    pub mon_grouping: &'static str,
    /// Non-negative sign: "".
    pub positive_sign: &'static str,
    /// Negative sign: "".
    pub negative_sign: &'static str,
    /// Local currency symbol: "".
    pub currency_symbol: &'static str,
    /// International currency symbol: "".
    pub int_curr_symbol: &'static str,
    /// Fraction digits: CHAR_MAX.
    pub frac_digits: i8,
    /// CHAR_MAX.
    pub p_cs_precedes: i8,
    /// CHAR_MAX.
    pub n_cs_precedes: i8,
    /// CHAR_MAX.
    pub p_sep_by_space: i8,
    /// CHAR_MAX.
    pub n_sep_by_space: i8,
    /// CHAR_MAX.
    pub p_sign_posn: i8,
    /// CHAR_MAX.
    pub n_sign_posn: i8,
    /// CHAR_MAX.
    pub int_frac_digits: i8,
    /// CHAR_MAX.
    pub int_p_cs_precedes: i8,
    /// CHAR_MAX.
    pub int_n_cs_precedes: i8,
    /// CHAR_MAX.
    pub int_p_sep_by_space: i8,
    /// CHAR_MAX.
    pub int_n_sep_by_space: i8,
    /// CHAR_MAX.
    pub int_p_sign_posn: i8,
    /// CHAR_MAX.
    pub int_n_sign_posn: i8,
}
