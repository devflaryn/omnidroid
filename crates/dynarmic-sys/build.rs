//! Builds the pinned dynarmic tree and the Omnidroid C shim over it.
//!
//! Everything this script needs is vendored under `vendor/`, so no network
//! access happens here or at test time. What it does need from the host is a
//! C++ toolchain and CMake; when one is missing the script says which one and
//! stops, rather than letting CMake print two hundred lines about a failed
//! compiler check.
//!
//! Layout: `OUT_DIR/b` is the CMake binary directory. The name is one letter on
//! purpose. MSVC still refuses paths over 260 characters (`C1083`), CMake nests
//! object files around 120 characters deep inside the binary directory, and
//! `OUT_DIR` is already ~90 characters for a crate in a workspace. That leaves
//! very little headroom, so the script checks for it up front.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Longest path CMake+Ninja generate below the binary directory, measured on
/// the pinned tree (`floating_point_data_processing_three_register.cpp.obj`,
/// 118 characters), rounded up for headroom.
const MAX_RELATIVE_BUILD_PATH: usize = 160;
const MAX_WINDOWS_PATH: usize = 259;

fn fail(lines: &[String]) -> ! {
    eprintln!();
    eprintln!("dynarmic-sys could not build.");
    for l in lines {
        eprintln!("  {l}");
    }
    eprintln!();
    std::process::exit(1);
}

fn s(x: &str) -> String {
    x.to_string()
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Cargo watches these and nothing else. The vendored tree is 39 MiB and
    // ~4,000 files; making Cargo stat all of it on every build would cost more
    // than it saves. Touch `vendor/PIN.txt` (or `cargo clean -p dynarmic-sys`)
    // after editing anything under `vendor/`.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=shim/od_dynarmic.cpp");
    println!("cargo:rerun-if-changed=shim/od_dynarmic.h");
    println!("cargo:rerun-if-changed=vendor/PIN.txt");
    println!("cargo:rerun-if-env-changed=OMNIDROID_DYNARMIC_BUILD_DIR");
    // These pick a different compiler or a different CMake, which changes the
    // object files without changing a single watched source file.
    println!("cargo:rerun-if-env-changed=CMAKE");
    println!("cargo:rerun-if-env-changed=CXX");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=OMNIDROID_DYNARMIC_ALLOW_BROKEN_WX");

    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_arch != "x86_64" && target_arch != "aarch64" {
        fail(&[
            format!("Target architecture `{target_arch}` has no dynarmic backend."),
            s("dynarmic emits host code for x86_64 or aarch64 only."),
        ]);
    }

    let vendor = manifest.join("vendor");
    let dynarmic_src = vendor.join("dynarmic");
    let boost_include = vendor.join("boost");
    if !dynarmic_src.join("CMakeLists.txt").is_file() {
        fail(&[
            format!("Vendored dynarmic tree is missing: {}", dynarmic_src.display()),
            s("It is committed to this repository; a partial checkout or a"),
            s("`.gitignore` rule is the usual cause."),
        ]);
    }
    if !boost_include.join("boost/version.hpp").is_file() {
        fail(&[
            format!("Vendored Boost header subset is missing: {}", boost_include.display()),
            s("dynarmic needs boost::icl and boost::variant; see vendor/boost/README.md."),
        ]);
    }

    let build_dir = match env::var_os("OMNIDROID_DYNARMIC_BUILD_DIR") {
        Some(v) => PathBuf::from(v),
        None => out_dir.join("b"),
    };
    check_path_budget(&build_dir);
    std::fs::create_dir_all(&build_dir).unwrap_or_else(|e| {
        fail(&[format!("Could not create {}: {e}", build_dir.display())])
    });

    let cmake = find_cmake();
    let compiler = find_compiler();

    configure(&cmake, &compiler, &dynarmic_src, &boost_include, &manifest.join("cmake/boost"), &build_dir);
    build(&cmake, &compiler, &build_dir);

    // The shim is compiled separately rather than added to the CMake project so
    // that `cc` picks the same CRT and flags Cargo is using for the rest of the
    // crate, and so that editing the shim does not re-run CMake.
    let mut shim = cc::Build::new();
    shim.cpp(true)
        .file(manifest.join("shim/od_dynarmic.cpp"))
        .include(manifest.join("shim"))
        .include(dynarmic_src.join("src"))
        .include(dynarmic_src.join("externals/mcl/include"))
        .include(dynarmic_src.join("externals/fmt/include"))
        .include(&boost_include)
        .define("NOMINMAX", None)
        .define("WIN32_LEAN_AND_MEAN", None)
        // The same answer CMake was given, so `od_jit_effective_config` reports
        // the protection the code cache actually has rather than a guess.
        .define("OD_DYNARMIC_W_XOR_X", if want_w_xor_x() { "1" } else { "0" });
    if compiler.is_like_msvc() {
        shim.flag("/std:c++20").flag("/EHsc");
    } else {
        shim.flag("-std=c++20");
    }
    shim.try_compile("od_dynarmic").unwrap_or_else(|e| {
        fail(&[
            format!("Compiling the C shim failed: {e}"),
            s("The dynarmic library itself built, so this is a shim problem,"),
            s("not a toolchain problem."),
        ])
    });

    emit_link_directives(&build_dir);
}

