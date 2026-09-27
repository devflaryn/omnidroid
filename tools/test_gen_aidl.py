#!/usr/bin/env python3
"""Tests of tools/gen_aidl.py: python -m unittest tools/test_gen_aidl.py (or run it)."""
import os
import re
import shutil
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import gen_aidl as gen  # noqa: E402

PKG = "test.omni.demo"
HASH = "0123456789abcdef0123456789abcdef01234567"


def write_package(root, files, pkg=PKG, version="1", hashes=(HASH,)):
    base = os.path.join(root, pkg, version)
    for name, body in files.items():
        path = os.path.join(base, *pkg.split("."), name)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write("package %s;\n%s" % (pkg, body))
    with open(os.path.join(base, ".hash"), "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(hashes) + "\n")


def item(text, header):
    """The generated item starting at `header` (a struct, enum, impl or module), to its closing
    brace at column 0 (or 4, for items inside an interface module)."""
    start = text.index(header)
    indent = len(header) - len(header.lstrip())
    end = text.index("\n" + " " * indent + "}\n", start)
    return text[start:end + indent + 3]


class Synthetic(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="gen_aidl_")

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def gen(self, files, **kw):
        write_package(self.tmp, files, **kw)
        out, model = gen.generate(self.tmp, [(PKG, kw.get("version", "1"))])
        return out["test_omni_demo.rs"], model

    def test_a_union_is_a_tag_then_the_field(self):
        rs, _ = self.gen({
            "Point.aidl": "parcelable Point { int x; int y; }\n",
            "Choice.aidl": "union Choice {\n  boolean flag = true;\n  test.omni.demo.Point point;\n  @nullable String name;\n  long[] values;\n}\n",
        })
        self.assertIn("pub enum Choice {\n    Flag(bool),\n    Point(Point),\n    Name(Option<String>),\n    Values(Vec<i64>),\n}", rs)
        # The default is the first field, with its default value.
        self.assertIn("impl Default for Choice {\n    fn default() -> Self {\n        Self::Flag(true)\n", rs)
        body = item(rs, "impl Choice {")
        self.assertIn("0 => Self::Flag(r.bool()?),", body)
        self.assertIn("1 => Self::Point(Point::read_value(r)?),", body)
        self.assertIn("2 => Self::Name(r.string16()?),", body)
        self.assertIn("3 => Self::Values(r.i64_array()?.ok_or(Malformed(\"a null array\"))?),", body)
        self.assertIn('_ => return Err(Malformed("an unknown Choice tag")),', body)
        self.assertIn("Self::Point(v) => {\n                w.i32(1);\n                (*v).write_value(w);", body)
        # A union has no size header, but is nested like a parcelable.
        self.assertNotIn("parcelable_body", body)
        self.assertIn("r.enter()?;", body)

    def test_a_parcelable_honours_its_defaults(self):
        rs, _ = self.gen({
            "Mode.aidl": "@Backing(type=\"int\")\nenum Mode { OFF = 0, ON = (1 << 2) /* 4 */, AUTO = (OFF | ON) /* 4 */, NEXT }\n",
            "Settings.aidl": (
                "parcelable Settings {\n"
                "  int count = 5;\n"
                "  float ratio = 1.5f;\n"
                "  @utf8InCpp String label = \"hi \\\"there\\\"\";\n"
                "  boolean enabled = true;\n"
                "  test.omni.demo.Mode mode = test.omni.demo.Mode.ON;\n"
                "  long big = (1L << 40);\n"
                "  int[] list = {1, 2, -3};\n"
                "  byte small = -1;\n"
                "  @nullable String nothing;\n"
                "  int plain;\n"
                "  const int LIMIT = 0x7fffffff;\n"
                "  const long WIDE = -1L;\n"
                "  const String NAME = \"settings\";\n"
                "}\n"
            ),
        })
        # Explicit defaults: no derived Default, a hand-written one.
        self.assertIn("#[derive(Debug, Clone, PartialEq)]\npub struct Settings {", rs)
        default = item(rs, "impl Default for Settings {")
        for line in (
            "count: 5,",
            "ratio: 1.5,",
            'label: String::from("hi \\"there\\""),',
            "enabled: true,",
            "mode: Mode::ON,",
            "big: 1099511627776,",
            "list: vec![1, 2, -3],",
            "small: -1,",
            "nothing: None,",
            "plain: Default::default(),",
        ):
            self.assertIn(line, default)
        body = item(rs, "impl Settings {")
        self.assertIn("pub const LIMIT: i32 = 2147483647;", body)
        self.assertIn("pub const WIDE: i64 = -1;", body)
        self.assertIn('pub const NAME: &str = "settings";', body)
        # A field an older writer did not send keeps its default.
        self.assertIn("let v0 = if r.within(end) { r.i32()? } else { 5 };", body)
        self.assertIn("let v4 = if r.within(end) { Mode::read(r)? } else { Mode::ON };", body)
        # Enumerators: expressions, references to siblings, and an implicit next value.
        enum = item(rs, "impl Mode {")
        self.assertIn("pub const ON: Self = Self(4);", enum)
        self.assertIn("pub const AUTO: Self = Self(4);", enum)
        self.assertIn("pub const NEXT: Self = Self(5);", enum)

    def test_an_array_of_a_byte_backed_enum_is_a_byte_array(self):
        rs, _ = self.gen({
            "Bits.aidl": "enum Bits { A, B = 3, C }\n",  # no @Backing: byte
            "Wide.aidl": "@Backing(type=\"long\") enum Wide { X = (1L << 40) }\n",
            "Holder.aidl": "parcelable Holder { test.omni.demo.Bits one; test.omni.demo.Bits[] many; test.omni.demo.Wide[] wide; byte[] raw; }\n",
        })
        self.assertIn("pub struct Bits(pub i8);", rs)
        self.assertIn("pub const C: Self = Self(4);", rs)
        self.assertIn("pub struct Wide(pub i64);", rs)
        body = item(rs, "impl Holder {")
        # One byte-backed enum: an i32; an array of them: packed bytes.
        self.assertIn("let v0 = if r.within(end) { Bits::read(r)? }", body)
        self.assertIn("r.byte_array()?.map(|b| b.iter().map(|&x| Bits(x as i8)).collect::<Vec<_>>())", body)
        self.assertIn("w.byte_array(&self.many.iter().map(|x| x.0 as u8).collect::<Vec<u8>>());", body)
        self.assertIn("r.i64_array()?.map(|v| v.into_iter().map(Wide).collect::<Vec<_>>())", body)
        self.assertIn("r.byte_array()?.map(<[u8]>::to_vec)", body)
        self.assertIn("w.byte_array(&self.raw);", body)
        self.assertIn("pub raw: Vec<u8>,", rs)
        enum = item(rs, "impl Bits {")
        self.assertIn("Ok(Self(r.i8()?))", enum)
        self.assertIn("w.i8(self.0);", enum)

    def test_a_oneway_method_has_no_reply(self):
        rs, _ = self.gen({
            "Event.aidl": "parcelable Event { long when; }\n",
            "IListener.aidl": (
                "interface IListener {\n"
                "  oneway void onEvent(long display, in test.omni.demo.Event event);\n"
                "  int count(out int[] seen, inout test.omni.demo.Event last);\n"
                "  const int VERSION_LIKE = 2;\n"
                "}\n"
            ),
        })
        mod = item(rs, "pub mod i_listener {")
        self.assertIn("pub const TRANSACTION_ON_EVENT: u32 = 1;", mod)
        self.assertIn("pub const TRANSACTION_COUNT: u32 = 2;", mod)
        self.assertIn('pub const HASH: &str = "%s";' % HASH, mod)
        self.assertIn("pub const VERSION: i32 = 1;", mod)
        self.assertIn("pub const VERSION_LIKE: i32 = 2;", mod)
        self.assertIn("fn on_event(&self, ctx: &Ctx<'_>, display: i64, event: Event) -> Result<(), Status> {", mod)
        # The dispatcher calls it and writes nothing back.
        arm = mod[mod.index("TRANSACTION_ON_EVENT => {"):mod.index("TRANSACTION_COUNT => {")]
        self.assertIn("let _ = svc.on_event(&ctx, a_display, a_event);", arm)
        self.assertNotIn("status_ok", arm)
        # The proxy queues it and does not wait.
        self.assertIn("self.broker.host_transact_oneway(self.handle, TRANSACTION_ON_EVENT, w.data, &offsets)", mod)
        self.assertNotIn("decode_on_event", mod)
        # out and inout: not/also in the request; after the return value in the reply.
        self.assertIn("fn count(&self, ctx: &Ctx<'_>, last: Event) -> Result<(i32, Vec<i32>, Event), Status> {", mod)
        self.assertIn("pub fn encode_count(last: &Event) -> Writer {", mod)
        self.assertIn("w.i32(ret.0);\n", mod)
        self.assertIn("w.i32_array(&ret.1);", mod)
        self.assertIn("ret.2.write_value(w);", mod)
        self.assertIn("pub use i_listener::{IListenerProxy, IListenerServer};", rs)

    def test_a_nullable_array_of_parcelables_has_nullable_elements(self):
        rs, _ = self.gen({
            "Rect.aidl": "parcelable Rect { int l; }\n",
            "Damage.aidl": "parcelable Damage { @nullable test.omni.demo.Rect[] rects; @nullable int[] ints; ParcelFileDescriptor fd; List<String> names; }\n",
        })
        self.assertIn("pub rects: Option<Vec<Option<Rect>>>,", rs)
        self.assertIn("pub ints: Option<Vec<i32>>,", rs)
        self.assertIn("pub fd: Fd,", rs)
        self.assertIn("pub names: Vec<String>,", rs)
        # A required file descriptor has no default: no Default, and a missing one is malformed.
        self.assertIn("#[derive(Debug, Clone, PartialEq)]\npub struct Damage {", rs)
        self.assertNotIn("impl Default for Damage", rs)
        self.assertIn('return Err(Malformed("Damage.fd is missing"))', rs)

    def test_what_it_cannot_generate_is_an_error_naming_where(self):
        for body, what in (
            ("parcelable Bad { Map<String, int> m; }\n", "Map is not supported"),
            ("parcelable Bad { ParcelableHolder ext; }\n", "ParcelableHolder is not supported"),
            ("parcelable Bad { test.omni.demo.Missing m; }\n", "unknown type test.omni.demo.Missing"),
            ("parcelable Bad;\n", "unstructured parcelable Bad"),
            ("parcelable Bad<T> { T t; }\n", "generic parcelable Bad"),
            ("parcelable Bad { @nullable test.omni.demo.Bad next; }\n", "holds itself by value"),
        ):
            with self.subTest(what=what):
                shutil.rmtree(self.tmp, ignore_errors=True)
                with self.assertRaises(gen.GenError) as e:
                    self.gen({"Bad.aidl": body})
                self.assertIn(what, str(e.exception))
                self.assertRegex(str(e.exception), r"test/omni/demo/Bad\.aidl:\d+")

    def test_the_last_hash_line_is_the_interface_hash(self):
        rs, model = self.gen({"IEmpty.aidl": "interface IEmpty { void nothing(); }\n"}, hashes=("a" * 40, "b" * 40))
        self.assertIn('pub const HASH: &str = "%s";' % ("b" * 40), rs)


class Real(unittest.TestCase):
    """The vendored composer3 V3, graphics.common V5 and hardware.common V2."""

    @classmethod
    def setUpClass(cls):
        cls.files, cls.model = gen.generate()

    def test_generation_is_deterministic(self):
        again, _ = gen.generate()
        self.assertEqual(self.files, again)

    def test_the_checked_in_output_is_up_to_date(self):
        for name, text in self.files.items():
            with open(os.path.join(gen.OUT_DIR, name), encoding="utf-8", newline="") as f:
                self.assertEqual(f.read(), text, name)

    def test_every_file_starts_with_the_header(self):
        for name, text in self.files.items():
            self.assertTrue(text.startswith(gen.HEADER), name)

    def test_counts(self):
        self.assertEqual(gen.stats(self.model), (89, 3, 57))

    def test_composer_client(self):
        rs = self.files["android_hardware_graphics_composer3.rs"]
        mod = item(rs, "pub mod i_composer_client {")
        self.assertIn('pub const DESCRIPTOR: &str = "android.hardware.graphics.composer3.IComposerClient";', mod)
        self.assertIn('pub const HASH: &str = "d24fcd9648b8b2e7287f9238eee9180244612c10";', mod)
        self.assertIn("pub const VERSION: i32 = 3;", mod)
        self.assertIn("pub const TRANSACTION_GET_DISPLAY_ATTRIBUTE: u32 = 9;", mod)
        self.assertIn("pub const TRANSACTION_NOTIFY_EXPECTED_PRESENT: u32 = 47;", mod)
        self.assertIn("pub const EX_BAD_DISPLAY: i32 = 2;", mod)
        self.assertIn("fn get_display_attribute(&self, ctx: &Ctx<'_>, display: i64, config: i32, attribute: DisplayAttribute) -> Result<i32, Status> {", mod)
        self.assertIn("fn set_content_type(&self, ctx: &Ctx<'_>, display: i64, r#type: ContentType)", mod)
        self.assertIn("fn get_readback_buffer_fence(&self, ctx: &Ctx<'_>, display: i64) -> Result<Option<Fd>, Status> {", mod)
        common = self.files["android_hardware_graphics_common.rs"]
        self.assertIn("pub const VENDOR_MASK_HI: Self = Self(-281474976710656);", common)
        self.assertIn("pub const ROT_270: Self = Self(7);", common)
        self.assertIn("usage: BufferUsage::CPU_READ_NEVER,", common)


if __name__ == "__main__":
    unittest.main()
