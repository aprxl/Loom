//! The bytecode verifier. Treadle reads registers and code without checking
//! bounds, so every function passes through here before it can run
//! (design/vm.md §8.6):
//!
//! - every opcode exists, and every extension word is inside the function;
//! - every register is inside the frame;
//! - every jump lands on the start of an instruction of the same function;
//! - control can't run off the end of a function;
//! - every function, native, string and format item named exists, and each
//!   call's window fits what it passes and gets back;
//! - `ret` returns as many values as the function says, from its own frame;
//! - a call's window leaves room below it for the linkage;
//! - an access at an offset from the frame pointer stays inside the frame,
//!   and an address in the image's data stays inside that;
//! - landing pads cover stretches of the function's own code, don't
//!   overlap, and start at instructions.
//!
//! What a register *means* isn't checked: that's the compiler's job, and the
//! checked build's.

use std::collections::HashSet;
use std::fmt;

use crate::natives::{NativeImm, NativeSig, native};
use crate::word::{a, b, bc_i32, bc_u32, c, c_i16, opcode, unpack_cast};
use crate::{CRASHES, Form, Image, IntSpec, LINK_SLOTS, Op};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyError {
    /// The function's name, or empty for the image as a whole.
    pub func: String,
    /// Where in the function (in words from its start).
    pub pc: u32,
    pub what: String,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.func.is_empty() {
            write!(f, "{}", self.what)
        } else {
            write!(f, "in `{}` at {}: {}", self.func, self.pc, self.what)
        }
    }
}

impl std::error::Error for VerifyError {}

/// Checks that `image` is safe for Treadle to run.
pub fn verify(image: &Image) -> Result<(), VerifyError> {
    let whole = |what: String| VerifyError { func: String::new(), pc: 0, what };
    let mut end = 0u32;
    for f in &image.funcs {
        if f.entry < end || f.len == 0 || u64::from(f.entry) + u64::from(f.len) > image.code.len() as u64 {
            return Err(whole(format!("function `{}` has a bad code range", f.name)));
        }
        end = f.entry + f.len;
    }
    if image.entry.is_some_and(|e| e as usize >= image.funcs.len()) {
        return Err(whole("the entry function doesn't exist".into()));
    }
    let bytes = image.data.len() as u64 * 8;
    for &r in &image.relocs {
        match image.data.get(r as usize) {
            None => return Err(whole(format!("relocation of word {r}, which is past the data"))),
            Some(&to) if to > bytes => {
                return Err(whole(format!("word {r} points at data+{to}, past the data's {bytes} bytes")));
            }
            Some(_) => {}
        }
    }
    // Each native's signature, found once: a call names it by number.
    let mut sigs = Vec::with_capacity(image.natives.len());
    for name in &image.natives {
        match native(name) {
            Some(sig) => sigs.push(sig),
            None => return Err(whole(format!("unknown native `{name}`"))),
        }
    }
    let mut imports = HashSet::new();
    for import in &image.host_imports {
        if import.name.is_empty() || !imports.insert(import.name.as_str()) {
            return Err(whole(format!("empty or duplicate host import `{}`", import.name)));
        }
    }
    for i in 0..image.funcs.len() {
        verify_func(image, i, &sigs)?;
    }
    Ok(())
}

