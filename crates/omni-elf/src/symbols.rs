//! The dynamic symbol table, its string table, and both hash tables.

use crate::consts::*;
use crate::error::{ElfError, Result};
use crate::reader::View;

/// The dynamic string table.
#[derive(Clone, Copy)]
pub struct StrTab<'a> {
    data: &'a [u8],
}

impl<'a> StrTab<'a> {
    #[inline]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The NUL-terminated byte string at `offset`, without the NUL.
    pub fn bytes_at(&self, offset: u64) -> Result<&'a [u8]> {
        let start = usize::try_from(offset).map_err(|_| ElfError::StringOffsetOutOfBounds {
            offset,
            strsz: self.data.len() as u64,
        })?;
        let rest = self
            .data
            .get(start..)
            .ok_or(ElfError::StringOffsetOutOfBounds {
                offset,
                strsz: self.data.len() as u64,
            })?;
        let end = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or(ElfError::UnterminatedString { offset })?;
        Ok(&rest[..end])
    }

    /// The string at `offset`, which must be valid UTF-8.
    ///
    /// ELF strings are bytes, not UTF-8, so this can legitimately fail; [`Self::bytes_at`] is
    /// the escape hatch. Every symbol name in the target APK is ASCII.
    pub fn get(&self, offset: u64) -> Result<&'a str> {
        core::str::from_utf8(self.bytes_at(offset)?).map_err(|_| ElfError::NonUtf8String { offset })
    }
}

/// One `Elf64_Sym`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sym {
    pub st_name: u32,
    pub st_info: u8,
    pub st_other: u8,
    pub st_shndx: u16,
    pub st_value: u64,
    pub st_size: u64,
}

impl Sym {
    /// `ELF64_ST_BIND`.
    #[inline]
    pub fn bind(&self) -> u8 {
        self.st_info >> 4
    }

    /// `ELF64_ST_TYPE`.
    #[inline]
    pub fn sym_type(&self) -> u8 {
        self.st_info & 0xf
    }

    /// `ELF64_ST_VISIBILITY`.
    #[inline]
    pub fn visibility(&self) -> u8 {
        self.st_other & 0x3
    }

    /// `st_shndx == SHN_UNDEF`: an import, to be satisfied by another object.
    #[inline]
    pub fn is_undefined(&self) -> bool {
        self.st_shndx == SHN_UNDEF
    }

    #[inline]
    pub fn is_weak(&self) -> bool {
        self.bind() == STB_WEAK
    }

    #[inline]
    pub fn is_func(&self) -> bool {
        self.sym_type() == STT_FUNC
    }

    /// An `STT_OBJECT` import is *data*, and binding it to a function stub produces a failure
    /// that names no symbol. Ten of Roblox's imports are of this kind (D9).
    #[inline]
    pub fn is_object(&self) -> bool {
        self.sym_type() == STT_OBJECT
    }

    #[inline]
    pub fn is_ifunc(&self) -> bool {
        self.sym_type() == STT_GNU_IFUNC
    }

    #[inline]
    pub fn is_tls(&self) -> bool {
        self.sym_type() == STT_TLS
    }

    /// Is this a defined, externally visible symbol other objects may bind to?
    #[inline]
    pub fn is_exported(&self) -> bool {
        !self.is_undefined()
            && self.st_name != 0
            && matches!(self.bind(), STB_GLOBAL | STB_WEAK | STB_GNU_UNIQUE)
            && matches!(self.visibility(), STV_DEFAULT | STV_PROTECTED)
    }

    /// Human-readable `st_info` type name, for reports.
    pub fn type_name(&self) -> &'static str {
        match self.sym_type() {
            STT_NOTYPE => "NOTYPE",
            STT_OBJECT => "OBJECT",
            STT_FUNC => "FUNC",
            STT_SECTION => "SECTION",
            STT_FILE => "FILE",
            STT_COMMON => "COMMON",
            STT_TLS => "TLS",
            STT_GNU_IFUNC => "GNU_IFUNC",
            _ => "UNKNOWN",
        }
    }

    /// Human-readable `st_info` binding name, for reports.
    pub fn bind_name(&self) -> &'static str {
        match self.bind() {
            STB_LOCAL => "LOCAL",
            STB_GLOBAL => "GLOBAL",
            STB_WEAK => "WEAK",
            STB_GNU_UNIQUE => "GNU_UNIQUE",
            _ => "UNKNOWN",
        }
    }
}

