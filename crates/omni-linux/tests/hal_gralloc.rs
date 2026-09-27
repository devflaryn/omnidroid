//! The host `IAllocator` (AIDL V2) at the parcel level: requests as a guest's libbinder writes them,
//! replies as the NDK client reads them. The live path (libui in a guest) is `d2_gralloc.rs`.
use omni_linux::binder::{HostCall, HostReply};
use omni_linux::fd::FileKind;
use omni_linux::hal::gralloc::{Allocator, PIXELS_AT};
use omni_linux::hal::parcel::{Reader, Writer};

const DESCRIPTOR: &str = "android.hardware.graphics.allocator.IAllocator";
const ALLOCATE: u32 = 1;
const ALLOCATE2: u32 = 2;
const IS_SUPPORTED: u32 = 3;
const GET_SUFFIX: u32 = 4;
const GET_VERSION: u32 = 0x00ff_ffff;
const GET_HASH: u32 = 0x00ff_fffe;
const RGBA_8888: i32 = 1;
const RGBA_FP16: i32 = 0x16;
const YCBCR_420_888: i32 = 0x23;
const CPU_RW_OFTEN: i64 = 0x3 | 0x30;

fn token(w: &mut Writer) {
    w.i32(i32::MIN);
    w.i32(-1);
    w.i32(0x5359_5354); // 'SYST'
    w.string16(DESCRIPTOR);
}

/// A `BufferDescriptorInfo` as the NDK writes one (`AParcel_writeParcelable`).
fn descriptor(w: &mut Writer, name: &str, width: i32, height: i32, layers: i32, format: i32, usage: i64) {
    let at = w.parcelable_start();
    let mut n = [0u8; 128];
    n[..name.len()].copy_from_slice(name.as_bytes());
    w.i32(128);
    w.data.extend_from_slice(&n);
    w.i32(width);
    w.i32(height);
    w.i32(layers);
    w.i32(format);
    w.i64(usage);
    w.i64(0); // reservedSize
    w.i32(0); // additionalOptions
    w.parcelable_end(at);
}

fn call(a: &Allocator, code: u32, data: Vec<u8>) -> HostReply {
    a.call(HostCall { code, data, offsets: vec![], fds: vec![], handles: vec![], sender_pid: 42, sender_euid: 1000 })
}

fn request(code: u32, body: impl FnOnce(&mut Writer)) -> (u32, Vec<u8>) {
    let mut w = Writer::new();
    token(&mut w);
    body(&mut w);
    (code, w.data)
}

fn exception(reply: &HostReply) -> i32 {
    Reader::new(&reply.data).i32().unwrap()
}

#[test]
fn it_names_its_mapper_and_its_version() {
    let a = Allocator::new();
    let (c, d) = request(GET_SUFFIX, |_| {});
    let r = call(&a, c, d);
    let mut rd = Reader::new(&r.data);
    assert_eq!(rd.i32().unwrap(), 0);
    assert_eq!(rd.string16().unwrap().as_deref(), Some("omni"));

    let (c, d) = request(GET_VERSION, |_| {});
    let r = call(&a, c, d);
    let mut rd = Reader::new(&r.data);
    assert_eq!((rd.i32().unwrap(), rd.i32().unwrap()), (0, 2));

    let (c, d) = request(GET_HASH, |_| {});
    let r = call(&a, c, d);
    let mut rd = Reader::new(&r.data);
    assert_eq!(rd.i32().unwrap(), 0);
    assert_eq!(rd.string16().unwrap().as_deref(), Some("9499fec09c544e9de5be3c87125721600f8ade66"));
}

#[test]
fn it_supports_single_plane_formats_only() {
    let a = Allocator::new();
    let supported = |format| {
        let (c, d) = request(IS_SUPPORTED, |w| descriptor(w, "t", 64, 32, 1, format, CPU_RW_OFTEN));
        let r = call(&a, c, d);
        let mut rd = Reader::new(&r.data);
        assert_eq!(rd.i32().unwrap(), 0);
        rd.i32().unwrap() != 0
    };
    assert!(supported(RGBA_8888));
    assert!(supported(0x22), "IMPLEMENTATION_DEFINED");
    assert!(!supported(YCBCR_420_888));
}

