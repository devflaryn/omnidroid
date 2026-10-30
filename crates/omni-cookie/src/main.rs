//! `omni-plant-cookie [--new] <Cookies database> <cookie file>`: `tools/plant_cookie.py`, with no
//! Python (the crate's `lib.rs` says what it does). What it did is said on stdout, why it failed on
//! stderr (exit 1; 2 for arguments it cannot read) -- and the cookie's value on neither.
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let new = args.iter().any(|a| a == "--new");
    let rest: Vec<&String> = args.iter().filter(|a| *a != "--new").collect();
    let [db, cookie, ..] = rest.as_slice() else {
        eprintln!("usage: omni-plant-cookie [--new] <Cookies database> <cookie file>");
        return ExitCode::from(2);
    };
    let (db, cookie) = (Path::new(db.as_str()), Path::new(cookie.as_str()));
    // Read as Python's `errors="replace"` reads it: a stray byte is not a reason to refuse.
    let text = match std::fs::read(cookie) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => {
            eprintln!("plant_cookie: {}: {e}", cookie.display());
            return ExitCode::FAILURE;
        }
    };
    let value = omni_cookie::value_of(&text);
    if !omni_cookie::is_session(&value) {
        eprintln!("plant_cookie: the file does not hold a .ROBLOSECURITY value");
        return ExitCode::FAILURE;
    }
    let planted = (if new { omni_cookie::create(db) } else { Ok(()) }).and_then(|()| omni_cookie::plant(db, &value));
    match planted {
        Ok(said) => {
            println!("{said}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("plant_cookie: {e}");
            ExitCode::FAILURE
        }
    }
}