/// The `.dynsym` table, as reached through `DT_SYMTAB`.
#[derive(Clone, Copy)]
pub struct SymbolTable<'a> {
    view: View<'a>,
    count: u32,
}

impl<'a> SymbolTable<'a> {
    /// `view` must start at `DT_SYMTAB` and be at least `count * 24` bytes.
    pub fn new(view: View<'a>, count: u32) -> Self {
        Self { view, count }
    }

    /// Number of entries, index 0 (the reserved null symbol) included.
    #[inline]
    pub fn len(&self) -> u32 {
        self.count
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn get(&self, index: u32) -> Result<Sym> {
        if index >= self.count {
            return Err(ElfError::SymbolIndexOutOfBounds {
                index,
                count: self.count,
            });
        }
        let at = index as usize * SIZEOF_SYM;
        Ok(Sym {
            st_name: self.view.u32("st_name", at)?,
            st_info: self.view.u8("st_info", at + 4)?,
            st_other: self.view.u8("st_other", at + 5)?,
            st_shndx: self.view.u16("st_shndx", at + 6)?,
            st_value: self.view.u64("st_value", at + 8)?,
            st_size: self.view.u64("st_size", at + 16)?,
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = Result<Sym>> + '_ {
        (0..self.count).map(move |i| self.get(i))
    }
}

/// The classic `DT_HASH` table.
#[derive(Clone, Copy)]
pub struct SysvHash<'a> {
    view: View<'a>,
    nbucket: u32,
    nchain: u32,
}

impl<'a> SysvHash<'a> {
    pub fn parse(view: View<'a>) -> Result<Self> {
        let nbucket = view.u32("DT_HASH nbucket", 0)?;
        let nchain = view.u32("DT_HASH nchain", 4)?;
        // Force the bounds check now rather than on first lookup.
        let need = 8usize
            .checked_add((nbucket as usize).saturating_mul(4))
            .and_then(|n| n.checked_add((nchain as usize).saturating_mul(4)))
            .ok_or(ElfError::BadSysvHash("nbucket/nchain overflow a usize"))?;
        if view.len() < need {
            return Err(ElfError::OutOfBounds {
                what: "DT_HASH table",
                offset: 0,
                need,
                have: view.len(),
            });
        }
        Ok(Self {
            view,
            nbucket,
            nchain,
        })
    }

    /// `nchain` is by construction the number of entries in `.dynsym`.
    #[inline]
    pub fn symbol_count(&self) -> u32 {
        self.nchain
    }

    #[inline]
    pub fn bucket_count(&self) -> u32 {
        self.nbucket
    }

    /// The SysV ELF hash function.
    pub fn hash(name: &[u8]) -> u32 {
        let mut h: u32 = 0;
        for &c in name {
            h = h.wrapping_mul(16).wrapping_add(c as u32);
            let g = h & 0xf000_0000;
            if g != 0 {
                h ^= g >> 24;
            }
            h &= !g;
        }
        h
    }

    /// Walk the chain for `name`, returning the matching symbol index.
    pub fn lookup(&self, name: &[u8], symtab: &SymbolTable<'_>, strtab: &StrTab<'_>) -> Result<Option<u32>> {
        if self.nbucket == 0 {
            return Ok(None);
        }
        let h = Self::hash(name);
        let bucket_at = 8 + (h % self.nbucket) as usize * 4;
        let mut index = self.view.u32("DT_HASH bucket", bucket_at)?;
        // A malformed table can form a cycle; bound the walk by the chain length.
        for _ in 0..=self.nchain {
            if index == SHN_UNDEF as u32 {
                return Ok(None);
            }
            if index >= self.nchain {
                return Err(ElfError::BadSysvHash("chain index past nchain"));
            }
            let sym = symtab.get(index)?;
            if strtab.bytes_at(sym.st_name as u64)? == name {
                return Ok(Some(index));
            }
            let chain_at = 8 + self.nbucket as usize * 4 + index as usize * 4;
            index = self.view.u32("DT_HASH chain", chain_at)?;
        }
        Err(ElfError::BadSysvHash("chain does not terminate"))
    }
}

/// The `DT_GNU_HASH` table.
#[derive(Clone, Copy)]
pub struct GnuHash<'a> {
    view: View<'a>,
    nbucket: u32,
    /// First symbol index covered by the hash table. Every index below it is **not** in the
    /// table, which for a shared library means it is an undefined symbol (an import).
    symndx: u32,
    maskwords: u32,
    shift2: u32,
    bloom_at: usize,
    buckets_at: usize,
    chains_at: usize,
}

impl<'a> GnuHash<'a> {
    pub fn parse(view: View<'a>) -> Result<Self> {
        let nbucket = view.u32("DT_GNU_HASH nbucket", 0)?;
        let symndx = view.u32("DT_GNU_HASH symndx", 4)?;
        let maskwords = view.u32("DT_GNU_HASH maskwords", 8)?;
        let shift2 = view.u32("DT_GNU_HASH shift2", 12)?;
        if nbucket == 0 {
            return Err(ElfError::BadGnuHash("nbucket is zero"));
        }
        if !maskwords.is_power_of_two() {
            return Err(ElfError::BadGnuHash("maskwords is not a power of two"));
        }
        let bloom_at = 16usize;
        let buckets_at = bloom_at
            .checked_add((maskwords as usize).saturating_mul(8))
            .ok_or(ElfError::BadGnuHash("maskwords overflows a usize"))?;
        let chains_at = buckets_at
            .checked_add((nbucket as usize).saturating_mul(4))
            .ok_or(ElfError::BadGnuHash("nbucket overflows a usize"))?;
        if view.len() < chains_at {
            return Err(ElfError::OutOfBounds {
                what: "DT_GNU_HASH header, bloom filter and buckets",
                offset: 0,
                need: chains_at,
                have: view.len(),
            });
        }
        Ok(Self {
            view,
            nbucket,
            symndx,
            maskwords,
            shift2,
            bloom_at,
            buckets_at,
            chains_at,
        })
    }

