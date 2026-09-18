//! The leaf-function selection tool: what M2 used to pick a function out of a stripped 109 MB
//! binary, kept so the choice can be reproduced and re-run against a different build of the engine.
//!
//! ```text
//! cargo run -p omni-elf --release --example leaf-scan -- <library.so> [--kind pure|stack|tls|guarded]
//!     [--min-bytes N] [--max-bytes N] [--at 0xVADDR] [--limit N]
//! ```
//!
//! With no library named it looks for the extraction cache the test fixtures fill, so that on a
//! machine that has already run the test suite it can be invoked with no arguments at all.
//!
//! It prints, in this order:
//!
//! * how many functions `.eh_frame_hdr` names, which is the population every figure below is out of;
//! * how many relocations land in an executable segment, because "no relocation-bearing loads" is
//!   only meaningful once that number is known;
//! * the leaf count by grade;
//! * and one line per candidate, with the facts that justify its grade.
//!
//! `--at` prints the full decode of one function, which is what a report quotes.

use std::path::{Path, PathBuf};

use omni_elf::leaf::{self, LeafKind, TextRelocations};
use omni_elf::ElfImage;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path: Option<PathBuf> = None;
    let mut kind: Option<LeafKind> = None;
    let mut min_bytes = 0u64;
    let mut max_bytes = u64::MAX;
    let mut at: Option<u64> = None;
    let mut limit = 40usize;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let next = |i: &mut usize| -> String {
            *i += 1;
            args.get(*i).cloned().unwrap_or_else(|| fail(&format!("{a} needs a value")))
        };
        match a {
            "--kind" => {
                kind = Some(match next(&mut i).as_str() {
                    "pure" => LeafKind::PureRegister,
                    "stack" => LeafKind::StackOnly,
                    "tls" => LeafKind::StackAndThreadPointer,
                    "guarded" => LeafKind::StackGuardProtected,
                    other => fail(&format!("unknown --kind {other}")),
                })
            }
            "--min-bytes" => min_bytes = parse_number(&next(&mut i)),
            "--max-bytes" => max_bytes = parse_number(&next(&mut i)),
            "--at" => at = Some(parse_number(&next(&mut i))),
            "--limit" => limit = parse_number(&next(&mut i)) as usize,
            "-h" | "--help" => {
                println!("{}", USAGE);
                return;
            }
            other if other.starts_with('-') => fail(&format!("unknown option {other}")),
            other => path = Some(PathBuf::from(other)),
        }
        i += 1;
    }

    let path = path.unwrap_or_else(default_library);
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| fail(&format!("cannot read {}: {e}", path.display())));
    let elf = ElfImage::parse(&bytes)
        .unwrap_or_else(|e| fail(&format!("cannot parse {}: {e}", path.display())));

    let bounds = elf
        .eh_frame_functions()
        .unwrap_or_else(|e| fail(&format!("cannot read .eh_frame_hdr: {e}")))
        .unwrap_or_else(|| fail("this object has no PT_GNU_EH_FRAME, so it names no functions"));
    let relocations = TextRelocations::collect(&elf)
        .unwrap_or_else(|e| fail(&format!("cannot decode the relocation tables: {e}")));

    println!("{}", path.display());
    println!("  .eh_frame_hdr names {} functions", bounds.len());
    println!(
        "  {} relocations examined, {} of which land in an executable segment",
        relocations.examined,
        relocations.total()
    );

    if let Some(at) = at {
        let Some(b) = bounds.iter().find(|b| b.contains(at)) else {
            fail(&format!("{at:#x} is inside no function .eh_frame_hdr names"));
        };
        let code = elf
            .slice_at_vaddr("function body", b.start, b.len)
            .unwrap_or_else(|e| fail(&format!("cannot read the body: {e}")));
        let facts = leaf::body_facts(code, b.start, Some(&relocations));
        println!("\n  {:#x}..{:#x} ({} bytes)", b.start, b.end(), b.len);
        println!("  grade: {:?}", facts.kind());
        println!("  facts: {facts:#?}");
        println!("  words:");
        for (n, word) in code.chunks_exact(4).enumerate() {
            let w = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
            println!("    {:#010x}  {w:#010x}", b.start + (n as u64) * 4);
        }
        return;
    }

    let mut by_kind = [0usize; 4];
    let mut shown = 0usize;
    let mut matched = 0usize;
    let mut lines = Vec::new();
    for b in &bounds {
        let Ok(code) = elf.slice_at_vaddr("function body", b.start, b.len) else {
            continue;
        };
        let facts = leaf::body_facts(code, b.start, Some(&relocations));
        let k = facts.kind();
        match k {
            LeafKind::PureRegister => by_kind[0] += 1,
            LeafKind::StackOnly => by_kind[1] += 1,
            LeafKind::StackAndThreadPointer => by_kind[2] += 1,
            LeafKind::StackGuardProtected => by_kind[3] += 1,
            LeafKind::NotALeaf => continue,
        }
        if kind.is_some_and(|want| want != k) || b.len < min_bytes || b.len > max_bytes {
            continue;
        }
        matched += 1;
        if shown < limit {
            shown += 1;
            lines.push(format!(
                "    {:#012x}  {:>5} bytes  {:<22}  mem_bases {:?}  tp_reads {}  guard_loads {}  calls {:x?}",
                b.start,
                b.len,
                format!("{k:?}"),
                facts.memory_bases,
                facts.thread_pointer_reads,
                facts.stack_guard_loads,
                facts.direct_calls,
            ));
        }
    }

    println!(
        "  leaves: {} pure-register, {} stack-only, {} stack + thread pointer, {} stack-guard protected",
        by_kind[0], by_kind[1], by_kind[2], by_kind[3]
    );
    println!("  {matched} match the filter; showing {shown}:");
    for line in lines {
        println!("{line}");
    }
}

const USAGE: &str = "\
leaf-scan [library.so] [--kind pure|stack|tls|guarded] [--min-bytes N] [--max-bytes N]
          [--at 0xVADDR] [--limit N]

Recovers exact function bounds from .eh_frame_hdr and reports which functions are
self-contained enough to execute without the imported-symbol layer.";

fn parse_number(s: &str) -> u64 {
    let t = s.trim();
    let parsed = if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else {
        t.parse::<u64>()
    };
    parsed.unwrap_or_else(|_| fail(&format!("{s:?} is not a number")))
}

/// Where the test fixtures leave the extracted libraries, so the tool runs with no arguments on a
/// machine that has run the suite.
fn default_library() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let cached = root.join("target").join("omni-elf-fixtures").join("libroblox.so");
    if cached.is_file() {
        return cached;
    }
    fail(&format!(
        "no library named, and there is none at {}. Run the omni-elf test suite once to fill the \
         extraction cache, or name a .so on the command line.",
        cached.display()
    ))
}

fn fail(message: &str) -> ! {
    eprintln!("leaf-scan: {message}");
    std::process::exit(2);
}
