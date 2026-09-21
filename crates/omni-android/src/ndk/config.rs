//! `AConfiguration`: the nine symbols the game thread reads before `android_main`.
//!
//! `AConfiguration_new`, `_delete`, `_fromAssetManager`, `_getLanguage`, `_getCountry`,
//! `_getNavHidden`, `_getScreenWidthDp`, `_getScreenHeightDp`, `_getScreenSize`.
//!
//! `jni-surface.md` §5.2: `android_app_entry` — the glue's game-thread entry point, which runs
//! *inside* step 13 because `GameActivity_onCreate` blocks until it signals — does
//! `AConfiguration_new` → `AConfiguration_fromAssetManager(config, activity->assetManager)` →
//! `AConfiguration_getLanguage` / `_getCountry` before it touches anything else.
//!
//! # Every value here is the embedding's, and there is no default
//!
//! The same shape as `Bionic::set_hwcap_policy` (D26) and `LoggingProtocol.getProcessTimestamp`
//! (D28): the language, the country, the screen and the density are facts about the *device*, and
//! a number this layer picked would be a number with nothing behind it. An instance whose
//! embedding has not decided refuses `AConfiguration_fromAssetManager` **by name**, and the
//! refusal says which call decides.
//!
//! That is not pedantry about one struct. `screenWidthDp` and `densityDpi` reach the engine's
//! layout and its texture budget, and a plausible 1080p phone invented here would be a device
//! profile nobody chose, indistinguishable in every log from one the host meant.
//!
//! # Why `fromAssetManager` takes an asset manager it does not read
//!
//! On a device the configuration comes out of the `AssetManager`'s resource table, because that
//! is where the selected locale and density were resolved. There is no resource table here — the
//! engine's assets are raw files, and §3.1's `Configuration` object is the Java-side answer to the
//! same question — so what the argument does here is **identify the instance and get checked**.
//! Passing something that is not an `AAssetManager` is refused rather than ignored, because a
//! caller that passed the wrong pointer is a caller whose configuration would silently be the
//! right one.

use omni_mem::GuestAddr;

use crate::abi::Args;
use crate::boundary::{ImportCall, ImportFn};
use crate::error::{AbiError, AbiResult};
use crate::mem::Blame;

use super::{active, Ndk};

// ================================================================== the NDK's own constants
//
// `android/configuration.h`.

/// `ACONFIGURATION_SCREENSIZE_ANY`.
pub const ACONFIGURATION_SCREENSIZE_ANY: i32 = 0x00;
/// `ACONFIGURATION_SCREENSIZE_SMALL`.
pub const ACONFIGURATION_SCREENSIZE_SMALL: i32 = 0x01;
/// `ACONFIGURATION_SCREENSIZE_NORMAL`.
pub const ACONFIGURATION_SCREENSIZE_NORMAL: i32 = 0x02;
/// `ACONFIGURATION_SCREENSIZE_LARGE`.
pub const ACONFIGURATION_SCREENSIZE_LARGE: i32 = 0x03;
/// `ACONFIGURATION_SCREENSIZE_XLARGE`.
pub const ACONFIGURATION_SCREENSIZE_XLARGE: i32 = 0x04;

/// `ACONFIGURATION_NAVHIDDEN_ANY`.
pub const ACONFIGURATION_NAVHIDDEN_ANY: i32 = 0x0000;
/// `ACONFIGURATION_NAVHIDDEN_NO`.
pub const ACONFIGURATION_NAVHIDDEN_NO: i32 = 0x1;
/// `ACONFIGURATION_NAVHIDDEN_YES`.
pub const ACONFIGURATION_NAVHIDDEN_YES: i32 = 0x2;

