//! What an APK says it is: the root `<manifest>` element's `package`, `android:versionName` and
//! `android:versionCode`, read from the binary `AndroidManifest.xml`.
//!
//! **Only that element, and only those three attributes.** The rest of the manifest -- activities,
//! permissions, and every value that is a reference into `resources.arsc` -- stays undecoded, which
//! is still the crate's rule (see the crate docs). These three exist here because an APK is
//! *chosen* by them: the host tells the engine the app version it was installed as, and a
//! directory holding several APKs is resolved to the newest ([`crate::choose_apk`]).
//!
//! The format is AOSP's `ResXMLTree` (`frameworks/base/libs/androidfw/ResourceTypes.cpp`): a
//! `RES_XML_TYPE` chunk holding a string pool, an optional resource map, and one chunk per
//! element event. An attribute is named by a string-pool index; a shrunk APK may leave the name
//! empty, and then the resource map's id at that index names it (`versionCode` is `0x0101021b`,
//! `versionName` `0x0101021c`), which is what Android itself reads.

use std::borrow::Cow;

use crate::error::{ApkError, ApkResult};

const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_XML_TYPE: u16 = 0x0003;
const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
const RES_XML_RESOURCE_MAP_TYPE: u16 = 0x0180;
const UTF8_FLAG: u32 = 1 << 8;
const NO_INDEX: u32 = u32::MAX;

const TYPE_REFERENCE: u8 = 0x01;
const TYPE_STRING: u8 = 0x03;
const TYPE_INT_DEC: u8 = 0x10;
const TYPE_INT_HEX: u8 = 0x11;

const ATTR_VERSION_CODE: u32 = 0x0101_021b;
const ATTR_VERSION_NAME: u32 = 0x0101_021c;

/// The identity an APK declares for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppManifest {
    /// `package`, e.g. `com.roblox.client`.
    pub package: String,
    /// `android:versionName`, e.g. `2.739.691` -- what the app reports as its version.
    pub version_name: String,
    /// `android:versionCode`, the integer the store orders releases by.
    pub version_code: u32,
}

impl AppManifest {
    /// Decode the root element of a binary `AndroidManifest.xml`.
    ///
    /// # Errors
    ///
    /// [`ApkError::Manifest`] naming what is missing or malformed -- including a `versionName` that
    /// is a resource reference, which only `resources.arsc` could resolve and this crate does not
    /// read.
    pub fn parse(axml: &[u8]) -> ApkResult<AppManifest> {
        let (kind, header, size) = chunk_header(axml, 0)?;
        if kind != RES_XML_TYPE {
            return Err(bad(format!("not binary XML: the first chunk is type {kind:#06x}")));
        }
        let end = (size as usize).min(axml.len());
        let mut strings: Option<StringPool<'_>> = None;
        let mut resource_ids: &[u8] = &[];
        let mut at = usize::from(header);
        while at + 8 <= end {
            let (kind, header, size) = chunk_header(axml, at)?;
            let size = size as usize;
            if size < usize::from(header) || at + size > end {
                return Err(bad(format!("a chunk at {at:#x} runs past the document")));
            }
            let chunk = &axml[at..at + size];
            match kind {
                RES_STRING_POOL_TYPE => strings = Some(StringPool::new(chunk)?),
                RES_XML_RESOURCE_MAP_TYPE => resource_ids = &chunk[usize::from(header)..],
                RES_XML_START_ELEMENT_TYPE => {
                    let strings = strings.ok_or_else(|| bad("an element before the string pool"))?;
                    // The first element is the root, and the root is `<manifest>`.
                    return root_element(chunk, header, &strings, resource_ids);
                }
                _ => {}
            }
            at += size;
        }
        Err(bad("no element at all"))
    }
}

