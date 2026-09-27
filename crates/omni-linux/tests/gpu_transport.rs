//! `/dev/omni-gpu`'s transport: one `ioctl(OMNI_GPU_CALL)` per Vulkan command, reaching the host's
//! Vulkan driver. The guest's driver on top of it is `d3a_gpu.rs`.
use omni_linux::fd::Output;
use omni_linux::gpu::{command_id, OMNI_GPU_CALL};
use omni_linux::process::Process;
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};

const EINVAL: i64 = -22;
const ENOTTY: i64 = -25;

#[test]
fn a_call_reaches_the_host_driver_and_a_bad_one_is_refused() {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let mut t = p.test_task();
    let s = p.scratch();
    p.mem.write(s, b"/dev/omni-gpu\0").unwrap();
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s, 2, 0, 0, 0]) as i64;
    assert!(fd >= 0, "open /dev/omni-gpu: {fd}");

    // vkEnumerateInstanceVersion(uint32_t* pApiVersion)
    let id = command_id("vkEnumerateInstanceVersion").expect("forwarded");
    let (call, args, out) = (s + 0x100, s + 0x200, s + 0x300);
    let request = |id: u32, argc: u32| {
        let mut c = Vec::new();
        c.extend_from_slice(&id.to_le_bytes());
        c.extend_from_slice(&argc.to_le_bytes());
        c.extend_from_slice(&args.to_le_bytes());
        c.extend_from_slice(&[0u8; 16]);
        c
    };
    p.mem.write(args, &out.to_le_bytes()).unwrap();
    p.mem.write(call, &request(id, 1)).unwrap();
    let r = p.syscall(&mut t, nr::IOCTL, [fd as u64, OMNI_GPU_CALL, call, 0, 0, 0]) as i64;
    assert_eq!(r, 0, "the call");
    assert_eq!(p.mem.read_u64(call + 16).unwrap(), 0, "VK_SUCCESS");
    let version = u32::from_le_bytes(p.mem.read(out, 4).unwrap().try_into().unwrap());
    let (major, minor) = (version >> 22 & 0x7f, version >> 12 & 0x3ff);
    assert!((major, minor) >= (1, 1), "the host's Vulkan is 1.1 or later: {major}.{minor}");

    // A wrong argument count, an unknown command, another ioctl.
    p.mem.write(call, &request(id, 2)).unwrap();
    assert_eq!(p.syscall(&mut t, nr::IOCTL, [fd as u64, OMNI_GPU_CALL, call, 0, 0, 0]) as i64, EINVAL);
    p.mem.write(call, &request(u32::MAX, 1)).unwrap();
    assert_eq!(p.syscall(&mut t, nr::IOCTL, [fd as u64, OMNI_GPU_CALL, call, 0, 0, 0]) as i64, EINVAL);
    assert_eq!(p.syscall(&mut t, nr::IOCTL, [fd as u64, 0x1234, call, 0, 0, 0]) as i64, ENOTTY);
}