/// Which screen-size bucket the device is in.
///
/// An enum rather than a raw `i32` so a host cannot supply a number `android/configuration.h`
/// does not define — the same reason `HwcapPolicy` is an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenSize {
    /// `ACONFIGURATION_SCREENSIZE_SMALL`.
    Small,
    /// `ACONFIGURATION_SCREENSIZE_NORMAL`.
    Normal,
    /// `ACONFIGURATION_SCREENSIZE_LARGE`.
    Large,
    /// `ACONFIGURATION_SCREENSIZE_XLARGE`.
    ExtraLarge,
}

impl ScreenSize {
    /// The NDK's own number for this bucket.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        match self {
            ScreenSize::Small => ACONFIGURATION_SCREENSIZE_SMALL,
            ScreenSize::Normal => ACONFIGURATION_SCREENSIZE_NORMAL,
            ScreenSize::Large => ACONFIGURATION_SCREENSIZE_LARGE,
            ScreenSize::ExtraLarge => ACONFIGURATION_SCREENSIZE_XLARGE,
        }
    }
}

/// What the embedding says the device's configuration is.
///
/// Every field is a decision. See this module's documentation for why there is no default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceConfiguration {
    /// The two-letter ISO 639-1 language, e.g. `*b"en"`.
    ///
    /// **Two bytes, not a string**, because that is what `AConfiguration_getLanguage` writes: it
    /// takes a `char out[2]` and fills exactly two bytes, *unterminated*. A three-letter ISO 639-2
    /// code cannot be expressed through this call at all, on a device either.
    pub language: [u8; 2],
    /// The two-letter ISO 3166-1 country, e.g. `*b"US"`.
    pub country: [u8; 2],
    /// `screenWidthDp`.
    pub screen_width_dp: i32,
    /// `screenHeightDp`.
    pub screen_height_dp: i32,
    /// Which screen-size bucket.
    pub screen_size: ScreenSize,
    /// Whether the navigation is hidden: [`ACONFIGURATION_NAVHIDDEN_NO`] or
    /// [`ACONFIGURATION_NAVHIDDEN_YES`].
    pub nav_hidden: i32,
}

/// One live `AConfiguration`.
///
/// It holds a **copy** of the configuration as it was when `AConfiguration_fromAssetManager`
/// filled it, not a reference to the instance's. That is what a device does: a configuration the
/// guest holds does not change under it when the device rotates, which is why
/// `onConfigurationChanged` exists at all.
#[derive(Debug, Clone, Copy)]
pub(super) struct LiveConfiguration {
    pub(super) filled: Option<DeviceConfiguration>,
}

fn refuse(c: &ImportCall<'_, '_>, why: String) -> AbiError {
    AbiError::Refused { symbol: c.symbol().to_string(), address: c.address(), why }
}

fn count(ndk: &Ndk, symbol: &'static str) {
    *ndk.census.lock().entry(symbol).or_insert(0) += 1;
}

/// The live configuration a guest `AConfiguration *` names, **filled**.
///
/// A configuration that `AConfiguration_fromAssetManager` has not filled is refused rather than
/// answered from zeroes. `AConfiguration_new` gives the guest an *empty* one on a device too, and
/// reading a field of one is reading whatever the allocator left — so answering zero here would
/// be this layer inventing "language `\0\0`, screen size ANY", which is a legal-looking
/// configuration the engine would act on.
fn filled(ndk: &Ndk, c: &ImportCall<'_, '_>, config: u64) -> AbiResult<DeviceConfiguration> {
    let at = GuestAddr::try_from(config).ok().unwrap_or(0);
    let state = ndk.state.lock();
    let live = state.configs.get(at).ok_or_else(|| {
        refuse(
            c,
            format!(
                "`{}` was given {config:#x} as an `AConfiguration *`, and this instance did not \
                 hand that out",
                c.symbol()
            ),
        )
    })?;
    live.filled.ok_or_else(|| {
        refuse(
            c,
            String::from(
                "the guest read a field of an `AConfiguration` that `AConfiguration_fromAssetManager` \
                 has never filled. On a device `AConfiguration_new` gives back an object whose \
                 fields are whatever the allocator left, so answering zero here would be this \
                 layer inventing a legal-looking configuration -- language \\0\\0, screen size \
                 ANY -- that the engine would then act on",
            ),
        )
    })
}