/// Whether `DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT` was asked for, having first
/// refused to hand back `true` on a pin where it does not work.
///
/// D12 says Omnidroid never holds a page that is writable and executable at
/// once. dynarmic's default leaves its code cache `PAGE_EXECUTE_READWRITE`
/// (`block_of_code.cpp:280`), so that is false for the component holding every
/// byte of generated guest code, and this option is the upstream switch for it.
///
/// It does not work. On this pin, on Windows x86-64, a build with it on
/// segfaults immediately -- **including dynarmic's own test suite**, which is
/// how we know it is the option and not our integration:
///
/// ```text
/// cmake -S crates/dynarmic-sys/vendor/dynarmic -B <build> -DDYNARMIC_TESTS=ON \
///       -DDYNARMIC_ENABLE_NO_EXECUTE_SUPPORT=ON
/// <build>/tests/dynarmic_tests.exe "[a64]"   # SIGSEGV on the first test
/// ```
///
/// The feature is kept so the next re-pin can retest in one flag, and the
/// escape hatch is kept so that retest does not need a code change. Turning it
/// on without the escape hatch is a clear stop rather than an access violation
/// in every test.
fn want_w_xor_x() -> bool {
    if env::var_os("CARGO_FEATURE_W_XOR_X").is_none() {
        return false;
    }
    if env::var_os("OMNIDROID_DYNARMIC_ALLOW_BROKEN_WX").is_some() {
        println!("cargo:warning=dynarmic-sys: W^X code cache enabled; it crashes on this pin");
        return true;
    }
    fail(&[
        s("The `w-xor-x` feature is enabled, and it does not work on this pin."),
        s("dynarmic built with DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT=ON segfaults"),
        s("immediately on Windows x86-64 -- its own test suite included, so this"),
        s("is upstream and not the Omnidroid shim."),
        s("See crates/dynarmic-sys/patches/README.md."),
        s("To retest it on a new pin anyway, set"),
        s("OMNIDROID_DYNARMIC_ALLOW_BROKEN_WX=1."),
    ])
}

/// Refuses to start a build that MSVC will abandon with `C1083` a minute in.
fn check_path_budget(build_dir: &Path) {
    if !cfg!(windows) {
        return;
    }
    let len = build_dir.as_os_str().len();
    let budget = MAX_WINDOWS_PATH.saturating_sub(MAX_RELATIVE_BUILD_PATH);
    if len > budget {
        fail(&[
            format!("The CMake build directory path is {len} characters, and the"),
            format!("budget is {budget}: CMake nests object files up to"),
            format!("{MAX_RELATIVE_BUILD_PATH} characters below it and MSVC still fails with"),
            s("C1083 past 260 characters."),
            format!("  {}", build_dir.display()),
            s("Either move the checkout closer to the drive root, or set"),
            s("OMNIDROID_DYNARMIC_BUILD_DIR to a short path such as C:\\od-build."),
        ]);
    }
}

fn find_cmake() -> PathBuf {
    let exe = env::var_os("CMAKE").unwrap_or_else(|| OsString::from("cmake"));
    let ok = Command::new(&exe)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        fail(&[
            format!("`{}` could not be run.", PathBuf::from(&exe).display()),
            s("CMake 3.12 or newer is required to build dynarmic."),
            s("Install it and put it on PATH, or set the CMAKE environment variable."),
        ]);
    }
    PathBuf::from(exe)
}

