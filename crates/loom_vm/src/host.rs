//! Natives that reach the machine the program runs on: the environment,
//! files, time and the terminal (`std.env`, `std.fs`, `std.time`,
//! `std.term`), as the Lua host gives them.
//!
//! A sandboxed run (the fixture tests) sees no environment, keeps its files
//! in memory (starting empty), never waits, and has no terminal. Compile-time
//! code is refused all of them (`Config::refuse_host`): each native starts
//! with `host_check`.

#![allow(unsafe_code)]

use std::io::Write;

use crate::Trap;
use crate::natives::{Ctx, NativeFn};

/// The natives of this module, by name.
pub(crate) fn resolve(name: &str) -> Option<NativeFn> {
    Some(match name {
        "env.get" => env_get,
        "env.args" => env_args,
        "time.now" => now,
        "io.write_err" => write_err,
        "process.exit" => exit,
        "term.raw_mode" => raw_mode,
        "term.poll_keys" => poll_keys,
        "fs.append" => fs_append,
        "fs.write_line" => fs_write_line,
        "fs.read" => fs_read,
        "fs.write" => fs_write,
        "fs.exists" => fs_exists,
        "fs.kind" => fs_kind,
        "fs.size" => fs_size,
        "time.unix" => unix_nanos,
        "process.id" => process_id,
        "env.cwd" => env_cwd,
        "term.info" => term_info,
        "fs.dir" => fs_dir,
        "fs.move" => fs_move,
        "fs.list" => fs_list,
        "fs.remove" => fs_remove,
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
    ctx.host_check("env.get")?;
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
    ctx.host_check("env.args")?;
    let all = ctx.args.clone();
    let n = all.len() as u64;
    let buf = ctx.heap.alloc(n * 24, 8)?;
    for (i, a) in all.iter().enumerate() {
        ctx.new_string(a.as_bytes(), buf + 24 * i as u64)?;
    }
    // SAFETY: `dst` is the header the compiler gave: three words.
    unsafe {
        let d = args[0] as *mut u64;
        d.write_unaligned(buf);
        d.add(1).write_unaligned(n);
        d.add(2).write_unaligned(n);
    }
    Ok(())
}

/// `time.now() -> nanoseconds`: a clock that only goes forward, counted
/// from the first time anything asks. Compile-time code can't read it.
fn now(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("time.now")?;
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    args[0] = START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64;
    Ok(())
}

/// `io.read_line`'s result at `dst`: `raw` (a line or nothing) as a `?String`.
pub(crate) fn line_to(ctx: &mut Ctx, raw: &[u8], dst: u64) -> Result<(), Trap> {
    // Bytes that aren't UTF-8 become U+FFFD: a `String` is always text.
    let line = String::from_utf8_lossy(raw).into_owned();
    if raw.is_empty() {
        // SAFETY: `dst` is the 24 bytes of a `?String` the compiler gave.
        unsafe { std::ptr::write_bytes(dst as *mut u8, 0, 24) };
        return Ok(());
    }
    let trimmed = line.strip_suffix('\n').unwrap_or(&line);
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
    ctx.new_string(trimmed.as_bytes(), dst)
}

/// `io.read_all`'s result at `dst`.
pub(crate) fn all_to(ctx: &mut Ctx, raw: &[u8], dst: u64) -> Result<(), Trap> {
    ctx.new_string(String::from_utf8_lossy(raw).as_bytes(), dst)
}

/// `io.write_err(text)`: standard error, after what is waiting for
/// standard output, so the two keep their order on a terminal.
fn write_err(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("io.write_err")?;
    // SAFETY: the compiler passes text it built; writing doesn't touch it.
    let bytes: Vec<u8> = unsafe { ctx.text(args[0], args[1])? }.to_vec();
    ctx.flush();
    if !ctx.sandbox {
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(&bytes);
        let _ = err.flush();
    }
    Ok(())
}

/// `process.exit(code)`: unwinds, and the run ends with the code.
fn exit(_: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    Err(Trap::Exit(args[0] as u8))
}

/// `term.raw_mode(on)`.
fn raw_mode(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("term.raw_mode")?;
    if ctx.sandbox {
        return Ok(());
    }
    ctx.flush();
    ctx.term.raw_mode(args[0] != 0).map_err(Trap::Unravel)
}

/// `term.poll_keys(keymap)`: a bit for each of its keys held down.
fn poll_keys(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("term.poll_keys")?;
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
    ctx.host_check("fs.append")?;
    let path = text(ctx, args[0], args[1])?;
    args[0] = match append(ctx, &path, b"") {
        Ok(()) => FINE,
        Err(e) => failure(ctx, &e, args[2])?,
    };
    Ok(())
}

/// `fs.write_line(path, line, dst)`: adds the line and a newline.
fn fs_write_line(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("fs.write_line")?;
    let path = text(ctx, args[0], args[1])?;
    let mut line = text(ctx, args[2], args[3])?.into_bytes();
    line.push(b'\n');
    args[0] = match append(ctx, &path, &line) {
        Ok(()) => FINE,
        Err(e) => failure(ctx, &e, args[4])?,
    };
    Ok(())
}

/// `fs.write(path, text, dst)`: the file's contents become `text`, made if
/// it isn't there.
fn fs_write(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("fs.write")?;
    let path = text(ctx, args[0], args[1])?;
    // SAFETY: as for `write_err`.
    let bytes: Vec<u8> = unsafe { ctx.text(args[2], args[3])? }.to_vec();
    ctx.open_files.remove(&path);
    let done = if ctx.sandbox {
        ctx.files.insert(path, bytes);
        Ok(())
    } else {
        std::fs::write(&path, &bytes)
    };
    args[0] = match done {
        Ok(()) => FINE,
        Err(e) => failure(ctx, &e, args[4])?,
    };
    Ok(())
}

/// `fs.exists(path)`: whether there is a file or a directory there.
fn fs_exists(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("fs.exists")?;
    let path = text(ctx, args[0], args[1])?;
    args[0] = u64::from(if ctx.sandbox { ctx.files.contains_key(&path) } else { std::path::Path::new(&path).exists() });
    Ok(())
}

/// `fs.dir(path, dst)`: makes the directory and the ones above it.
fn fs_dir(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("fs.dir")?;
    let path = text(ctx, args[0], args[1])?;
    let done = if ctx.sandbox { Ok(()) } else { std::fs::create_dir_all(&path) };
    args[0] = match done {
        Ok(()) => FINE,
        Err(e) => failure(ctx, &e, args[2])?,
    };
    Ok(())
}

/// `fs.move(from, to, dst)` with `imm` 0 renames the file, 1 copies it.
fn fs_move(ctx: &mut Ctx, args: &mut [u64], imm: u16) -> Result<(), Trap> {
    ctx.host_check("fs.move")?;
    let from = text(ctx, args[0], args[1])?;
    let to = text(ctx, args[2], args[3])?;
    ctx.open_files.remove(&from);
    ctx.open_files.remove(&to);
    let done = if ctx.sandbox {
        match ctx.files.get(&from).cloned() {
            Some(bytes) => {
                if imm == 0 {
                    ctx.files.remove(&from);
                }
                ctx.files.insert(to, bytes);
                Ok(())
            }
            None => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        }
    } else if imm == 0 {
        std::fs::rename(&from, &to)
    } else {
        std::fs::copy(&from, &to).map(|_| ())
    };
    args[0] = match done {
        Ok(()) => FINE,
        Err(e) => failure(ctx, &e, args[4])?,
    };
    Ok(())
}

/// `fs.list(path, dst)`: the names in the directory, in order, as a
/// `List[String]` at `dst`.
fn fs_list(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("fs.list")?;
    let path = text(ctx, args[0], args[1])?;
    let names: std::io::Result<Vec<String>> = if ctx.sandbox {
        let prefix = if path.is_empty() || path.ends_with('/') { path.clone() } else { format!("{path}/") };
        let mut names: Vec<String> = ctx
            .files
            .keys()
            .filter_map(|k| k.strip_prefix(prefix.as_str()))
            .map(|rest| rest.split('/').next().unwrap_or(rest).to_string())
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    } else {
        std::fs::read_dir(&path).and_then(|entries| {
            let mut names = Vec::new();
            for entry in entries {
                names.push(entry?.file_name().to_string_lossy().into_owned());
            }
            names.sort();
            Ok(names)
        })
    };
    match names {
        Ok(names) => {
            let n = names.len() as u64;
            let buf = ctx.heap.alloc((n * 24).max(8), 8)?;
            for (i, name) in names.iter().enumerate() {
                ctx.new_string(name.as_bytes(), buf + 24 * i as u64)?;
            }
            // SAFETY: `dst` is the header of a `List[String]` the compiler gave.
            unsafe {
                let d = args[2] as *mut u64;
                d.write_unaligned(buf);
                d.add(1).write_unaligned(n);
                d.add(2).write_unaligned(n);
            }
            args[0] = FINE;
        }
        Err(e) => args[0] = failure(ctx, &e, args[2])?,
    }
    Ok(())
}

/// `fs.kind(path)`: 0 when there is nothing there, 1 for a file, 2 for a
/// directory.
fn fs_kind(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("fs.kind")?;
    let path = text(ctx, args[0], args[1])?;
    args[0] = if ctx.sandbox {
        u64::from(ctx.files.contains_key(&path))
    } else {
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => 2,
            Ok(_) => 1,
            Err(_) => 0,
        }
    };
    Ok(())
}

/// `fs.size(path)`: the file's length in bytes, 0 when it can't be read.
fn fs_size(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("fs.size")?;
    let path = text(ctx, args[0], args[1])?;
    args[0] = if ctx.sandbox {
        ctx.files.get(&path).map_or(0, |f| f.len() as u64)
    } else {
        std::fs::metadata(&path).map_or(0, |m| m.len())
    };
    Ok(())
}

/// `time.unix()`: nanoseconds since 1970 on the system clock.
fn unix_nanos(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("time.unix")?;
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    args[0] = since.map_or(0, |d| d.as_nanos() as u64);
    Ok(())
}

/// `process.id()`.
fn process_id(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("process.id")?;
    args[0] = if ctx.sandbox { 1 } else { u64::from(std::process::id()) };
    Ok(())
}

/// `env.cwd(dst)`: the working directory, as a `?String` at `dst`.
fn env_cwd(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("env.cwd")?;
    let dir = if ctx.sandbox {
        Some("/".to_string())
    } else {
        std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned())
    };
    match dir {
        Some(d) => ctx.new_string(d.as_bytes(), args[0]),
        None => {
            // SAFETY: `dst` is the 24 bytes of a `?String` the compiler gave.
            unsafe { std::ptr::write_bytes(args[0] as *mut u8, 0, 24) };
            Ok(())
        }
    }
}

