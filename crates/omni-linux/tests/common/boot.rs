//! A whole boot for the framework gates (C4, C5): the image's `derive_classpath`, then
//! `omni-linux-run` with init's classes, the display HALs and system_server started as the zygote
//! would start it, its output read line by line.
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use omni_linux::fd::Output;
use omni_linux::{ExitStatus, Process, SpawnConfig};

/// The image's `derive_classpath`, run in `instance`: its `export NAME VALUE` lines.
pub fn derive_classpath(sysroot: &Path, instance: &Path) -> Vec<(String, String)> {
    std::fs::create_dir_all(instance.join("data/system/environ")).expect("/data/system/environ");
    let err = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let p = Process::spawn_as(
        SpawnConfig {
            sysroot: sysroot.to_path_buf(),
            instance_dir: instance.to_path_buf(),
            argv: vec![b"/apex/com.android.sdkext/bin/derive_classpath".to_vec()],
            envp: vec![b"PATH=/system/bin".to_vec()],
            stdout: Output::Capture(Arc::default()),
            stderr: Output::Capture(Arc::clone(&err)),
            trace: false,
        },
        1000,
    )
    .expect("derive_classpath");
    assert_eq!(p.run(), ExitStatus::Exited(0), "{}", String::from_utf8_lossy(&err.lock()));
    let text = std::fs::read_to_string(instance.join("data/system/environ/classpath")).expect("the classpath derive_classpath writes");
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            (w.next() == Some("export")).then(|| (w.next().unwrap_or_default().to_string(), w.next().unwrap_or_default().to_string()))
        })
        .collect()
}

/// A booting system: the runner's merged output, line by line.
pub struct Boot {
    child: Child,
    lines: mpsc::Receiver<String>,
    /// The last lines seen, for a failure's message.
    pub tail: std::collections::VecDeque<String>,
    pub instance: PathBuf,
    /// Every line, kept for a failure's reader: `<temp>/<instance name>.log`.
    pub log: PathBuf,
    log_file: Option<std::fs::File>,
}

impl Boot {
    /// Boot `instance` (made fresh) with system_server, `extra` runner arguments and the `then`
    /// shell command run beside it as the shell user.
    pub fn start(sysroot: &Path, instance: PathBuf, extra: &[&str], then: &str) -> Self {
        let exports = derive_classpath(sysroot, &instance);
        let ss_classpath = exports.iter().find(|(k, _)| k == "SYSTEMSERVERCLASSPATH").map(|(_, v)| v.clone()).expect("SYSTEMSERVERCLASSPATH");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_omni-linux-run"));
        cmd.args(["--sysroot", &sysroot.to_string_lossy(), "--instance", &instance.to_string_lossy(), "--uid", "1000"]);
        for (k, v) in &exports {
            cmd.arg("--env").arg(format!("{k}={v}"));
        }
        cmd.args(["--env", &format!("CLASSPATH={ss_classpath}")]);
        cmd.args(["--env", "ANDROID_ART_ROOT=/apex/com.android.art", "--env", "ANDROID_I18N_ROOT=/apex/com.android.i18n", "--env", "ANDROID_TZDATA_ROOT=/apex/com.android.tzdata"]);
        cmd.args(["--init", "early_hal,core,hal,main,late_start", "--hal", "gralloc", "--hal", "composer"]);
        cmd.args(["--setprop", "dalvik.vm.profilesystemserver=true"]);
        // The capabilities the zygote gives system_server (ZygoteInit.forkSystemServer).
        cmd.args(["--caps", "IPC_LOCK,KILL,NET_ADMIN,NET_BIND_SERVICE,NET_BROADCAST,NET_RAW,SYS_MODULE,SYS_NICE,SYS_PTRACE,SYS_TIME,SYS_TTY_CONFIG,WAKE_ALARM,BLOCK_SUSPEND"]);
        cmd.args(extra);
        cmd.args(["--then", then]);
        cmd.args(["--", "/system/bin/app_process64", "-Xgc:CMC", "-Xhidden-api-policy:disabled", "/system/bin", "com.android.server.SystemServer"]);
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("omni-linux-run");
        let (tx, lines) = mpsc::channel::<String>();
        for stream in [Box::new(child.stdout.take().expect("stdout")) as Box<dyn std::io::Read + Send>, Box::new(child.stderr.take().expect("stderr"))] {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        return;
                    }
                }
            });
        }
        let log = instance.with_extension("log");
        let log_file = std::fs::File::create(&log).ok();
        Self { child, lines, tail: std::collections::VecDeque::new(), instance, log, log_file }
    }

    /// Give each line to `seen` until it answers that the boot has shown what it must, the runner
    /// ends, or `limit` passes.
    pub fn watch(&mut self, limit: Duration, mut seen: impl FnMut(&str) -> bool) {
        let deadline = Instant::now() + limit;
        let mut done = false;
        while Instant::now() < deadline && !done {
            let Ok(line) = self.lines.recv_timeout(Duration::from_secs(5)) else {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                continue;
            };
            if let Some(f) = &mut self.log_file {
                use std::io::Write;
                let _ = writeln!(f, "{line}");
            }
            done = seen(&line);
            self.tail.push_back(line);
            if self.tail.len() > 80 {
                self.tail.pop_front();
            }
        }
    }

    /// The last lines, joined, and where the whole log is.
    pub fn tail(&self) -> String {
        format!("{}\n(the whole boot: {})", self.tail.iter().cloned().collect::<Vec<_>>().join("\n"), self.log.display())
    }
}

impl Drop for Boot {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.instance);
    }
}