fn verify_func(image: &Image, index: usize, sigs: &[&NativeSig]) -> Result<(), VerifyError> {
    let f = &image.funcs[index];
    let code = &image.code[f.entry as usize..(f.entry + f.len) as usize];
    let frame = usize::from(f.frame);
    let err = |pc: usize, what: String| VerifyError { func: f.name.clone(), pc: pc as u32, what };
    if f.params > f.frame || f.results > f.frame {
        return Err(err(0, "parameters or results don't fit the frame".into()));
    }
    // Where instructions start, for checking jumps.
    let mut starts = vec![false; code.len() + 1];
    let mut pc = 0;
    let mut last = None;
    while pc < code.len() {
        let op = Op::from_u16(opcode(code[pc])).ok_or_else(|| err(pc, "unknown opcode".into()))?;
        let words = op.form().words();
        if pc + words > code.len() {
            return Err(err(pc, "the extension word is past the end".into()));
        }
        starts[pc] = true;
        last = Some(op);
        pc += words;
    }
    if !last.is_some_and(Op::ends_flow) {
        return Err(err(code.len(), "control runs off the end".into()));
    }
    let mut pc = 0;
    while pc < code.len() {
        let w = code[pc];
        let op = Op::from_u16(opcode(w)).expect("decoded above");
        let ext = code.get(pc + 1).copied().unwrap_or(0);
        let reg =
            |r: usize| if r < frame { Ok(()) } else { Err(err(pc, format!("register r{r} is outside the frame"))) };
        let target = |off: isize| {
            let t = pc as isize + off;
            if t >= 0 && starts.get(t as usize).copied().unwrap_or(false) {
                Ok(())
            } else {
                Err(err(pc, format!("jump to {t}, which isn't an instruction")))
            }
        };
        let spec = |v: u64| IntSpec::unpack(v).map(|_| ()).ok_or_else(|| err(pc, "bad integer type".into()));
        match op.form() {
            Form::AB => {
                reg(a(w))?;
                reg(b(w))?;
            }
            Form::ABC => {
                reg(a(w))?;
                reg(b(w))?;
                reg(c(w))?;
            }
            Form::AI | Form::AK => reg(a(w))?,
            Form::AS => {
                reg(a(w))?;
                if bc_u32(w) as usize >= image.strings.len() {
                    return Err(err(pc, "no such string".into()));
                }
            }
            Form::ABX => {
                reg(a(w))?;
                reg(b(w))?;
                match op {
                    Op::CastT | Op::CastW | Op::CastS => {
                        unpack_cast(ext).ok_or_else(|| err(pc, "bad cast".into()))?;
                    }
                    _ => spec(ext)?,
                }
            }
            Form::ABCX => {
                reg(a(w))?;
                reg(b(w))?;
                reg(c(w))?;
                spec(ext)?;
            }
            Form::ABJ => {
                reg(a(w))?;
                reg(b(w))?;
                target(c_i16(w))?;
            }
            Form::ABK => {
                reg(a(w))?;
                reg(b(w))?;
                let k = c_i16(w);
                let bits = match op {
                    Op::ShlKI8 | Op::ShlKU8 => 8,
                    Op::ShlKI16 | Op::ShlKU16 => 16,
                    Op::ShlKI32 | Op::ShlKU32 => 32,
                    Op::ShlKI64 | Op::ShlKU64 | Op::ShrKS | Op::ShrKU => 64,
                    _ => 0,
                };
                if bits > 0 && !(0..bits).contains(&k) {
                    return Err(err(pc, "a shift by a constant past the width".into()));
                }
                if matches!(op, Op::DivKS | Op::DivKU | Op::RemKS | Op::RemKU) && k <= 0 {
                    return Err(err(pc, "a division by a constant that isn't above zero".into()));
                }
            }
            Form::ABU => {
                reg(a(w))?;
                reg(b(w))?;
            }
            Form::AKJ => {
                reg(a(w))?;
                target(c_i16(w))?;
            }
            Form::J => target(bc_i32(w) as isize)?,
            Form::AJ => {
                reg(a(w))?;
                target(bc_i32(w) as isize)?;
            }
            Form::Call => {
                let callee = image.funcs.get(bc_u32(w) as usize).ok_or_else(|| err(pc, "no such function".into()))?;
                let base = a(w);
                if base < LINK_SLOTS {
                    return Err(err(pc, "the call's window leaves no room for its linkage".into()));
                }
                if base + usize::from(callee.params.max(callee.results)) > frame {
                    return Err(err(
                        pc,
                        format!("the call's window doesn't fit the frame (calling `{}`)", callee.name),
                    ));
                }
            }
            Form::Native => {
                let name = image.natives.get(b(w)).ok_or_else(|| err(pc, "no such native".into()))?;
                let sig = sigs[b(w)];
                let window = if sig.imm == NativeImm::HostImport {
                    let import = image.host_imports.get(c(w)).ok_or_else(|| err(pc, "no such host import".into()))?;
                    import.args.max(import.results)
                } else {
                    sig.args.max(sig.results)
                };
                if a(w) + usize::from(window) > frame {
                    return Err(err(pc, format!("`{name}`'s arguments don't fit the frame")));
                }
                if sig.imm == NativeImm::Fmt {
                    let item = image.fmts.get(c(w)).ok_or_else(|| err(pc, "no such format item".into()))?;
                    // `sink.pad` pads text that's already written: any kind.
                    if name != "sink.pad" && item.kind.regs() + sig.lead != sig.args {
                        return Err(err(pc, format!("`{name}` can't format this kind of value")));
                    }
                }
            }
            Form::Ret => {
                if a(w) + b(w) > frame || b(w) != usize::from(f.results) || c(w) != 0 {
                    return Err(err(pc, "`ret` doesn't match the function".into()));
                }
            }
            Form::K => {
                if a(w) >= CRASHES.len() {
                    return Err(err(pc, "no such internal error".into()));
                }
            }
            Form::ABO | Form::ABZ => {
                reg(a(w))?;
                reg(b(w))?;
            }
            Form::AZ => reg(a(w))?,
            Form::ABCZ => {
                reg(a(w))?;
                reg(b(w))?;
                reg(c(w))?;
            }
            Form::RE | Form::RX => {
                reg(a(w))?;
                reg(b(w))?;
                reg(c(w))?;
            }
            Form::RXC => {
                reg(a(w))?;
                reg(b(w))?;
                reg(c(w))?;
                reg((ext >> 48) as usize)?;
            }
            Form::FB => {
                // The list's header: three words, inside the frame, on a word.
                if (ext >> 32) + 24 > frame as u64 * 8 {
                    return Err(err(pc, format!("fp+{} is outside the frame", ext >> 32)));
                }
                if (ext >> 32) % 8 != 0 {
                    return Err(err(pc, format!("the list header at fp+{} isn't on a word", ext >> 32)));
                }
                if a(w) > 12 {
                    return Err(err(pc, "an alignment past 4096".into()));
                }
            }
            Form::RB => {
                reg(a(w))?;
                if b(w) > 12 {
                    return Err(err(pc, "an alignment past 4096".into()));
                }
            }
            Form::FE | Form::ABFZ => {
                reg(a(w))?;
                reg(b(w))?;
                // The list's header: two words, inside the frame, on a word.
                if (ext >> 32) + 16 > frame as u64 * 8 {
                    return Err(err(pc, format!("fp+{} is outside the frame", ext >> 32)));
                }
                if (ext >> 32) % 8 != 0 {
                    return Err(err(pc, format!("the list header at fp+{} isn't on a word", ext >> 32)));
                }
            }
            Form::AF => {
                reg(a(w))?;
                let width = u64::from(op.access_bytes().unwrap_or(0));
                if u64::from(bc_u32(w)) + width > frame as u64 * 8 {
                    return Err(err(pc, format!("fp+{} is outside the frame", bc_u32(w))));
                }
            }
            Form::AG => {
                reg(a(w))?;
                if u64::from(bc_u32(w)) > image.data.len() as u64 * 8 {
                    return Err(err(pc, format!("data+{} is outside the image's data", bc_u32(w))));
                }
            }
            Form::AFn => {
                reg(a(w))?;
                if bc_u32(w) as usize >= image.funcs.len() {
                    return Err(err(pc, "no such function".into()));
                }
            }
            Form::CallR => {
                let base = a(w);
                reg(b(w))?;
                if base < LINK_SLOTS {
                    return Err(err(pc, "the call's window leaves no room for its linkage".into()));
                }
                let (params, results) = (c(w) >> 8, c(w) & 0xFF);
                if base + params.max(results) > frame {
                    return Err(err(pc, "the call's window doesn't fit the frame".into()));
                }
            }
            Form::Z => {}
        }
        pc += op.form().words();
    }
    let mut last_end = 0;
    for p in &f.pads {
        let inside = p.start < p.end && p.end as usize <= code.len() && p.start >= last_end;
        if !inside
            || !starts.get(p.pad as usize).copied().unwrap_or(false)
            || !starts.get(p.start as usize).copied().unwrap_or(false)
        {
            return Err(err(p.start as usize, format!("bad landing pad {}..{} → {}", p.start, p.end, p.pad)));
        }
        last_end = p.end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::word::{word, word_i32};
    use crate::{FmtItem, Func, ValueKind};

    /// An image of one function with `code`, a frame of 4 registers and
    /// one result.
    fn image(code: Vec<u64>) -> Image {
        let func = Func {
            name: "f".into(),
            entry: 0,
            len: code.len() as u32,
            frame: 4,
            params: 1,
            results: 1,
            lines: vec![],
            pads: vec![],
            sites: vec![],
        };
        Image {
            funcs: vec![func],
            code,
            strings: vec![b"hi".to_vec()],
            fmts: vec![FmtItem { kind: ValueKind::Bool, debug: false, spec: None }],
            natives: vec!["out.fmt".into()],
            host_imports: vec![],
            files: vec![],
            data: vec![0; 2],
            relocs: vec![],
            entry: Some(0),
        }
    }

    #[test]
    fn relocations_stay_inside_the_data() {
        let mut img = image(vec![ret()]);
        img.relocs = vec![1];
        assert!(verify(&img).is_ok());
        // To the end of the data is a pointer to nothing past it: fine.
        img.data[1] = 16;
        assert!(verify(&img).is_ok());
        img.data[1] = 17;
        assert!(verify(&img).is_err());
        img.data[1] = 0;
        img.relocs = vec![2];
        assert!(verify(&img).is_err());
    }

    fn ret() -> u64 {
        word(Op::Ret, 0, 1, 0)
    }

    fn rejects(code: Vec<u64>, what: &str) {
        let Err(e) = verify(&image(code)) else { panic!("accepted, expected `{what}`") };
        assert!(e.what.contains(what), "{e}");
    }

    #[test]
    fn accepts_a_good_function() {
        let code = vec![
            word(Op::AddTI32, 1, 0, 0),
            word(Op::JLtS, 0, 1, 2),
            word(Op::Mov, 0, 1, 0),
            word(Op::CallN, 2, 0, 0),
            word(Op::Ldk, 3, 0, 0),
            42,
            word(Op::Call, 3, 0, 0),
            ret(),
        ];
        verify(&image(code)).expect("valid");
    }

    #[test]
    fn rejects_broken_functions() {
        rejects(vec![0xFFFF, ret()], "unknown opcode");
        rejects(vec![word(Op::Mov, 4, 0, 0), ret()], "register r4");
        rejects(vec![word(Op::AddTI32, 0, 0, 9), ret()], "register r9");
        rejects(vec![word(Op::Mov, 0, 1, 0)], "runs off the end");
        rejects(vec![word(Op::Mov, 0, 0, 0), word_i32(Op::Jz, 0, -1)], "runs off the end");
        rejects(vec![word_i32(Op::Jmp, 0, 5), ret()], "isn't an instruction");
        rejects(vec![word(Op::Ldk, 0, 0, 0), 7, word_i32(Op::Jmp, 0, -1), ret()], "isn't an instruction");
        rejects(vec![word(Op::Ret, 0, 1, 0), word(Op::Ldk, 0, 0, 0)], "past the end");
        rejects(vec![word(Op::Call, 0, 9, 0), ret()], "no such function");
        rejects(vec![word(Op::Call, 4, 0, 0), ret()], "window");
        rejects(vec![word(Op::Call, 2, 0, 0), ret()], "no room for its linkage");
        rejects(vec![word(Op::Call, 2, 0, 0), ret()], "no room for its linkage");
        rejects(vec![word(Op::CallN, 0, 3, 0), ret()], "no such native");
        rejects(vec![word(Op::CallN, 0, 0, 5), ret()], "no such format item");
        rejects(vec![word_i32(Op::Ldstr, 0, 1), ret()], "no such string");
        rejects(vec![word(Op::Ret, 0, 0, 0)], "`ret` doesn't match");
        rejects(vec![word(Op::Ret, 0, 1, 3)], "`ret` doesn't match");
        rejects(vec![word(Op::Ret, 3, 2, 0)], "`ret` doesn't match");
        rejects(vec![word(Op::AddTG, 0, 0, 0), 99, ret()], "bad integer type");
        rejects(vec![word(Op::CastT, 0, 0, 0), 0, ret()], "bad cast");
        rejects(vec![word(Op::Crash, 7, 0, 0)], "no such internal error");
        rejects(vec![word(Op::Ld64, 4, 0, 0), ret()], "register r4");
        rejects(vec![word(Op::St8, 0, 9, 0), ret()], "register r9");
        rejects(vec![word_i32(Op::LdF64, 0, 25), ret()], "outside the frame");
        rejects(vec![word_i32(Op::StF8, 0, 32), ret()], "outside the frame");
        rejects(vec![word_i32(Op::Lea, 0, 33), ret()], "outside the frame");
        rejects(vec![word_i32(Op::Gaddr, 0, 17), ret()], "outside the image's data");
        rejects(vec![word(Op::Copy, 0, 5, 0), 8, ret()], "register r5");
        rejects(vec![word(Op::Copy, 0, 1, 0)], "past the end");
        rejects(vec![word_i32(Op::Ldfn, 0, 3), ret()], "no such function");
        rejects(vec![word(Op::CallR, 2, 0, 0x0101), ret()], "no room for its linkage");
        rejects(vec![word(Op::CallR, 3, 0, 0x0200), ret()], "window");
        rejects(vec![word(Op::Resume, 0, 0, 0), word(Op::Idx, 0, 1, 7), 8, ret()], "register r7");
    }

    #[test]
    fn list_headers_in_the_frame_are_on_a_word() {
        // A header of three words at fp+8 fills a 4-register frame; at fp+4 it
        // is inside the frame but not on a word.
        let mut ok = vec![word(Op::FreeBufF, 3, 0, 0), 8 << 32 | 8, ret()];
        verify(&image(ok.clone())).expect("aligned");
        ok[1] = 4 << 32 | 8;
        rejects(ok, "isn't on a word");
        // The two-word form: fp+16 and fp+12 in a 4-register frame (32 bytes).
        let mut elem = vec![word(Op::LdElF64, 0, 1, 0), 16 << 32, ret()];
        verify(&image(elem.clone())).expect("aligned");
        elem[1] = 12 << 32;
        rejects(elem, "isn't on a word");
    }

    #[test]
    fn checks_landing_pads() {
        let code = vec![word(Op::AddTI32, 0, 0, 0), ret(), word(Op::Resume, 0, 0, 0)];
        let mut img = image(code);
        img.funcs[0].pads = vec![crate::image::Pad { start: 0, end: 1, pad: 2 }];
        verify(&img).expect("a good pad");
        img.funcs[0].pads = vec![crate::image::Pad { start: 0, end: 4, pad: 2 }];
        assert!(verify(&img).is_err(), "past the end");
        img.funcs[0].pads = vec![crate::image::Pad { start: 0, end: 1, pad: 7 }];
        assert!(verify(&img).is_err(), "pad outside");
        img.funcs[0].pads =
            vec![crate::image::Pad { start: 0, end: 2, pad: 2 }, crate::image::Pad { start: 1, end: 2, pad: 2 }];
        assert!(verify(&img).is_err(), "overlapping");
        assert_eq!(img.funcs[0].pad_at(1), Some(2));
    }

    #[test]
    fn rejects_broken_images() {
        let mut bad = image(vec![ret()]);
        bad.natives.push("launch.missiles".into());
        assert!(verify(&bad).expect_err("unknown native").what.contains("unknown native"));
        let mut bad = image(vec![ret()]);
        bad.funcs[0].len = 2;
        assert!(verify(&bad).is_err());
        let mut bad = image(vec![ret()]);
        bad.entry = Some(1);
        assert!(verify(&bad).is_err());
    }
}
