//! Listings of images, for `loom build --emit bytecode` and for tests.

use std::fmt::Write as _;

use crate::word::{a, b, bc_i32, bc_u32, c, c_i16, opcode, unpack_cast};
use crate::{Form, Image, IntSpec, Op};

/// Every function of `image`, one instruction per line.
pub fn disassemble(image: &Image) -> String {
    let mut out = String::new();
    for (i, f) in image.funcs.iter().enumerate() {
        let entry = if image.entry == Some(i as u32) { "  (entry)" } else { "" };
        let _ =
            writeln!(out, "fn {} #{i}: frame {}, params {}, results {}{entry}", f.name, f.frame, f.params, f.results);
        let code = &image.code[f.entry as usize..(f.entry + f.len) as usize];
        let mut pc = 0;
        while pc < code.len() {
            let w = code[pc];
            let Some(op) = Op::from_u16(opcode(w)) else {
                let _ = writeln!(out, "  {pc:>4}  ??? {w:#018x}");
                pc += 1;
                continue;
            };
            let ext = code.get(pc + 1).copied().unwrap_or(0);
            let _ = writeln!(out, "  {pc:>4}  {:<12}{}", op.name(), operands(image, op, w, ext, pc));
            pc += op.form().words();
        }
    }
    out
}

fn operands(image: &Image, op: Op, w: u64, ext: u64, pc: usize) -> String {
    let at = |off: isize| pc as isize + off;
    let int = |v: u64| IntSpec::unpack(v).map_or_else(|| "?".into(), |s| s.name());
    match op.form() {
        Form::AB => format!("r{}, r{}", a(w), b(w)),
        Form::ABC => format!("r{}, r{}, r{}", a(w), b(w), c(w)),
        Form::AI => format!("r{}, {}", a(w), bc_i32(w)),
        Form::AK => format!("r{}, {ext:#x}", a(w)),
        Form::AS => {
            let text = image.strings.get(bc_u32(w) as usize).map(|s| String::from_utf8_lossy(s).into_owned());
            format!("r{}, {:?}", a(w), text.unwrap_or_default())
        }
        Form::ABX => match op {
            Op::CastT | Op::CastW | Op::CastS => {
                let (to, from) = unpack_cast(ext).map_or(("?".into(), "?".into()), |(t, f)| (t.name(), f.name()));
                format!("r{}, r{} ({from} to {to})", a(w), b(w))
            }
            _ => format!("r{}, r{} ({})", a(w), b(w), int(ext)),
        },
        Form::ABCX => format!("r{}, r{}, r{} ({})", a(w), b(w), c(w), int(ext)),
        Form::ABJ => format!("r{}, r{}, →{}", a(w), b(w), at(c_i16(w))),
        Form::J => format!("→{}", at(bc_i32(w) as isize)),
        Form::AJ => format!("r{}, →{}", a(w), at(bc_i32(w) as isize)),
        Form::Call => {
            let name = image.funcs.get(bc_u32(w) as usize).map_or("?", |f| f.name.as_str());
            format!("{name} at r{}", a(w))
        }
        Form::Native => {
            let name = image.natives.get(b(w)).map_or("?", String::as_str);
            format!("{name} at r{}, #{}", a(w), c(w))
        }
        Form::Ret => format!("r{}, {} (frame {})", a(w), b(w), c(w)),
        Form::K => format!("{}", a(w)),
    }
}
