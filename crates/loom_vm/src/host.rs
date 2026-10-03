//! Natives that reach the machine the program runs on: the environment,
//! files, time and the terminal (`std.env`, `std.fs`, `std.time`,
//! `std.term`), as the Lua host gives them.
//!
//! A sandboxed run (the fixture tests) sees no environment, keeps its files
//! in memory (starting empty), never waits, and has no terminal.

#![allow(unsafe_code)]

use std::io::Write;

use crate::Trap;
use crate::natives::{Ctx, NativeFn};

/// The natives of this module, by name.
pub(crate) fn resolve(name: &str) -> Option<NativeFn> {
    Some(match name {
        "env.get" => env_get,
        "env.args" => env_args,
        "time.sleep" => sleep,
        "process.exit" => exit,
        "term.raw_mode" => raw_mode,
        "term.poll_keys" => poll_keys,
        "fs.append" => fs_append,
        "fs.write_line" => fs_write_line,
        "fs.read" => fs_read,
        _ => return None,
    })
}

/// Text the compiler passes, as a Rust string.
fn text(ctx: &Ctx, ptr: u64, len: u64) -> Result<String, Trap> {
    // SAFETY: every caller passes a `str` the compiler built.
    let bytes = unsafe { ctx.text(ptr, len)? };
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// `env.get(name, dst)`: a `?String` at `dst` (all zero for `none`).
fn env_get(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let name = text(ctx, args[0], args[1])?;
    let value = if ctx.sandbox { None } else { std::env::var(name).ok() };
    match value {
        Some(v) => ctx.new_string(v.as_bytes(), args[2]),
        None => {
            // SAFETY: `dst` is the 24 bytes of a `?String` the compiler gave.
            unsafe { std::ptr::write_bytes(args[2] as *mut u8, 0, 24) };
            Ok(())
        }
    }
}

/// `env.args(dst)`: a `List[String]` at `dst`: the program's name, then
/// its arguments.
fn env_args(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let all = ctx.args.clone();
    let n = all.len() as u64;
    let buf = ctx.heap.alloc(n * 24, 8)?;
    for (i, a) in all.iter().enumerate() {
        ctx.new_string(a.as_bytes(), buf + 24 * i as u64)?;
    }
    // SAFETY: `dst` is the header the compiler gave: three words.
    unsafe {
        let d = args[0] as *mut u64;
        d.write(buf);
        d.add(1).write(n);
        d.add(2).write(n);
    }
    Ok(())
}

/// `process.exit(code)`: unwinds, and the run ends with the code.
fn exit(_: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    Err(Trap::Exit(args[0] as u8))
}

/// `time.sleep(nanos)`.
fn sleep(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    if ctx.sandbox {
        return Ok(());
    }
    ctx.flush();
    // In slices, so Ctrl-C ends a long sleep, which then unwinds.
    let slice = std::time::Duration::from_millis(50);
    let end = std::time::Instant::now().checked_add(std::time::Duration::from_nanos(args[0]));
    loop {
        if ctx.interrupted() {
            return Err(Trap::Interrupt);
        }
        let left = end.map_or(slice, |end| end.saturating_duration_since(std::time::Instant::now()));
        if left.is_zero() {
            return Ok(());
        }
        std::thread::sleep(left.min(slice));
    }
}

/// `term.raw_mode(on)`.
fn raw_mode(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    if ctx.sandbox {
        return Ok(());
    }
    ctx.flush();
    ctx.term.raw_mode(args[0] != 0).map_err(Trap::Unravel)
}

/// `term.poll_keys(keymap)`: a bit for each of its keys held down.
fn poll_keys(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    if ctx.sandbox {
        args[0] = 0;
        return Ok(());
    }
    let keymap = text(ctx, args[0], args[1])?;
    ctx.flush();
    args[0] = u64::from(ctx.term.poll_keys(&keymap).map_err(Trap::Unravel)?);
    Ok(())
}

/// What an I/O failure is, for `IoError`: its case's number (0
/// `NotFound`, 1 `Denied`, 2 `Other`), and 3 for none.
const FINE: u64 = 3;

fn failure(ctx: &mut Ctx, e: &std::io::Error, dst: u64) -> Result<u64, Trap> {
    Ok(match e.kind() {
        std::io::ErrorKind::NotFound => 0,
        std::io::ErrorKind::PermissionDenied => 1,
        _ => {
            ctx.new_string(e.to_string().as_bytes(), dst)?;
            2
        }
    })
}

/// Adds `bytes` to the end of the file at `path` (made if it isn't there),
/// which stays open for the next write (a write is one system call: nothing
/// is buffered, so it's all there if the run is killed).
fn append(ctx: &mut Ctx, path: &str, bytes: &[u8]) -> std::io::Result<()> {
    if ctx.sandbox {
        ctx.files.entry(path.to_string()).or_default().extend_from_slice(bytes);
        return Ok(());
    }
    if !ctx.open_files.contains_key(path) {
        // Not many at once: a program writing to thousands of files would
        // run out of what the system gives.
        if ctx.open_files.len() >= MAX_OPEN {
            ctx.open_files.clear();
        }
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        ctx.open_files.insert(path.to_string(), file);
    }
    match ctx.open_files.get_mut(path) {
        Some(file) => file.write_all(bytes),
        None => Ok(()),
    }
}

/// How many files `append` keeps open.
const MAX_OPEN: usize = 64;

/// `fs.append(path, dst)`: opens (makes) the file to add to: how it went;
/// for `Other`, the message is at `dst`.
fn fs_append(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let path = text(ctx, args[0], args[1])?;
    args[0] = match append(ctx, &path, b"") {
        Ok(()) => FINE,
        Err(e) => failure(ctx, &e, args[2])?,
    };
    Ok(())
}

/// `fs.write_line(path, line, dst)`: adds the line and a newline.
fn fs_write_line(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let path = text(ctx, args[0], args[1])?;
    let mut line = text(ctx, args[2], args[3])?.into_bytes();
    line.push(b'\n');
    args[0] = match append(ctx, &path, &line) {
        Ok(()) => FINE,
        Err(e) => failure(ctx, &e, args[4])?,
    };
    Ok(())
}

/// `fs.read(path, dst)`: the file's bytes, at `dst` (as a `String`, or a
/// `List[u8]`: the same header); or how it failed.
fn fs_read(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    let path = text(ctx, args[0], args[1])?;
    let read = if ctx.sandbox {
        ctx.files.get(&path).cloned().ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    } else {
        std::fs::read(&path)
    };
    args[0] = match read {
        Ok(bytes) => {
            ctx.new_string(&bytes, args[2])?;
            FINE
        }
        Err(e) => failure(ctx, &e, args[2])?,
    };
    Ok(())
}
