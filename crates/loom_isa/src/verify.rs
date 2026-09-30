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
//! - `ret` returns as many values as the function says, from its own frame.
//!
//! What a register *means* isn't checked: that's the compiler's job, and the
//! checked build's.

use std::collections::HashSet;
use std::fmt;

use crate::natives::{NativeImm, native};
use crate::word::{a, b, bc_i32, bc_u32, c, c_i16, opcode, unpack_cast};
use crate::{CRASHES, Form, Image, IntSpec, Op};

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
    for name in &image.natives {
        if native(name).is_none() {
            return Err(whole(format!("unknown native `{name}`")));
        }
    }
    for i in 0..image.funcs.len() {
        verify_func(image, i)?;
    }
    Ok(())
}

fn verify_func(image: &Image, index: usize) -> Result<(), VerifyError> {
    let f = &image.funcs[index];
    let code = &image.code[f.entry as usize..(f.entry + f.len) as usize];
    let frame = usize::from(f.frame);
    let err = |pc: usize, what: String| VerifyError { func: f.name.clone(), pc: pc as u32, what };
    if f.params > f.frame || f.results > f.frame {
        return Err(err(0, "parameters or results don't fit the frame".into()));
    }
    // Where instructions start, for checking jumps.
    let mut starts = HashSet::new();
    let mut pc = 0;
    let mut last = None;
    while pc < code.len() {
        let op = Op::from_u16(opcode(code[pc])).ok_or_else(|| err(pc, "unknown opcode".into()))?;
        let words = op.form().words();
        if pc + words > code.len() {
            return Err(err(pc, "the extension word is past the end".into()));
        }
        starts.insert(pc);
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
            if t >= 0 && starts.contains(&(t as usize)) {
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
            Form::J => target(bc_i32(w) as isize)?,
            Form::AJ => {
                reg(a(w))?;
                target(bc_i32(w) as isize)?;
            }
            Form::Call => {
                let callee = image.funcs.get(bc_u32(w) as usize).ok_or_else(|| err(pc, "no such function".into()))?;
                let base = a(w);
                if base + usize::from(callee.params.max(callee.results)) > frame {
                    return Err(err(
                        pc,
                        format!("the call's window doesn't fit the frame (calling `{}`)", callee.name),
                    ));
                }
            }
            Form::Native => {
                let name = image.natives.get(b(w)).ok_or_else(|| err(pc, "no such native".into()))?;
                let sig = native(name).expect("checked in verify");
                if a(w) + usize::from(sig.args.max(sig.results)) > frame {
                    return Err(err(pc, format!("`{name}`'s arguments don't fit the frame")));
                }
                if sig.imm == NativeImm::Fmt {
                    let item = image.fmts.get(c(w)).ok_or_else(|| err(pc, "no such format item".into()))?;
                    if item.kind.regs() != sig.args {
                        return Err(err(pc, format!("`{name}` can't format this kind of value")));
                    }
                }
            }
            Form::Ret => {
                if a(w) + b(w) > frame || b(w) != usize::from(f.results) || c(w) != frame {
                    return Err(err(pc, "`ret` doesn't match the function".into()));
                }
            }
            Form::K => {
                if a(w) >= CRASHES.len() {
                    return Err(err(pc, "no such internal error".into()));
                }
            }
        }
        pc += op.form().words();
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
        let func =
            Func { name: "f".into(), entry: 0, len: code.len() as u32, frame: 4, params: 1, results: 1, lines: vec![] };
        Image {
            funcs: vec![func],
            code,
            strings: vec![b"hi".to_vec()],
            fmts: vec![FmtItem { kind: ValueKind::Bool, debug: false, spec: None }],
            natives: vec!["out.fmt".into()],
            files: vec![],
            entry: Some(0),
        }
    }

    fn ret() -> u64 {
        word(Op::Ret, 0, 1, 4)
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
            word(Op::Call, 2, 0, 0),
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
        rejects(vec![word(Op::Ret, 0, 1, 4), word(Op::Ldk, 0, 0, 0)], "past the end");
        rejects(vec![word(Op::Call, 0, 9, 0), ret()], "no such function");
        rejects(vec![word(Op::Call, 4, 0, 0), ret()], "window");
        rejects(vec![word(Op::CallN, 0, 3, 0), ret()], "no such native");
        rejects(vec![word(Op::CallN, 0, 0, 5), ret()], "no such format item");
        rejects(vec![word_i32(Op::Ldstr, 0, 1), ret()], "no such string");
        rejects(vec![word(Op::Ret, 0, 0, 4)], "`ret` doesn't match");
        rejects(vec![word(Op::Ret, 0, 1, 3)], "`ret` doesn't match");
        rejects(vec![word(Op::AddTG, 0, 0, 0), 99, ret()], "bad integer type");
        rejects(vec![word(Op::CastT, 0, 0, 0), 0, ret()], "bad cast");
        rejects(vec![word(Op::Crash, 7, 0, 0)], "no such internal error");
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