/// `AConfiguration *AConfiguration_new(void)`
///
/// Returns **null** when this instance holds [`MAX_CONFIGURATIONS`](super::MAX_CONFIGURATIONS),
/// which is what a device answers on allocation failure.
fn configuration_new(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AConfiguration_new");
    let at = ndk.state.lock().configs.insert(LiveConfiguration { filled: None }).unwrap_or(0);
    c.ret().u64(at as u64);
    Ok(())
}

/// `void AConfiguration_delete(AConfiguration *config)`
fn configuration_delete(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let config = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AConfiguration_delete");
    let at = GuestAddr::try_from(config).ok().unwrap_or(0);
    if ndk.state.lock().configs.remove(at).is_none() {
        return Err(refuse(
            c,
            format!(
                "`AConfiguration_delete` was given {config:#x}, which is not a live \
                 AConfiguration. Ignoring it would make a double delete invisible"
            ),
        ));
    }
    Ok(())
}

/// `void AConfiguration_fromAssetManager(AConfiguration *out, AAssetManager *am)`
fn configuration_from_asset_manager(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (config, manager) = {
        let mut a: Args<'_> = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AConfiguration_fromAssetManager");

    let Some(decided) = ndk.configuration() else {
        return Err(refuse(
            c,
            "this guest instance's configuration has not been decided. The language, country, \
             screen size and density are facts about the *device*, and a number this layer picked \
             would be a number with nothing behind it -- `screenWidthDp` and the density reach \
             the engine's layout and its texture budget, so an invented 1080p phone would be a \
             device profile nobody chose and nothing in any log would say so. \
             `Ndk::set_configuration` is what decides"
                .to_string(),
        ));
    };
    let config_at = GuestAddr::try_from(config).ok().unwrap_or(0);
    let manager_at = GuestAddr::try_from(manager).ok().unwrap_or(0);
    let mut state = ndk.state.lock();
    if state.managers.get(manager_at).is_none() {
        return Err(AbiError::Refused {
            symbol: c.symbol().to_string(),
            address: c.address(),
            why: format!(
                "`AConfiguration_fromAssetManager` was given {manager:#x} as an `AAssetManager *`, \
                 and this instance did not hand that out. On a device the configuration really \
                 does come out of the asset manager's resource table, so a wrong pointer here is \
                 a caller whose configuration would otherwise silently be the right one"
            ),
        });
    }
    let live = state.configs.get_mut(config_at).ok_or_else(|| AbiError::Refused {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: format!(
            "`AConfiguration_fromAssetManager` was given {config:#x} as an `AConfiguration *`, and \
             this instance did not hand that out"
        ),
    })?;
    // A **copy**, as a device makes: the guest's configuration does not change under it when the
    // host's does, which is why `onConfigurationChanged` exists.
    live.filled = Some(decided);
    let thread = Ndk::thread_index(&mut state);
    state.record(
        config_at,
        thread,
        "configuration",
        format!(
            "{}-{} {}x{} dp",
            String::from_utf8_lossy(&decided.language),
            String::from_utf8_lossy(&decided.country),
            decided.screen_width_dp,
            decided.screen_height_dp
        ),
    );
    Ok(())
}

/// `void AConfiguration_getLanguage(AConfiguration *config, char *outLanguage)`
/// and `AConfiguration_getCountry`.
///
/// **Two bytes, unterminated.** The NDK's header says `char outLanguage[2]`, and the caller
/// supplies a two-byte buffer; writing a third byte — even a NUL — writes past the object the
/// caller gave. That is the shape this project refuses everywhere else and it is refused here by
/// writing exactly two.
fn write_pair(c: &mut ImportCall<'_, '_>, pick: fn(&DeviceConfiguration) -> [u8; 2]) -> AbiResult<()> {
    let (config, out) = {
        let mut a: Args<'_> = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let ndk = active(c.symbol(), c.address())?;
    let decided = filled(&ndk, c, config)?;
    if out == 0 {
        return Err(refuse(
            c,
            format!("`{}` was given a null output buffer", c.symbol()),
        ));
    }
    let at = GuestAddr::try_from(out)
        .map_err(|_| refuse(c, "a guest pointer wider than the host's usize".to_string()))?;
    c.mem().write_bytes(at, &pick(&decided), Blame::new(c.symbol(), c.address(), 1))?;
    Ok(())
}

fn configuration_get_language(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AConfiguration_getLanguage");
    write_pair(c, |decided| decided.language)
}

fn configuration_get_country(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AConfiguration_getCountry");
    write_pair(c, |decided| decided.country)
}

/// `int32_t AConfiguration_getScreenWidthDp(AConfiguration *config)`
fn configuration_get_screen_width_dp(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let config = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AConfiguration_getScreenWidthDp");
    let decided = filled(&ndk, c, config)?;
    c.ret().i32(decided.screen_width_dp);
    Ok(())
}

/// `int32_t AConfiguration_getScreenHeightDp(AConfiguration *config)`
fn configuration_get_screen_height_dp(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let config = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AConfiguration_getScreenHeightDp");
    let decided = filled(&ndk, c, config)?;
    c.ret().i32(decided.screen_height_dp);
    Ok(())
}

/// `int32_t AConfiguration_getScreenSize(AConfiguration *config)`
fn configuration_get_screen_size(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let config = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AConfiguration_getScreenSize");
    let decided = filled(&ndk, c, config)?;
    c.ret().i32(decided.screen_size.as_i32());
    Ok(())
}

/// `int32_t AConfiguration_getNavHidden(AConfiguration *config)`
fn configuration_get_nav_hidden(c: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let config = c.args().next_u64()?;
    let ndk = active(c.symbol(), c.address())?;
    count(&ndk, "AConfiguration_getNavHidden");
    let decided = filled(&ndk, c, config)?;
    c.ret().i32(decided.nav_hidden);
    Ok(())
}

/// Every configuration symbol. All nine are inline: none calls guest code and none changes the
/// address space.
pub(super) static INLINE: &[(&str, ImportFn)] = &[
    ("AConfiguration_new", configuration_new),
    ("AConfiguration_delete", configuration_delete),
    ("AConfiguration_fromAssetManager", configuration_from_asset_manager),
    ("AConfiguration_getLanguage", configuration_get_language),
    ("AConfiguration_getCountry", configuration_get_country),
    ("AConfiguration_getScreenWidthDp", configuration_get_screen_width_dp),
    ("AConfiguration_getScreenHeightDp", configuration_get_screen_height_dp),
    ("AConfiguration_getScreenSize", configuration_get_screen_size),
    ("AConfiguration_getNavHidden", configuration_get_nav_hidden),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The screen-size numbers are `android/configuration.h`'s own, asserted against literals.
    #[test]
    fn the_screen_size_buckets_are_the_ndk_numbers() {
        assert_eq!(ScreenSize::Small.as_i32(), 1);
        assert_eq!(ScreenSize::Normal.as_i32(), 2);
        assert_eq!(ScreenSize::Large.as_i32(), 3);
        assert_eq!(ScreenSize::ExtraLarge.as_i32(), 4);
        assert_eq!(ACONFIGURATION_SCREENSIZE_ANY, 0);
        assert_eq!(ACONFIGURATION_NAVHIDDEN_ANY, 0);
        assert_eq!(ACONFIGURATION_NAVHIDDEN_NO, 1);
        assert_eq!(ACONFIGURATION_NAVHIDDEN_YES, 2);
        // Distinct, so a caller cannot take the wrong branch on two buckets that render the same.
        let all = [
            ScreenSize::Small.as_i32(),
            ScreenSize::Normal.as_i32(),
            ScreenSize::Large.as_i32(),
            ScreenSize::ExtraLarge.as_i32(),
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
    }
}
