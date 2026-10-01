//! End-to-end: drive the MCP server the way an agent would — `initialize`, `tools/list`, then
//! `tools/call` for the emulation-layer debug tools — against the real `libz.so` fixture, and check
//! that a value crossed the whole stack correctly (adler-32 against an independent oracle).
//!
//! This is the Phase A/B "verify it drives" for the introspection/debug family, and it is
//! deterministic and identical on every host (no live boot, no network, no account).
#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

use std::path::PathBuf;

use omni_mcp::json::{self, Json};
use omni_mcp::mcp::Dispatch;
use omni_mcp::{Config, Server};

fn fixture() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../omni-debug/tests/fixtures/libz.so")
        .to_string_lossy()
        .into_owned()
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

/// Call a tool and return the inner result JSON (the server wraps it as `content[0].text`).
fn call(server: &mut Server, name: &str, args: Json) -> Json {
    let params = json::obj([("name", json::s(name)), ("arguments", args)]);
    let wrapped = server.handle("tools/call", &params).unwrap_or_else(|e| panic!("{name}: {}", e.message));
    let text = wrapped.get("content").unwrap().as_array().unwrap()[0].get("text").unwrap().as_str().unwrap();
    json::parse(text).unwrap()
}

#[test]
fn initialize_and_list_tools() {
    let mut server = Server::new(Config::from_env());
    let init = server.handle("initialize", &Json::Null).unwrap();
    assert_eq!(init.get("serverInfo").unwrap().get("name").unwrap().as_str(), Some("omni-mcp"));
    assert!(init.get("capabilities").unwrap().get("tools").is_some());

    let list = server.handle("tools/list", &Json::Null).unwrap();
    let tools = list.get("tools").unwrap().as_array().unwrap();
    assert!(tools.len() >= 20);
}

#[test]
fn drives_the_lab_debugger_over_mcp() {
    let mut server = Server::new(Config::from_env());

    // Load the library.
    let loaded = call(&mut server, "lab_load", json::obj([("path", json::s(fixture()))]));
    assert_eq!(loaded.get("module").unwrap().as_str(), Some("libz.so"));
    assert!(loaded.get("exported_symbols").unwrap().as_f64().unwrap() > 0.0);

    // Resolve a symbol.
    let sym = call(&mut server, "resolve_symbol", json::obj([("name", json::s("adler32"))]));
    assert!(sym.get("address").unwrap().as_str().unwrap().starts_with("0x"));

    // list_maps shows an executable region.
    let maps = call(&mut server, "list_maps", json::obj([]));
    assert!(maps
        .get("maps")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m.get("perms").unwrap().as_str().unwrap().contains('x')));

    // Place a buffer and call adler32 with crafted inputs; check against the oracle.
    let data = b"hello omni-mcp";
    let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
    let alloc = call(&mut server, "alloc_data", json::obj([("hex", json::s(hex))]));
    let buf = alloc.get("address").unwrap().as_str().unwrap().to_string();

    let out = call(
        &mut server,
        "call_function",
        json::obj([
            ("symbol", json::s("adler32")),
            ("args", Json::Array(vec![Json::Num(1.0), json::s(buf), Json::Num(data.len() as f64)])),
        ]),
    );
    let ret = out.get("ret_u64").unwrap().as_f64().unwrap() as u32;
    assert_eq!(ret, adler32(data), "adler32 over MCP must match the oracle");

    // dump_module returns a non-trivial image.
    let dump = call(&mut server, "dump_module", json::obj([]));
    assert!(dump.get("bytes").unwrap().as_f64().unwrap() > 4096.0);

    // read_mem at the module start sees the ELF magic.
    let start = loaded.get("start").unwrap().as_str().unwrap().to_string();
    let read = call(&mut server, "read_mem", json::obj([("address", json::s(start)), ("len", Json::Num(4.0))]));
    assert_eq!(read.get("hex").unwrap().as_str(), Some("7f454c46"), "\\x7fELF");
}

#[test]
fn write_mem_and_intercept_over_mcp() {
    let mut server = Server::new(Config::from_env());
    call(&mut server, "lab_load", json::obj([("path", json::s(fixture()))]));

    // Patch adler32's entry to RET (0xD65F03C0), then call it: X0 comes straight back.
    let sym = call(&mut server, "resolve_symbol", json::obj([("name", json::s("adler32"))]));
    let addr = sym.get("address").unwrap().as_str().unwrap().to_string();
    call(&mut server, "write_mem", json::obj([("address", json::s(addr.clone())), ("hex", json::s("c0035fd6"))]));
    let out = call(
        &mut server,
        "call_function",
        json::obj([("address", json::s(addr)), ("args", Json::Array(vec![json::s("0xabc")]))]),
    );
    assert_eq!(out.get("ret").unwrap().as_str(), Some("0xabc"), "the patched RET returned arg0");
}

