//! The image's own BPF loaders at boot: netbpfload loads the network programs and maps, hands
//! over to the platform bpfloader, which hands back (`netbpfload done`), and `bpf.progs_loaded` is
//! set -- what netd waits for. Its own test binary: the property service is one per host process,
//! made from the first sysroot a process is made from.
mod common;

use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::vfs::Sysroot;
use omni_linux::{ExitStatus, SpawnConfig};

#[test]
fn the_images_loaders_load_every_program_and_report_it() {
    let sysroot = common::sysroot().expect("no sysroot (tools/make_sysroot.py)");
    let instance = std::env::temp_dir().join(format!("omni-linux-bpf-{}", std::process::id()));
    let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let p = Process::spawn_as(
        SpawnConfig {
            sysroot: sysroot.clone(),
            instance_dir: instance,
            argv: vec![b"/apex/com.android.tethering/bin/netbpfload".to_vec()],
            envp: vec![b"PATH=/system/bin".to_vec()],
            stdout: Output::Capture(Arc::clone(&out)),
            stderr: Output::Capture(Arc::clone(&out)),
            trace: false,
        },
        0,
    )
    .expect("spawn");
    let status = p.run();
    let log = String::from_utf8_lossy(&out.lock()).into_owned();
    assert_eq!(status, ExitStatus::Exited(0), "{log}\n{}", p.report());
    let props = omni_linux::props::PropertyService::global(&Sysroot::open(&sysroot).unwrap());
    assert_eq!(props.get("bpf.progs_loaded").as_deref(), Some("1"), "{log}");
    let netd: Vec<String> = omni_linux::bpf::list(b"/sys/fs/bpf/netd_shared").into_iter().map(|(n, _)| n).collect();
    assert!(netd.iter().any(|n| n == "mainline_done"), "{netd:?}");
    assert!(netd.iter().any(|n| n.starts_with("map_netd_")), "netd's maps pinned: {netd:?}");
    assert!(netd.iter().any(|n| n.starts_with("prog_netd_")), "netd's programs pinned: {netd:?}");
}