/// True when Ninja is usable. Ninja is not required, but the difference is
/// large enough (27 s against several minutes with the Visual Studio
/// generator) that it is worth preferring and worth saying so.
fn have_ninja() -> bool {
    Command::new("ninja")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn find_compiler() -> cc::Tool {
    let mut b = cc::Build::new();
    b.cpp(true);
    b.try_get_compiler().unwrap_or_else(|e| {
        let hint = if cfg!(windows) {
            s("Install \"Desktop development with C++\" from the Visual Studio \
               Build Tools. cl.exe does not need to be on PATH; it is located \
               through the registry.")
        } else {
            s("Install g++ or clang++, or set the CXX environment variable.")
        };
        fail(&[
            s("No C++ compiler was found."),
            format!("cc reported: {e}"),
            hint,
        ])
    })
}

/// The C compiler CMake is given. `cl.exe` compiles both languages, so on MSVC it is the C++
/// compiler's own path, exactly as before. Anywhere else the C++ driver (`c++`, `clang++`) is
/// refused as a C compiler by CMake's own check (`CMAKE_C_COMPILER is set to a C++ compiler`,
/// measured on macOS with Apple clang 16; on Linux `g++` fails CMake's C compiler check the same
/// way), so the matching C driver is asked of `cc` instead, and `CC` keeps working as `CXX` does.
fn c_compiler_for(cxx: &cc::Tool) -> PathBuf {
    if cxx.is_like_msvc() {
        return cxx.path().to_path_buf();
    }
    let mut b = cc::Build::new();
    b.cpp(false);
    match b.try_get_compiler() {
        Ok(tool) => tool.path().to_path_buf(),
        Err(e) => fail(&[
            s("No C compiler was found (dynarmic's externals include C sources)."),
            format!("cc reported: {e}"),
            s("Install clang or gcc, or set the CC environment variable."),
        ]),
    }
}

fn cmake_cmd(cmake: &Path, compiler: &cc::Tool) -> Command {
    let mut c = Command::new(cmake);
    // cl.exe is useless without INCLUDE/LIB/PATH pointing at the matching
    // Windows SDK. `cc` works those out from the registry; pass them straight
    // through so CMake's compiler check sees the same environment.
    for (k, v) in compiler.env() {
        c.env(k, v);
    }
    c
}

fn configure(
    cmake: &Path,
    compiler: &cc::Tool,
    src: &Path,
    boost_include: &Path,
    boost_cmake_dir: &Path,
    build_dir: &Path,
) {
    let mut c = cmake_cmd(cmake, compiler);
    c.arg("-S").arg(src).arg("-B").arg(build_dir);
    if have_ninja() {
        c.arg("-G").arg("Ninja");
    }
    // Always Release. A debug dynarmic is too slow to run a game engine under,
    // and Rust's msvc target links the release CRT regardless of Cargo profile,
    // so a Debug build here would also mix CRTs.
    c.arg("-DCMAKE_BUILD_TYPE=Release");
    c.arg("-DCMAKE_POLICY_DEFAULT_CMP0091=NEW");
    c.arg("-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreadedDLL");
    // robin-map still declares `cmake_minimum_required(VERSION 3.1)`, which
    // CMake 4.x refuses outright. This is the documented escape hatch.
    c.arg("-DCMAKE_POLICY_VERSION_MINIMUM=3.5");
    // Boost is an undeclared dynarmic dependency (icl and variant). Both
    // variables are passed: BOOST_ROOT for CMake's FindBoost module, Boost_DIR
    // for the CONFIG-mode shim in `cmake/boost`, which takes over if a future
    // CMake finishes removing FindBoost.
    c.arg(format!("-DBOOST_ROOT={}", cmake_path(boost_include)));
    c.arg(format!("-DBoost_INCLUDE_DIR={}", cmake_path(boost_include)));
    c.arg(format!("-DBoost_DIR={}", cmake_path(boost_cmake_dir)));
    // Every external dynarmic uses is vendored, and the build is meant to use those copies. Its
    // CMake prefers an installed package when one exists (`find_package(fmt 9 CONFIG)` and so
    // on), and on a macOS host with Homebrew it found `/opt/homebrew/lib/cmake/fmt` (MEASURED),
    // built no `libfmt.a` of its own and left the link to a system library the pin never named.
    // The upstream switch makes it take the vendored tree. Not passed for Windows targets, whose
    // configure line is left exactly as it was measured.
    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os != "windows") {
        c.arg("-DDYNARMIC_USE_BUNDLED_EXTERNALS=ON");
    }
    c.arg("-DDYNARMIC_TESTS=OFF");
    c.arg("-DBUILD_TESTING=OFF");
    c.arg("-DDYNARMIC_FRONTENDS=A64");
    // Upstream turns warnings into errors for a master-project build. We are a
    // consumer, and a new compiler's new warning is not our defect.
    c.arg("-DDYNARMIC_WARNINGS_AS_ERRORS=OFF");
    // Leave asserts in. dynarmic's asserts terminate the process, which is bad,
    // but DYNARMIC_IGNORE_ASSERTS replaces them with undefined behaviour, which
    // is worse. The shim's job is to make the reachable ones unreachable.
    c.arg("-DDYNARMIC_IGNORE_ASSERTS=OFF");
    // D12 says Omnidroid never holds a page that is writable and executable at
    // once. Upstream's default leaves dynarmic's code cache
    // `PAGE_EXECUTE_READWRITE`, which makes that false for the component that
    // holds every byte of generated guest code. The `w-xor-x` feature turns it
    // on; `README.md` records the measured cost of doing so.
    c.arg(format!(
        "-DDYNARMIC_ENABLE_NO_EXECUTE_SUPPORT={}",
        if want_w_xor_x() { "ON" } else { "OFF" }
    ));
    c.arg(format!("-DCMAKE_C_COMPILER={}", cmake_path(&c_compiler_for(compiler))));
    c.arg(format!("-DCMAKE_CXX_COMPILER={}", cmake_path(compiler.path())));

    run(c, "CMake configure", build_dir);
}

