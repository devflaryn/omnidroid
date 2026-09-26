//! `omni-linux-run --sysroot <dir> [--instance <dir>] [--env KEY=VALUE]... -- <program> [args...]`
use std::path::PathBuf;
use std::process::ExitCode;

use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut sysroot = PathBuf::from("sysroot/aosp-35");
    let mut instance = std::env::temp_dir().join("omni-linux-run");
    let mut argv = Vec::new();
    let mut envp = vec![b"PATH=/system/bin".to_vec(), b"ANDROID_ROOT=/system".to_vec(), b"ANDROID_DATA=/data".to_vec()];
    while let Some(a) = args.next() {
        match a.as_str() {
            "--sysroot" => sysroot = PathBuf::from(args.next().expect("--sysroot needs a value")),
            "--instance" => instance = PathBuf::from(args.next().expect("--instance needs a value")),
            "--env" => envp.push(args.next().expect("--env needs KEY=VALUE").into_bytes()),
            "--" => {
                argv.extend(args.by_ref().map(String::into_bytes));
            }
            other => {
                eprintln!("unknown argument {other:?}; usage: omni-linux-run --sysroot <dir> -- <program> [args...]");
                return ExitCode::from(2);
            }
        }
    }
    let config = SpawnConfig {
        sysroot,
        instance_dir: instance,
        argv,
        envp,
        stdout: Output::Host,
        stderr: Output::Host,
        trace: std::env::var("OMNI_SYSCALL_TRACE").as_deref() == Ok("1"),
    };
    let p = match Process::spawn(config) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("omni-linux-run: {e}");
            return ExitCode::from(127);
        }
    };
    let status = p.run();
    eprint!("{}", p.report());
    match status {
        ExitStatus::Exited(code) => ExitCode::from(code as u8),
        ExitStatus::Killed { signal, .. } => ExitCode::from(128 + signal as u8),
    }
}