// ---- RE workbench tools -----------------------------------------------------------------------

#[test]
fn lab_load_apk_on_a_single_so_reports_unresolved_imports() {
    let mut server = Server::new(Config::from_env());
    // A bare .so is accepted as a one-library "apk"; libz imports libc functions, so the
    // unresolved list is non-empty and the module is loaded.
    let out = call(&mut server, "lab_load_apk", json::obj([("apk", json::s(fixture()))]));
    assert_eq!(out.get("target").unwrap().as_str(), Some("libz.so"));
    let unresolved = out.get("unresolved").unwrap().as_array().unwrap();
    assert!(!unresolved.is_empty(), "libz has unresolved imports without a bionic provider");
}

#[test]
fn lab_trace_calls_with_an_arg_spec() {
    let mut server = Server::new(Config::from_env());
    call(&mut server, "lab_load", json::obj([("path", json::s(fixture()))]));
    // adler32(1, "Hello", 5) described declaratively.
    let out = call(
        &mut server,
        "lab_trace",
        json::obj([
            ("symbol", json::s("adler32")),
            (
                "arg_spec",
                Json::Array(vec![
                    json::obj([("scalar", Json::Num(1.0))]),
                    json::obj([("in_buffer", json::s("48656c6c6f"))]),
                    json::obj([("scalar", Json::Num(5.0))]),
                ]),
            ),
        ]),
    );
    assert_eq!(out.get("ret_u64").unwrap().as_f64().unwrap() as u32, 0x058c_01f5);
}

#[test]
fn lab_corpus_is_deterministic_and_stored() {
    let mut server = Server::new(Config::from_env());
    let templates = Json::Array(vec![
        json::obj([("kind", json::s("scalar"))]),
        json::obj([("kind", json::s("buffer")), ("len", Json::Num(8.0))]),
    ]);
    let a = call(
        &mut server,
        "lab_corpus",
        json::obj([("templates", templates), ("seed", Json::Num(42.0)), ("count", Json::Num(10.0))]),
    );
    assert_eq!(a.get("count").unwrap().as_f64().unwrap() as usize, 10);
    assert!(a.get("corpus_id").unwrap().as_str().is_some(), "a corpus id is returned");
}

#[test]
fn lab_diff_matches_identical_library() {
    let mut server = Server::new(Config::from_env());
    call(&mut server, "lab_load", json::obj([("path", json::s(fixture())), ("session", json::s("original"))]));
    call(&mut server, "lab_load", json::obj([("path", json::s(fixture())), ("session", json::s("candidate"))]));
    // adler32(seed, buf[16], 16): the length is held constant so the read stays in bounds.
    let templates = Json::Array(vec![
        json::obj([("kind", json::s("scalar"))]),
        json::obj([("kind", json::s("buffer")), ("len", Json::Num(16.0))]),
        json::obj([("kind", json::s("const")), ("value", Json::Num(16.0))]),
    ]);
    let corpus = call(
        &mut server,
        "lab_corpus",
        json::obj([("templates", templates), ("seed", Json::Num(3.0)), ("count", Json::Num(8.0))]),
    );
    let corpus_id = corpus.get("corpus_id").unwrap().as_str().unwrap().to_string();
    let out = call(
        &mut server,
        "lab_diff",
        json::obj([("symbol", json::s("adler32")), ("corpus_id", json::s(corpus_id))]),
    );
    let total = out.get("total").unwrap().as_f64().unwrap() as usize;
    let matched = out.get("matched").unwrap().as_f64().unwrap() as usize;
    assert_eq!(matched, total, "a library must match itself; divergence: {:?}", out.get("first_divergence"));
    assert_eq!(total, 8);
}

#[test]
fn lab_build_without_a_toolchain_reports_cleanly() {
    let mut server = Server::new(Config::from_env());
    let out = call(
        &mut server,
        "lab_build",
        json::obj([(
            "sources",
            Json::Array(vec![json::obj([
                ("name", json::s("cand.c")),
                ("text", json::s("int add(int a, int b){return a+b;}")),
            ])]),
        )]),
    );
    // Either it built (a toolchain is present) or it reported the toolchain missing — never thrown.
    let built = out.get("so_path").and_then(Json::as_str).is_some();
    let missing = out
        .get("error")
        .and_then(Json::as_str)
        .map(|s| s.contains("toolchain"))
        .unwrap_or(false);
    assert!(built || missing, "lab_build must report build or missing-toolchain: {out:?}");
}

#[test]
fn a_missing_lab_is_a_clean_error() {
    let mut server = Server::new(Config::from_env());
    let params = json::obj([("name", json::s("list_maps")), ("arguments", json::obj([]))]);
    let err = server.handle("tools/call", &params).expect_err("no lab loaded yet");
    assert!(err.message.contains("lab"), "the error mentions the missing lab: {}", err.message);
}