fn build(cmake: &Path, compiler: &cc::Tool, build_dir: &Path) {
    let jobs = env::var("NUM_JOBS").ok().unwrap_or_else(|| "4".into());
    let mut c = cmake_cmd(cmake, compiler);
    c.arg("--build")
        .arg(build_dir)
        .arg("--config")
        .arg("Release")
        .arg("--parallel")
        .arg(jobs)
        .arg("--target")
        .arg("dynarmic");
    run(c, "CMake build", build_dir);
}

fn run(mut c: Command, what: &str, build_dir: &Path) {
    let status = c.status().unwrap_or_else(|e| {
        fail(&[format!("{what} could not be started: {e}")])
    });
    if !status.success() {
        fail(&[
            format!("{what} failed with {status}."),
            format!("Build directory: {}", build_dir.display()),
            s("The CMake output above has the details."),
        ]);
    }
}

/// CMake wants forward slashes even on Windows; a backslash in a `-D` value is
/// an escape character to its parser.
fn cmake_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

fn emit_link_directives(build_dir: &Path) {
    // Relative layout of the static libraries inside the CMake binary
    // directory. Single-config generators (Ninja, Makefiles) put them here;
    // multi-config ones (Visual Studio) add a `Release/` component, which
    // `search_dirs` covers.
    let libs: [(&str, &str); 5] = [
        ("src/dynarmic", "dynarmic"),
        ("externals/fmt", "fmt"),
        ("externals/mcl/src", "mcl"),
        ("externals/zydis", "Zydis"),
        ("externals/zydis/zycore", "Zycore"),
    ];
    // Zydis (and Zycore under it) is the x86-64 backend's disassembler: dynarmic's
    // `externals/CMakeLists.txt` adds it only when `x86_64` is in `ARCHITECTURE`. The arm64
    // backend's assembler, oaknut, is an INTERFACE (header-only) library with nothing to link.
    let x86_64 = env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|a| a == "x86_64");
    let libs = libs.into_iter().filter(|(_, lib)| x86_64 || !lib.starts_with("Zy"));
    for (dir, lib) in libs {
        let base = build_dir.join(dir);
        let found = [base.clone(), base.join("Release")]
            .into_iter()
            .find(|d| {
                d.join(format!("{lib}.lib")).is_file()
                    || d.join(format!("lib{lib}.a")).is_file()
            });
        match found {
            Some(d) => {
                println!("cargo:rustc-link-search=native={}", d.display());
                println!("cargo:rustc-link-lib=static={lib}");
            }
            None => fail(&[
                format!("dynarmic built, but `{lib}` was not where it was expected:"),
                format!("  {}", base.display()),
                s("The vendored tree's CMake layout has changed."),
            ]),
        }
    }
    if cfg!(windows) {
        // dynarmic's Windows code cache allocator queries process memory.
        println!("cargo:rustc-link-lib=dylib=psapi");
    }
}