fn root_element(
    chunk: &[u8],
    header: u16,
    strings: &StringPool<'_>,
    resource_ids: &[u8],
) -> ApkResult<AppManifest> {
    // ResXMLTree_attrExt, after the node header: ns, name, attributeStart, attributeSize,
    // attributeCount, idIndex, classIndex, styleIndex.
    let ext = usize::from(header);
    let element = strings.get(u32_at(chunk, ext + 4)?)?;
    if element != "manifest" {
        return Err(bad(format!("the root element is <{element}>, not <manifest>")));
    }
    let attribute_start = usize::from(u16_at(chunk, ext + 8)?);
    let attribute_size = usize::from(u16_at(chunk, ext + 10)?);
    let attribute_count = usize::from(u16_at(chunk, ext + 12)?);
    if attribute_size < 20 {
        return Err(bad(format!(
            "attributes of {attribute_size} bytes, under the 20 a Res_value needs"
        )));
    }
    let resource_id = |index: u32| -> Option<u32> {
        let at = index as usize * 4;
        resource_ids.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };

    let (mut package, mut version_name, mut version_code) = (None, None, None);
    for i in 0..attribute_count {
        let at = ext + attribute_start + i * attribute_size;
        let name_index = u32_at(chunk, at + 4)?;
        let raw_value = u32_at(chunk, at + 8)?;
        let data_type = *chunk.get(at + 15).ok_or_else(|| bad("an attribute past the element"))?;
        let data = u32_at(chunk, at + 16)?;
        let name = strings.get(name_index)?;
        let id = resource_id(name_index);
        let text = || -> ApkResult<String> {
            match (raw_value, data_type) {
                (NO_INDEX, TYPE_STRING) => strings.get(data).map(Cow::into_owned),
                (NO_INDEX, TYPE_REFERENCE) => Err(bad(format!(
                    "`{name}` is a resource reference (@{data:#010x}); only resources.arsc could \
                     resolve it, and this crate does not read that"
                ))),
                (NO_INDEX, other) => {
                    Err(bad(format!("`{name}` has value type {other:#04x}, not a string")))
                }
                (index, _) => strings.get(index).map(Cow::into_owned),
            }
        };
        if name == "package" {
            package = Some(text()?);
        } else if name == "versionName" || id == Some(ATTR_VERSION_NAME) {
            version_name = Some(text()?);
        } else if name == "versionCode" || id == Some(ATTR_VERSION_CODE) {
            version_code = Some(match data_type {
                TYPE_INT_DEC | TYPE_INT_HEX => data,
                _ => text()?.parse().map_err(|_| bad("`versionCode` is not an integer"))?,
            });
        }
    }
    Ok(AppManifest {
        package: package.ok_or_else(|| bad("<manifest> has no `package`"))?,
        version_name: version_name.ok_or_else(|| bad("<manifest> has no `android:versionName`"))?,
        version_code: version_code.ok_or_else(|| bad("<manifest> has no `android:versionCode`"))?,
    })
}

/// `ResStringPool`: offsets into UTF-8 or UTF-16 strings, each prefixed by its length.
#[derive(Clone, Copy)]
struct StringPool<'a> {
    chunk: &'a [u8],
    count: u32,
    utf8: bool,
    strings_start: usize,
    offsets_at: usize,
}

impl<'a> StringPool<'a> {
    fn new(chunk: &'a [u8]) -> ApkResult<Self> {
        Ok(Self {
            chunk,
            count: u32_at(chunk, 8)?,
            utf8: u32_at(chunk, 16)? & UTF8_FLAG != 0,
            strings_start: u32_at(chunk, 20)? as usize,
            offsets_at: usize::from(u16_at(chunk, 2)?),
        })
    }

    fn get(&self, index: u32) -> ApkResult<Cow<'a, str>> {
        if index >= self.count {
            return Err(bad(format!("string {index} of a pool of {}", self.count)));
        }
        let offset = u32_at(self.chunk, self.offsets_at + index as usize * 4)? as usize;
        let at = self.strings_start + offset;
        if self.utf8 {
            // Two lengths, each one byte, or two with the high bit set: UTF-16 units, then bytes.
            let (_, at) = self.utf8_length(at)?;
            let (len, at) = self.utf8_length(at)?;
            let bytes = self.chunk.get(at..at + len).ok_or_else(|| bad("a string past its pool"))?;
            std::str::from_utf8(bytes)
                .map(Cow::Borrowed)
                .map_err(|_| bad(format!("string {index} is not UTF-8")))
        } else {
            let (len, at) = self.utf16_length(at)?;
            let bytes =
                self.chunk.get(at..at + len * 2).ok_or_else(|| bad("a string past its pool"))?;
            let units: Vec<u16> =
                bytes.chunks_exact(2).map(|u| u16::from_le_bytes([u[0], u[1]])).collect();
            String::from_utf16(&units)
                .map(Cow::Owned)
                .map_err(|_| bad(format!("string {index} is not UTF-16")))
        }
    }

    fn utf8_length(&self, at: usize) -> ApkResult<(usize, usize)> {
        let first = *self.chunk.get(at).ok_or_else(|| bad("a string past its pool"))?;
        if first & 0x80 == 0 {
            return Ok((usize::from(first), at + 1));
        }
        let second = *self.chunk.get(at + 1).ok_or_else(|| bad("a string past its pool"))?;
        Ok(((usize::from(first & 0x7f) << 8) | usize::from(second), at + 2))
    }

    fn utf16_length(&self, at: usize) -> ApkResult<(usize, usize)> {
        let first = usize::from(u16_at(self.chunk, at)?);
        if first & 0x8000 == 0 {
            return Ok((first, at + 2));
        }
        let second = usize::from(u16_at(self.chunk, at + 2)?);
        Ok((((first & 0x7fff) << 16) | second, at + 4))
    }
}

fn chunk_header(bytes: &[u8], at: usize) -> ApkResult<(u16, u16, u32)> {
    Ok((u16_at(bytes, at)?, u16_at(bytes, at + 2)?, u32_at(bytes, at + 4)?))
}