    /// The first symbol index the hash table covers. Indices `1..symndx` are the imports.
    #[inline]
    pub fn symndx(&self) -> u32 {
        self.symndx
    }

    #[inline]
    pub fn bucket_count(&self) -> u32 {
        self.nbucket
    }

    #[inline]
    pub fn bloom_words(&self) -> u32 {
        self.maskwords
    }

    /// The GNU hash function (`djb2` with multiplier 33).
    pub fn hash(name: &[u8]) -> u32 {
        let mut h: u32 = 5381;
        for &c in name {
            h = h.wrapping_mul(33).wrapping_add(c as u32);
        }
        h
    }

    fn chain_at(&self, index: u32) -> Result<u32> {
        let at = self
            .chains_at
            .checked_add((index.wrapping_sub(self.symndx)) as usize * 4)
            .ok_or(ElfError::BadGnuHash("chain offset overflows a usize"))?;
        self.view.u32("DT_GNU_HASH chain", at)
    }

    /// Derive the number of entries in `.dynsym` by finding the largest index any chain reaches.
    ///
    /// `DT_GNU_HASH` does not record the symbol count, and a runtime loader has no section
    /// headers to fall back on, so this walk is the only way to size the table when `DT_HASH` is
    /// absent — which is the case for `libroblox.so`.
    pub fn derive_symbol_count(&self) -> Result<u32> {
        let mut max_index = self.symndx.saturating_sub(1);
        for b in 0..self.nbucket {
            let at = self.buckets_at + b as usize * 4;
            let first = self.view.u32("DT_GNU_HASH bucket", at)?;
            if first == 0 {
                continue;
            }
            if first < self.symndx {
                return Err(ElfError::BadGnuHash("bucket points below symndx"));
            }
            let mut index = first;
            loop {
                let chain = self.chain_at(index)?;
                if index > max_index {
                    max_index = index;
                }
                if chain & 1 != 0 {
                    break;
                }
                index = index
                    .checked_add(1)
                    .ok_or(ElfError::BadGnuHash("chain index overflow"))?;
            }
        }
        max_index
            .checked_add(1)
            .ok_or(ElfError::BadGnuHash("symbol count overflow"))
    }