#[test]
fn allocate2_answers_one_handle_whose_fd_is_a_shm_region() {
    let a = Allocator::new();
    let (c, d) = request(ALLOCATE2, |w| {
        descriptor(w, "d2-test", 64, 32, 1, RGBA_8888, CPU_RW_OFTEN);
        w.i32(1);
    });
    let r = call(&a, c, d);
    let mut rd = Reader::new(&r.data);
    assert_eq!(rd.i32().unwrap(), 0, "status");
    let end = rd.parcelable_start().unwrap();
    let stride = rd.i32().unwrap();
    assert_eq!(stride, 64);
    assert_eq!(rd.i32().unwrap(), 1, "one buffer");
    let handle_end = rd.parcelable_start().unwrap();
    assert_eq!(rd.i32().unwrap(), 1, "one fd");
    assert_eq!((rd.i32().unwrap(), rd.i32().unwrap()), (1, 0), "non-null, no comm channel");
    let fd_at = rd.position();
    rd.i64().unwrap();
    rd.i64().unwrap();
    rd.i64().unwrap();
    let n = rd.i32().unwrap();
    assert_eq!(n, 14, "ints");
    let ints: Vec<i32> = (0..n).map(|_| rd.i32().unwrap()).collect();
    rd.parcelable_end(handle_end).unwrap();
    rd.parcelable_end(end).unwrap();
    assert_eq!(rd.position(), r.data.len());

    assert_eq!(ints[0] as u32, 0x4247_4d4f, "magic");
    assert_eq!(&ints[1..6], &[1, 64, 32, 1, RGBA_8888]);
    assert_eq!((ints[6], ints[7]), (CPU_RW_OFTEN as i32, 0), "usage");
    assert_eq!(ints[8], 64, "stride");
    let id = (ints[9] as u32 as u64) | ((ints[10] as u32 as u64) << 32);
    assert_ne!(id, 0);
    assert_eq!((ints[11], ints[12]), (64 * 32 * 4, 0), "pixel bytes");
    assert_eq!(ints[13] as u64, PIXELS_AT);

    assert_eq!(r.fds.len(), 1);
    assert_eq!(r.fds[0].0, fd_at, "the fd object is where the parcel says");
    let shm = match &*r.fds[0].1.kind.lock() {
        FileKind::Shared(m) => m.clone(),
        _ => panic!("the fd is not shared memory"),
    };
    assert_eq!(shm.len(), PIXELS_AT + 64 * 32 * 4);
    let found = a.buffer(id).expect("the allocator finds the buffer by id");
    assert!(std::sync::Arc::ptr_eq(&found, &shm), "the registry holds the same region");
    // The requestor's name in the metadata page, at 64.
    let mut name = [0u8; 8];
    shm.read_at(&mut name, 64).unwrap();
    assert_eq!(&name, b"d2-test\0");
}

#[test]
fn it_refuses_what_it_cannot_allocate() {
    let a = Allocator::new();
    let service_specific = |format, w, h, layers, count| {
        let (c, d) = request(ALLOCATE2, |wr| {
            descriptor(wr, "x", w, h, layers, format, CPU_RW_OFTEN);
            wr.i32(count);
        });
        let r = call(&a, c, d);
        assert!(r.fds.is_empty(), "nothing allocated");
        let mut rd = Reader::new(&r.data);
        let ex = rd.i32().unwrap();
        rd.string16().unwrap();
        rd.i32().unwrap();
        (ex, if ex == -8 { rd.i32().unwrap() } else { 0 })
    };
    assert_eq!(service_specific(YCBCR_420_888, 64, 32, 1, 1), (-8, 2), "UNSUPPORTED");
    assert_eq!(service_specific(RGBA_8888, 64, 32, 2, 1), (-8, 2), "layers");
    assert_eq!(service_specific(RGBA_8888, 0, 32, 1, 1), (-8, 0), "BAD_DESCRIPTOR");
    assert_eq!(service_specific(RGBA_8888, -5, 32, 1, 1), (-8, 0), "BAD_DESCRIPTOR");
    assert_eq!(service_specific(RGBA_FP16, 65536, 65536, 1, 1), (-8, 1), "NO_RESOURCES");
    assert_eq!(service_specific(RGBA_8888, 64, 32, 1, 0), (-8, 0), "count 0");
    assert_eq!(service_specific(RGBA_8888, 64, 32, 1, 1000), (-8, 1), "count 1000");

    // allocate (V1's, a mapper@4 descriptor) is not served.
    let (c, d) = request(ALLOCATE, |w| {
        w.i32(4);
        w.i32(0);
        w.i32(1);
    });
    let r = call(&a, c, d);
    let mut rd = Reader::new(&r.data);
    assert_eq!(rd.i32().unwrap(), -8);
}

#[test]
fn a_malformed_parcel_is_an_illegal_argument() {
    let a = Allocator::new();
    // Truncated inside the descriptor.
    let (c, mut d) = request(ALLOCATE2, |w| descriptor(w, "x", 64, 32, 1, RGBA_8888, CPU_RW_OFTEN));
    d.truncate(d.len() - 30);
    assert_eq!(exception(&call(&a, c, d)), -3);
    // Another interface's token.
    let mut w = Writer::new();
    w.i32(0);
    w.i32(-1);
    w.i32(0);
    w.string16("android.os.IServiceManager");
    assert_eq!(exception(&call(&a, ALLOCATE2, w.data)), -3);
    // Nothing at all.
    assert_eq!(exception(&call(&a, ALLOCATE2, vec![])), -3);
    assert_eq!(exception(&call(&a, IS_SUPPORTED, vec![1, 2, 3])), -3);
}