/// `term.info()` with `imm`: 0 the columns, 1 the rows (80 and 24 when
/// there is no terminal), 2 whether standard output is a terminal.
fn term_info(ctx: &mut Ctx, args: &mut [u64], imm: u16) -> Result<(), Trap> {
    ctx.host_check("term.info")?;
    args[0] = if ctx.sandbox {
        [80, 24, 0][usize::from(imm.min(2))]
    } else {
        match imm {
            0 | 1 => {
                let (cols, rows) = loom_sys::term::size();
                u64::from(if imm == 0 { cols } else { rows })
            }
            _ => u64::from(std::io::IsTerminal::is_terminal(&std::io::stdout())),
        }
    };
    Ok(())
}

/// `fs.remove(path, dst)`: deletes the file.
fn fs_remove(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("fs.remove")?;
    let path = text(ctx, args[0], args[1])?;
    ctx.open_files.remove(&path);
    let done = if ctx.sandbox {
        ctx.files.remove(&path).map(|_| ()).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    } else {
        std::fs::remove_file(&path)
    };
    args[0] = match done {
        Ok(()) => FINE,
        Err(e) => failure(ctx, &e, args[2])?,
    };
    Ok(())
}

/// `fs.read(path, dst)`: the file's bytes, at `dst` (as a `String`, or a
/// `List[u8]`: the same header); or how it failed.
fn fs_read(ctx: &mut Ctx, args: &mut [u64], _: u16) -> Result<(), Trap> {
    ctx.host_check("fs.read")?;
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