    /// Look up `name`, returning its symbol index. Only *defined, exported* symbols are
    /// reachable this way; that is a property of the format, not a limitation here.
    pub fn lookup(
        &self,
        name: &[u8],
        symtab: &SymbolTable<'_>,
        strtab: &StrTab<'_>,
    ) -> Result<Option<u32>> {
        let h = Self::hash(name);

        // Bloom filter: a miss here is definitive.
        let word_index = ((h / 64) & (self.maskwords - 1)) as usize;
        let word = self
            .view
            .u64("DT_GNU_HASH bloom word", self.bloom_at + word_index * 8)?;
        let bit1 = h % 64;
        let bit2 = (h >> self.shift2) % 64;
        if word >> bit1 & 1 == 0 || word >> bit2 & 1 == 0 {
            return Ok(None);
        }

        let bucket_at = self.buckets_at + (h % self.nbucket) as usize * 4;
        let first = self.view.u32("DT_GNU_HASH bucket", bucket_at)?;
        if first == 0 {
            return Ok(None);
        }
        if first < self.symndx {
            return Err(ElfError::BadGnuHash("bucket points below symndx"));
        }
        let mut index = first;
        loop {
            let chain = self.chain_at(index)?;
            if chain | 1 == h | 1 {
                let sym = symtab.get(index)?;
                if strtab.bytes_at(sym.st_name as u64)? == name {
                    return Ok(Some(index));
                }
            }
            if chain & 1 != 0 {
                return Ok(None);
            }
            index = index
                .checked_add(1)
                .ok_or(ElfError::BadGnuHash("chain index overflow"))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gnu_hash_matches_known_values() {
        // The djb2-with-multiplier-33 variant used by DT_GNU_HASH, cross-computed against an
        // independent implementation of the same definition.
        assert_eq!(GnuHash::hash(b""), 5381);
        assert_eq!(GnuHash::hash(b"printf"), 0x156b_2bb8);
        assert_eq!(GnuHash::hash(b"exit"), 0x7c96_7e3f);
        assert_eq!(GnuHash::hash(b"syscall"), 0xbac2_12a0);
        assert_eq!(GnuHash::hash(b"JNI_OnLoad"), 0x8e82_23e2);
    }

    #[test]
    fn sysv_hash_matches_known_values() {
        // Cross-computed against the gABI reference `elf_hash`. The real proof that both hash
        // functions are right is the golden test that resolves every exported symbol of the
        // three libraries carrying both tables through each one; these pin the arithmetic.
        assert_eq!(SysvHash::hash(b""), 0);
        assert_eq!(SysvHash::hash(b"printf"), 0x0779_05a6);
        assert_eq!(SysvHash::hash(b"exit"), 0x0006_cf04);
        assert_eq!(SysvHash::hash(b"syscall"), 0x0b09_985c);
        assert_eq!(SysvHash::hash(b"JNI_OnLoad"), 0x0467_e784);
    }

    #[test]
    fn sysv_hash_never_exceeds_28_bits() {
        // The `h &= ~g` step is what keeps it in range; dropping it silently changes every
        // bucket index, so the invariant is worth pinning.
        for len in 0..40usize {
            let name: Vec<u8> = (0..len).map(|i| 0x80 | (i as u8)).collect();
            assert!(SysvHash::hash(&name) < 1 << 28, "len {len}");
        }
    }

    #[test]
    fn strtab_rejects_unterminated_and_out_of_range() {
        let st = StrTab::new(b"abc\0def");
        assert_eq!(st.get(0).unwrap(), "abc");
        assert_eq!(st.get(1).unwrap(), "bc");
        assert!(matches!(
            st.get(4).unwrap_err(),
            ElfError::UnterminatedString { offset: 4 }
        ));
        assert!(matches!(
            st.get(100).unwrap_err(),
            ElfError::StringOffsetOutOfBounds { offset: 100, strsz: 7 }
        ));
    }

    #[test]
    fn sym_info_decoding() {
        let s = Sym {
            st_info: (STB_WEAK << 4) | STT_OBJECT,
            st_other: STV_HIDDEN,
            st_shndx: SHN_UNDEF,
            ..Default::default()
        };
        assert_eq!(s.bind(), STB_WEAK);
        assert_eq!(s.sym_type(), STT_OBJECT);
        assert!(s.is_weak() && s.is_object() && s.is_undefined() && !s.is_func());
        assert_eq!(s.visibility(), STV_HIDDEN);
        assert!(!s.is_exported());
    }
}