fn u16_at(bytes: &[u8], at: usize) -> ApkResult<u16> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| bad(format!("a read at {at:#x} past the end")))
}

fn u32_at(bytes: &[u8], at: usize) -> ApkResult<u32> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| bad(format!("a read at {at:#x} past the end")))
}

fn bad(detail: impl Into<String>) -> ApkError {
    ApkError::Manifest { detail: detail.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal manifest laid out as aapt2 lays one out: a UTF-8 string pool, a resource map
    /// covering the two `android:` names, and the root element with its three attributes.
    fn manifest(version_name_by_id_only: bool) -> Vec<u8> {
        let names: [&str; 6] = [
            if version_name_by_id_only { "" } else { "versionName" },
            "versionCode",
            "package",
            "manifest",
            "2.739.691",
            "com.example.app",
        ];
        let mut pool_strings = Vec::new();
        let mut offsets = Vec::new();
        for s in names {
            offsets.push(pool_strings.len() as u32);
            pool_strings.extend([s.len() as u8, s.len() as u8]);
            pool_strings.extend(s.as_bytes());
            pool_strings.push(0);
        }
        while pool_strings.len() % 4 != 0 {
            pool_strings.push(0);
        }
        let pool_header = 28u16;
        let strings_start = u32::from(pool_header) + 4 * names.len() as u32;
        let mut pool = Vec::new();
        pool.extend(RES_STRING_POOL_TYPE.to_le_bytes());
        pool.extend(pool_header.to_le_bytes());
        pool.extend((strings_start + pool_strings.len() as u32).to_le_bytes());
        pool.extend((names.len() as u32).to_le_bytes());
        pool.extend(0u32.to_le_bytes());
        pool.extend(UTF8_FLAG.to_le_bytes());
        pool.extend(strings_start.to_le_bytes());
        pool.extend(0u32.to_le_bytes());
        for offset in offsets {
            pool.extend(offset.to_le_bytes());
        }
        pool.extend(pool_strings);

        let mut map = Vec::new();
        map.extend(RES_XML_RESOURCE_MAP_TYPE.to_le_bytes());
        map.extend(8u16.to_le_bytes());
        map.extend(16u32.to_le_bytes());
        map.extend(ATTR_VERSION_NAME.to_le_bytes());
        map.extend(ATTR_VERSION_CODE.to_le_bytes());

        let attribute = |name: u32, raw: u32, data_type: u8, data: u32| {
            let mut a = Vec::new();
            a.extend(NO_INDEX.to_le_bytes());
            a.extend(name.to_le_bytes());
            a.extend(raw.to_le_bytes());
            a.extend(8u16.to_le_bytes());
            a.push(0);
            a.push(data_type);
            a.extend(data.to_le_bytes());
            a
        };
        let attributes = [
            attribute(0, 4, TYPE_STRING, 4),
            attribute(1, NO_INDEX, TYPE_INT_DEC, 1_234),
            attribute(2, 5, TYPE_STRING, 5),
        ];
        let mut element = Vec::new();
        element.extend(RES_XML_START_ELEMENT_TYPE.to_le_bytes());
        element.extend(16u16.to_le_bytes());
        element.extend((16 + 20 + 20 * attributes.len() as u32).to_le_bytes());
        element.extend(1u32.to_le_bytes());
        element.extend(NO_INDEX.to_le_bytes());
        element.extend(NO_INDEX.to_le_bytes());
        element.extend(3u32.to_le_bytes());
        element.extend(20u16.to_le_bytes());
        element.extend(20u16.to_le_bytes());
        element.extend((attributes.len() as u16).to_le_bytes());
        element.extend([0u8; 6]);
        for a in attributes {
            element.extend(a);
        }

        let mut doc = Vec::new();
        doc.extend(RES_XML_TYPE.to_le_bytes());
        doc.extend(8u16.to_le_bytes());
        doc.extend(((8 + pool.len() + map.len() + element.len()) as u32).to_le_bytes());
        doc.extend(pool);
        doc.extend(map);
        doc.extend(element);
        doc
    }

    #[test]
    fn the_root_attributes_are_read_by_name() {
        assert_eq!(
            AppManifest::parse(&manifest(false)).expect("parse"),
            AppManifest {
                package: "com.example.app".to_string(),
                version_name: "2.739.691".to_string(),
                version_code: 1_234,
            }
        );
    }

    #[test]
    fn a_name_left_empty_by_a_shrinker_is_found_by_its_resource_id() {
        let parsed = AppManifest::parse(&manifest(true)).expect("parse");
        assert_eq!(parsed.version_name, "2.739.691");
    }

    #[test]
    fn a_document_that_is_not_binary_xml_is_refused_by_name() {
        let error = AppManifest::parse(b"<manifest/>").expect_err("text XML");
        assert!(error.to_string().contains("not binary XML"), "{error}");
    }
}
