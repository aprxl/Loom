# Review of Treadle's run time, after V3 (2026-10-10)

A read-through of `loom_vm` (the dispatch loop, stack, heap, ints, natives,
text methods, formatting, host), `loom_isa` (words, ops, images), the parts of
`codegen.rs` and `heddle/build/{map,text}.rs` that decide what the loop runs,
and the build configuration. **No code was changed and nothing was
benchmarked**: the speed gate (2.42× against 2.5×) is open, and this is a
list of where the time probably goes and what to do about it.

**Evidence tags.** *Verified* means seen in the machine code
(`cargo rustc --release -p loom_vm --lib -- --emit asm`, the same flags as
`.cargo/config.toml`) or measured by a throwaway test that was deleted.
*Read* means it comes from reading the source and wasn't confirmed lower
down. *Unsure* means a hypothesis to check first.

Each item gives where it is, the mechanism, what it saves, the effort and the
risk. Items are ranked by how much of the transcript's evidence (dispatch
counts per benchmark, the fannkuch/maps/word_freq listings) they touch, not by
a measurement.

---

## 0. Ranking

**Status (2026-10-11).** Done and kept, each by an interleaved A/B: the
release profile and the `pc` fix (+8.5% together), the fused
increment-and-branch (§2.1, +2.0%), intrusive free lists (§4, +1.6 to +3%
on the allocation-heavy ones), formatting straight into the sink (§3.1,
maps -18%), and `sqrt`/`abs` ops (§2.3, nbody -4%). Tried and dropped:
the 8-byte natives ABI and the unchecked natives table (§1.4, -1.7%),
`ticks` as a local (§1.3, 0%), casts without an extension word (§2.2,
+0.2%), `target-cpu=native` (§1.1, -1.8%), and branchy field ordering
(fewer instructions, the same time). Not tried: §3.2, §3.3, §5, §6.

| # | Item | Hits | Effort | Risk |
|---|---|---|---|---|
| 1 | `[profile.release]` is the default: no LTO, 16 codegen units, unwinding panics (§1.1) | everything | S | low |
| 2 | `pc` lives in memory in the dispatch loop (§1.2) | every instruction | M to L | medium |
| 3 | `forrange`: a fused add-and-branch for counted loops (§2.1) | sieve, fannkuch, mandelbrot, nbody, spectral_norm, chip8, sort | M | low |
| 4 | Casts: ops by target and source signedness, no extension word, no call (§2.2) | any program indexing with `as usize` / `as i32` | M | low |
| 5 | Natives return 8 bytes, not a 32-byte `Result`, and the table isn't bounds-checked (§1.4) | every `calln`: word_freq, maps, lexer, binary_trees | S | low |
| 6 | Formatting writes into the sink: no `String` or `Vec` per hole (§3.1) | maps, templates, `print` | S | low |
| 7 | Heap: intrusive free lists, a fixed array of heads, size classes decided at compile time (§4) | binary_trees, strings | M | medium |
| 8 | Sinks with an implicit handle (§3.2) | every template: maps, word_freq | M | medium |
| 9 | Ops for `sqrt`, `abs`, `floor`… (§2.3) | nbody, spectral_norm, mandelbrot | S | low |
| 10 | Maps: `copy` for `compact`'s moves, `reindex` as a native, hash tags in the index (§5) | maps, word_freq | M | medium |
| 11 | `ticks` as a flag read, `Ldstr` pre-resolved, `Ret`'s host check (§1.3, §1.5) | calls, loops | S | low |
| 12 | Text natives: `memchr`/`memmem`, fewer copies, SIMD (§3.3) | lexer, split/find programs | M | low |
| 13 | A C core with `musttail` (tail-call threading), PGO (§6) | everything | L | high |

---

## 1. The loop and how it's built

### 1.1 `[profile.release]` isn't set. *Verified.*

*Done (2026-10-10): `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`.
Alone it moved the geomean by about 1% (noise, mixed per benchmark); the
binary takes 76 s to build instead of 24 s. `target-cpu` was not tried.*

`Cargo.toml` has `[profile.dev.*]` sections and no `[profile.release]`, so
the `loom` binary is built with cargo's defaults: `opt-level = 3`, **no LTO**,
**`codegen-units = 16`**, **`panic = "unwind"`**, no `target-cpu`.

- *LTO and one codegen unit.* `loom_isa`'s accessors are `inline(always)` and
  fine, but `loom_isa::word::unpack_cast` is a *call* in the loop's machine
  code (three call sites: `CastT`, `CastW`, `CastS`). `lto =
  "fat"` (or `"thin"`) and `codegen-units = 1` let it inline and give
  LLVM the whole program for the natives' fast paths.
- *`panic = "abort"`.* The loop's machine code carries an SEH personality
  (`.seh_handler __CxxFrameHandler3`) and a state slot, because natives can
  unwind. Nothing relies on catching a panic (mlua's callbacks would abort
  instead of turning a panic into a Lua error: a bug either way). The tests
  keep unwinding, as cargo always builds those with it. Dropping the landing
  pads shrinks the loop and the natives; it did **not** by itself move `pc`
  into a register (§1.2, *verified* with `-C panic=abort`).
- *`target-cpu`.* The binary is a developer tool built on the machine it
  runs on: `-C target-cpu=native` (or `x86-64-v3`) puts `lzcnt`/`tzcnt`/
  `popcnt`/`bmi` where `leading_zeros` etc. are (`class_of`, `int.bits`,
  `text_hash`'s tails). It makes the binary non-portable, so keep it out of
  anything that ships.
- Effort S; risk: compile time of `loom` goes up (one codegen unit), the
  `-tail-dup-*` and `-align-all-nofallthru-blocks` flags in
  `.cargo/config.toml` were tuned on 16 units (design §9.1 says layout moved
  speed by 30%): this is the change most likely to move the benchmarks
  *either* way, so it wants the full A/B the design asks of V8 (3% to keep).

### 1.2 `pc` is a stack slot, not a register. *Verified.*

*Fixed (2026-10-10), and the cause wasn't register pressure: the `Resume`
arm's `unwrap_or_else(|| fault!(…, here!()))` closure captured `pc` by
reference, so `%pc` was an `alloca` the whole function (`--emit llvm-ir`
showed it; the closure stored `ptr %pc` into its capture struct). Reading
`here!()` into a local first removed the alloca, and `pc` is now in
registers (`rsi`/`rbx`). With the release profile: geomean 1.085× over the
default build on `bench/` (best of 7), every benchmark at or above noise
except `fib`/`word_freq`. The rule is now in `exec.rs`'s header. The text
below is what was found before the cause was known; options 1-3 are no
longer needed.*

`exec.rs:231-236` reads `let w = *pc; pc = pc.add(1);` and the module
comment (lines 3-8) says `pc` and `fp` stay in machine registers. In the
release build, `fp` is in `rdi`/`r13` and `w` in `rbx`, but **`pc` is at
`400(%rbp)`** (873 references in the loop; with `panic = "abort"` it's
`64(%rsp)`, so it isn't the SEH funclets' demotion). Every dispatch is

```
movq 400(%rbp), %rdx     ; pc from memory
movq (%rdx), %rbx        ; w = *pc
leaq 8(%rdx), %r8
movq %r8, 400(%rbp)      ; pc back to memory
movzwl %bx, %eax
movslq (%r13,%rax,4), %rax ; jump table
addq %r13, %rax
jmpq *%rax
```

and a taken branch is `store pc; load pc` back to back. The serial chain
through memory (store-to-load forwarding, 4-5 cycles on most cores; the
Zen 5 here probably has memory renaming for rsp-relative slots, which would
hide most of it: *unsure*) *is* the lower bound on every instruction's latency, and it's the cost
the design's "about 1 ns to dispatch a simple op" target (§16) would be
made of. Another core would pay more.

Why LLVM does it: 353 indirect jumps and ~500 arms with temporaries make one
huge function whose register pressure the greedy allocator resolves by
spilling the one value that's live across everything. Ways at it, from least
to most work:

1. **Shrink the loop's register demand.** Move every arm that isn't one of
   the ~60 common ops (the `G` generic-width forms, casts, floats'
   conversions, `Copy`/`Zero` of other sizes, `Unravel`, `CallR`, `Resume`,
   `FreeBuf*`, the `Ld/St` `Ex` forms the codegen rarely picks) out to a
   `#[inline(never)] fn slow(op, w, pc, fp, env) -> Next`. The hot
   function then has few live values and no calls but the natives. Cheap to
   try; the `execute` symbol and the linker's `/ORDER` keep working.
2. **Make the loop's state one struct passed by `&mut` to out-of-line arms**
   so that `pc` and `fp` are never address-taken (they aren't now, but
   `here!()` reads `pc` for every trap site: 5 inlined `instruction_at`
   calls, which also keep `pc` live to many cold blocks).
3. **Tail-call threading** (§6): every op a function that ends in a
   `musttail` call, `pc` and `fp` in argument registers by construction.
   Rust can't (`become` is unstable); the C core can.

*Unsure:* how much the final binary's loop differs from the library's
(the build.rs `/ORDER` and the CGU split could change the spill), so check
the same listing on `loom.exe` (`dumpbin /disasm`) before acting.

### 1.3 `ticks` is a memory read-modify-write, and `env` is reloaded. *Verified.*

`tick!()` (`exec.rs:314-325`) runs on every backward conditional jump, every
`loop` and every call. In the machine code it's
`movq 392(%rbp),%rax ; decl 168(%rax) ; jns …`: `env`'s address comes from a
stack slot each time (62 reloads in the loop; the `black_box` in
`exec.rs:219` is what keeps `env` out of registers, deliberately) and the
counter is decremented *in memory*.

- A read-only check is cheaper: the Ctrl-C handler already sets an
  `AtomicBool` (`ctx.interrupt`); the loop can test a **raw pointer to it**
  kept in `Env` (`*const AtomicBool`, `load(Relaxed)`): one load, no store,
  no dependent chain across iterations. `interrupted()` (which has the
  "once" logic) stays the slow path. Cost: the handler's flag gets checked
  on every back edge instead of every 65 536th, a load that hits L1.
- Or keep `ticks` in a register by making it the loop's third local (it was
  moved out because it cost `pc`/`fp` registers, §exec.rs:3-8: item 1.2
  first).
- Effort S, risk low. It matters most for tight loops (§2.1 removes one of
  their two instructions; this halves the other's cost).

### 1.4 Natives: a 32-byte `Result`, a bounds check, a slice. *Verified.*

`pub(crate) type NativeFn = fn(&mut Ctx, &mut [u64], u16) -> Result<(), Trap>`
(`natives.rs:257`). `size_of::<Trap>()` is **32**, so `Result<(), Trap>` is
32 bytes: the machine code passes a hidden return pointer
(`leaq 256(%rbp),%rcx ; callq *%r10 ; cmpb $-1, 256(%rbp)`), the native
writes its tag to memory, and `CallN` reads it back. The same holds for
`Heap::alloc`/`free`/`realloc` (`Result<u64, Trap>`: 32 bytes) which
`mem.alloc`/`mem.free`/`mem.grow` call on every allocation.

`Op::CallN` (`exec.rs:976-987`) also indexes `env.natives[b(w)]` with a
bounds check and a `panic_bounds_check` arm, though the verifier checked the
native's index (`loom_isa::verify`).

- Natives return `Result<(), Box<Trap>>` (8 bytes: `None` is a null niche,
  it comes back in `rax`): `Trap` is only built on the cold path, and `?`
  converts for free. The heap's `alloc`/`free` the same way, or a `u64`
  with 0 as "failed" and the trap in `ctx` for `alloc` (an object's address
  is never 0: `Heap::alloc` returns `align` for size 0, `align ≥ 1`).
- `get_unchecked` on `natives` (SAFETY: the verifier).
- A native that needs only `args` and not `ctx`/`imm` could take
  `(&mut Ctx, *mut u64)`: no slice length to build. Minor.
- Saves per `calln`: one store and one load of the tag, the bounds check,
  and (with `Box`) a stack temp. word_freq and maps run 2 and 8 per trip.
  Effort S, risk low.

### 1.5 Small things in the loop. *Read.*

- **`Ldstr`** (`exec.rs:624`, `natives.rs:131`): `strings[index]` and then
  `rodata[start..]`, two bounds checks and `env→ctx→strings` pointer
  chasing, per text literal use. The addresses are fixed once `Ctx::new`
  has built `rodata`: keep a `Vec<u64>` of them in `Env` and index it
  unchecked (SAFETY: verified index). Better still, resolve at load time
  and put the absolute address in the instruction (`Ldk` with an extension
  word), as the image is immutable after `Machine::load`. Saves 5-6 loads
  per `ldstr`.
- **`Ret`'s host check** (`exec.rs:1013`): `back.is_null()` on every
  return, to leave the loop once. Point the entry frame's return address at
  a static one-instruction `exit` (a new `Op::Exit` or a `Crash`-like word)
  and drop the compare: one fewer well-predicted branch per return. Low
  value.
- **`CallR`** (`exec.rs:1187-1198`) checks the callee's arity on every call
  through a function value. The verifier can't, but the compiler always
  builds the shape word; keep the check in the checked build and as a
  `debug_assert` otherwise (it's `crash 2` territory, design §14).
- **`Ret` copies results one by one** (`exec.rs:999-1001`) for `n > 1`:
  fine for 2-3 words; `ptr::copy` would be a `memmove` call for small `n`.
  Leave it.
- **`CastT/CastW/CastS` use `.expect("the verifier checked every cast")`**
  (four `option::expect_failed` call sites in the loop): `unwrap_unchecked`
  (SAFETY: the verifier) or, better, §2.2.

---

## 2. The instruction set

### 2.1 No fused increment-and-branch. *Verified (ISA read, §8.4 promises it).*

*Done (2026-10-11): `addjlt.{u,s,s32}` and `addjltk.{u,s,s32}` (wrapping
`+= 1` at `u64`/`i64`/`i32`, then `a < b`), fused in `codegen.rs`'s
`branch` when the add is the block's last word. Dispatches down 5-8% on
sieve, fannkuch, spectral_norm, chip8 and maps; A/B geomean +2.0% (fannkuch
-8%, spectral_norm -10%). The trapping adds (`addk.u64` before `jeqk`) and
`u32` loops are not fused.*

design §8.4: "`i += 1`, a compare and the back-edge become `forrange`". The
op table has no such op (`op.rs`: nothing like `forrange`, `incjlt`, `addj`),
and `codegen.rs:fusions` only fuses a compare into its branch. Every counted
loop in the listings is

```
add%k.u64 r17, r17, 1      ; the induction variable
jlt.u     r17, r15 → body  ; the back edge (a tick, §1.3)
```

two dispatches per iteration for what one op does: `r += k; if r < limit
jump`. fannkuch's swap loop (35 280 trips in its copy loop alone),
sieve's, mandelbrot's, spectral_norm's, nbody's, chip8's and sort's
inner loops all end this way.

Add `forrange` as a family:

- `IncJLtU a, b, off16` / `IncJLtS`: `r[a] += 1; if r[a] < r[b] jump`
  (`u64`/`i64`, wrapping, for `usize`/`isize`/`i64`/`u64` induction
  variables: the common ones); a `…K` form with a 16-bit limit constant in
  place of `b`; and one pair for `i32` (`add` then `wrap`, since `i32` loops
  are everywhere in the benchmarks).
- A *trapping* add must keep trapping: the op is selected only for
  `add%` (wrapping) or where `ranges` proved the add can't overflow (it
  rewrites to `%` then). `Op::check()` classifies by name prefix, so name
  them `addk…` or give the family its own `Check::Overflow` arm, or
  `check_counts.rs`/`counts.txt` move for reasons that aren't optimization.
- The tick stays in the op (it's a back edge).
- Saves one dispatch of every (body + 2) per iteration: 10-15% of a tight
  loop's instructions; the instruction counts of `bench/counts.txt` move,
  and must be blessed in the commit.
- Effort M (op, verifier operands and offset, disasm, exec arm, a codegen
  fusion in `inst_k`/`branch_word`, `-O0` parity), risk low.

A variant worth the same op: the **rotated loop's head test** is copied to
the back edge (`jlt.u` there) and costs nothing extra; `forrange` only
replaces the add+jlt pair.

### 2.2 Casts decode an extension word and call out. *Verified.*

`Op::CastT/CastW/CastS` (`exec.rs:927-941`) read an extension word, call
**`loom_isa::word::unpack_cast`** (out of line: 3 call sites, *verified*)
which unpacks two `IntSpec`s with range checks into `Option`s, `expect` the
result, then `ints::cast_t` (`ints.rs:213`) does the range test with
`i128` arithmetic (`IntSpec::min/max/value` are `i128`, `word.rs:102-117`).
Casts are everywhere: `i as usize`, `x as i32`, `b as u8`; fannkuch has
three per inner trip.

A register is *normalized* (sign- or zero-extended to 64 bits), so the
range check depends only on the **target** `(bits, signed)` and the
**source's signedness**: the source's width is irrelevant.

- Ops `CastT{I8,I16,I32,I64,U8,U16,U32,U64}{FromS,FromU}` (16), no
  extension word, the check in `i64`/`u64` arithmetic only:
  `from signed: v >= min && v <= max`, `from unsigned: v <= max`, with the
  `to`'s bounds as constants per arm. `CastW{to}` (8 ops) is `wrap` and
  needs only the target. `CastS` (saturating, rare) can keep the generic
  form.
- Keep the ops' names starting with `cast` (see §2.1's note on
  `Op::check()`).
- Also fixes the `expect` call sites and drops 2 of 3 loads of a cast.
- Effort M, risk low (`rt_int.rs` already checks the integer ops against the
  oracle; extend it over the new ops).

`FtoiT/FtoiS` (`exec.rs:946-960`) have the same extension-word shape, and
`ints::ftoi_t` (`ints.rs:229-241`) builds its bounds with
`(1u128 << (bits-1)) as f64`: **`__floatuntidf` is a call in the loop**
(*verified*, two sites), a software 128-bit to float conversion on every
float-to-int cast. Use `f64::from_bits(((1023 + bits - 1) as u64) << 52)`
(exact powers of two) or a 65-entry table, and `ints::spec` once per op
family as above.

### 2.3 `sqrt` and friends go through a native. *Read.*

`float.math` (`methods.rs:711-749`, placed next to the loop by `build.rs`
because "`nbody` calls it for every square root"): a `calln` is the
dispatch, 3-4 `mov`s of arguments into the native window, an indirect call
through a 16-byte table entry, a slice, a decode of `imm` (a `match` of 10
arms with NaN logic) and the result store. A dispatch loop arm with
`f64::sqrt` is `sqrtsd` plus `set!`.

- Ops `SqrtF64/F32`, `AbsF64/F32`, `FloorF64`, `CeilF64`, `TruncF64`,
  `MinF64`, `MaxF64`: the ones with hardware instructions
  (`sqrtsd`, `roundsd` needs SSE4.1: fine under `x86-64-v2`+, otherwise
  libm calls). `round` (away from zero) and `powf` stay natives.
- The compiler's `float.math` lowering picks the op by `imm`.
- Effort S, risk low. Note `min`/`max` have NaN rules (`methods.rs:717-731`)
  that `minsd`/`maxsd` don't share: keep the rules in the arm.

### 2.4 Generic-width forms are two words and branchy. *Read.*

`AddWG`, `SubWG`, `MulWG`, `AddTG`… (`exec.rs:633-732`) take their width
from an extension word (`ints::spec(ext!())`: three field extractions) and
`wrap` branches on `signed` at run time. They're what `u12`/`i24` code
(chip8's `pc`, `i`, `0xFFF` masks) runs. For unsigned widths, `add.u64`
followed by `andk mask` is two cheap dispatches; as one op, `AddWKG a, b,
imm` with the mask in the instruction (width ≤ 16 fits `c`) skips the
extension fetch. Low value unless a benchmark leans on odd widths.

---

## 3. Natives: text, formatting, sinks

### 3.1 Formatting allocates twice per hole. *Read.*

`out_fmt` (`natives.rs:302`) and `sink_fmt` (`natives.rs:432`) do
`let mut buf = Vec::new(); ctx.format(&mut buf, …); ctx.put(sink, &buf)`:
a `malloc` (Windows' heap: tens of ns), growth, a copy into the sink's
buffer and a `free`, for each `{x}`. Inside, `fmt::show` (`fmt.rs:33-66`)
does `int_text` → `r.to_string()` (a second `String`) and
`float_text` → `to_string()` (a third) and then `extend_from_slice`.
`with_spec` (`fmt.rs:104-174`) allocates `body` and a `fill` `Vec`
(`to_vec`) and walks `from_utf8_lossy(&body).chars().count()` for the
width.

- Format straight into the destination `Vec<u8>` (`ctx.bufs[h - 2]`,
  `ctx.out`, `ctx.msg`): borrow `ctx.fmts` separately (it's `&'i`).
  `ctx.format` takes `&self` today only because of `self.text`.
- Integers: a hand-written `itoa` into a `[u8; 20]` on the stack (the
  2-digit lookup table), then `extend_from_slice`: no `String`, no
  `fmt::Display` machinery. Floats: `write!(out, "{}", v)` into the `Vec`
  directly (`io::Write for Vec<u8>`), the `.0` suffix decided from the
  written bytes.
- `with_spec`: write the body into the destination's tail, then insert the
  padding (`splice`/rotate) instead of building `body`, `fill` and a lossy
  string; count characters of the *written bytes* (`b & 0xC0 != 0x80`).
- Saves two heap round trips and 1-2 copies per `{x}`; for
  `"{prefix}{i}"` in maps that's the integer hole. Effort S, risk low
  (the Lua runtime's `rt.fmt` tests compare outputs).

### 3.2 Every sink call carries a handle. *Read (LIFO confirmed in the source).*

`sink.push` returns a handle, and `sink.str/fmt/fmt2/pad/string` each take
it as register 0: in maps' listing the handle is reloaded into the window
(`mov r44, r11`) before every one, and it's kept live across all of them.
`Ctx::pop` already **requires** that the sink popped is the last made, and
`text.rs` is balanced: `write_value`'s `sink.pad` makes an inner buffer,
shows into it, and pads it into the *outer* one after the inner is popped;
operands are all worked out before the first write (`template_pieces`,
`text.rs:122-171`), and `show` glue called with a handle only writes to it
or to buffers it makes and pops itself. So every `sink.*` write is to the
**innermost open buffer**, except `out.*` (handle 0, its own natives) and
`msg.*` (handle 1, its own natives).

- Drop the handle: `sink.str(text)`, `sink.fmt(value)` and `sink.string(dst)`
  write to `bufs.last_mut()`, `sink.pad(into)` pops the inner and writes to
  the new last. One register and one `mov` per sink call; the sink's
  register isn't live across the template; `Formatter` values lose their
  handle field (`fmt_put`, `text.rs:323`, reads it from the warp).
- Not decided from the source: `stream.rs:1492` writes with `buf.0` inside
  a loop that may call user code; it is balanced too, but check it and
  user `Show` implementations that keep a `Formatter` past the call
  (`fmt_put`'s `f.put`). Also the Lua backend's `rt` sinks must keep the
  same observable behaviour: this is a Treadle-only codegen change.
- Effort M (the native table, the builder, `fmt.put`, the glue), risk
  medium (a wrong target silently interleaves text: the checked build's
  `pop` would catch an unbalanced one).

### 3.3 Text natives. *Read.*

- `position` (`methods.rs:59-64`, behind `str.find`, `replace`, `split`,
  `contains`): `hay.windows(n).position(|w| w == needle)` is O(n·m) with a
  slice compare per offset. Text is valid UTF-8 by construction: use
  `str::find` (`&str`: Two-Way) via `from_utf8_unchecked`, or a
  `memchr::memmem`-style first-byte scan (`memchr` for the first byte, SSE2/
  AVX2 compare 16/32 bytes at a time). For a single-byte needle, `memchr`
  alone.
- `next_line` (`methods.rs:619`) and `split`: `iter().position(|&b| b ==
  b'\n')` is a byte loop; a `memchr` is 16-32 bytes per cycle. `lexer`
  (chars, words) and any `lines()`/`split` program benefits.
- `next_word` / `is_space`: a lookup table (`[bool; 256]`) or a SWAR test
  in place of `b == b' ' || (9..=13).contains(&b)`; minor.
- `char_count` (`methods.rs:67`): `filter(|b| b & 0xC0 != 0x80).count()` is
  auto-vectorized by LLVM in release; fine, check with `x86-64-v3` that it
  uses `vpsadbw`.
- `push_str` (`methods.rs:363`): `text(...).to_vec()` is a heap copy per
  call. It guards against `s.push_str(s_view_of_itself)` across a `realloc`
  (the `append` may move the buffer the text points into): keep the copy
  only when `[ptr, ptr+len)` overlaps the header's buffer, which one pointer
  compare decides. `string.push_char` (`methods.rs:355`) is already
  alloc-free.
- `split` (`methods.rs:167`) copies `s` and `sep` with `.to_vec()` "to be
  safe", then `new_string`s each piece: the destination is a fresh
  allocation, so no copy is needed; allocate the `n * 24` header buffer
  once (it is) and use size-class `alloc` + `memcpy` per piece.
  `case` (`methods.rs:116`) copies, uppercases the copy, then
  `new_string` copies again: allocate the destination first and convert in
  place.
- `text_hash` (`methods.rs:416`) is one multiply per 8 bytes: good for
  identifier-sized keys. For long keys (paths, lines) an AES-NI/CLMUL
  hash (aHash/gxhash's shape: `aesenc` over 16 bytes/cycle) would be 4-8×
  faster; only worth it if a benchmark has 64+ byte keys. Unsure.
- `str.eq`/`str.cmp` are `memcmp`: fine. `str.hash` + `map.find_str` are
  already outlined and ordered next to the loop by `build.rs`.

---

## 4. The heap. *Read.*

`heap.rs`: size classes (16 bytes apart to 256, then 4 per doubling), 64 KiB
pages, per-class `Vec<u64>` free lists.

- **Intrusive free lists.** `free[class].push(ptr)` / `.pop()` (`heap.rs`
  lines 112, 155) is a `Vec` push/pop (capacity test, possible realloc, a
  second heap object per class) and the header `free: Vec<Vec<u64>>` is
  indexed with a bounds check after `resize_with` check (`heap.rs:109`
  runs on **every** small allocation). Freed objects are at least 16 bytes
  and 16-aligned: store the *next* pointer in the first word of the free
  object, keep `heads: [u64; CLASSES]` in the struct. `alloc` = load head,
  load `next` (from the object the program touches next anyway), store
  head. `free` = store old head into the object, set head. No `Vec`, no
  bounds check, one fewer cache line.
- **Size class from the compiler.** `mem.alloc(size, align)` almost always
  has constant arguments (a warp's size, an element size). `class_of`
  (`heap.rs:39`) and `is_small` are then compile-time facts: a dedicated
  `AllocC a, class` / `FreeC a, class` pair of ops (or natives with the
  class in `imm`) skips the class computation, the `LIMIT` check can move
  to page refills (count `live` by pages; the "out of memory" message is
  the same, only the exact byte of the limit moves) and the `size == 0`
  test goes (the compiler knows it).
- **`Result<u64, Trap>`** (§1.4): 32-byte hidden return for every
  allocation; with an `alloc` that returns 0 on failure and a cold
  `ctx.oom()` it's a register.
- `Heap::alloc`/`free` stay `export_name`d and ordered by `build.rs`: keep
  that (their placement moved benchmarks 30%).
- `Stack::first_holds`/`reset` are per call from outside only; `grow` and
  `shrink` are cold. Fine.
- Beyond: a bump region for short-lived `sink` strings, or `mimalloc`-style
  free-list sharding for V5's threads: not before V5.

---

## 5. Maps (`heddle/build/map.rs`). *Read.*

The lookups are already natives (`map.find_int/str`, word-at-a-time hash,
splitmix finalizer, linear probing on an index of `entry + 1` words kept at
most half full). What's left is around them.

- **`compact`** (`map.rs:377-411`) moves each live entry down with a
  `mem.move` *native call per entry* (line 398): ~6 dispatches and an
  indirect call per 24 bytes. `InstKind::Copy { size: esize }` is one
  dispatch (`Op::Copy` has 8/16/24-byte fast paths and handles overlap).
  Better: move *runs* (the live entries between tombstones) with one
  `mem.move` each. maps removes 30 000 keys.
- **`reindex`** (`map.rs:255-291`) rebuilds the index with a bytecode loop,
  `index_put` inlined per entry (~15 dispatches each), at every doubling
  and every compaction. A native `map.reindex(header, slots, layout)` is the
  loop `for e in 0..used { if hash != 0 { probe; store } }` in Rust: about
  20× fewer dispatches for the same work. `push_entry` (`map.rs:295`) calls
  it when the index is over half full.
- **Hash tags in the index.** A slot is `entry + 1` in a 64-bit word and
  entry numbers need 32 bits at most: put 32 bits of the hash above it.
  `probe` (`methods.rs:484`) then compares the tag first and reads an
  *entry* (a second cache line, `esize` apart) only on a tag match; a
  miss or a collision stops costing a cache miss per probed slot. The
  index is `slots × 8` bytes and sequential, so probes mostly stay in one
  line. The same encoding has to change **in three places together**: the
  native probe, and `find_key`/`index_put`/`reindex` in `map.rs`
  (the in-Loom path for structural keys), plus the Lua backend's `rt` if it
  shares the layout (it doesn't: it's a table). Effort M, risk medium.
- `copy_coll` (`map.rs:462-500`) is two `mem.move`s: good.
- Maps keep **insertion order** by an entries array; lookups that miss walk
  the index only: good.

---

## 6. Big-ticket options

These change the design's D2 (the dispatch strategy), so they are V8's by
the design's own rule ("15% for a new dispatch strategy").

- **Tail-call threading in C** (`[[clang::musttail]]`, optionally
  `__attribute__((preserve_none))`): the toolchain is clang-cl 22, which
  supports both on x86-64 Windows. Each op is a `static` function
  `(pc, fp, env, w) → void` ending in `MUSTTAIL return handlers[op(*pc)]
  (pc+1, fp, env, …)`; `pc`, `fp` and `env` are in argument registers *by
  construction*, so the spill of §1.2 cannot happen, and each handler is
  small enough for the allocator. CPython 3.14's interpreter gained 10-15%
  this way on clang 19. Generate the 500 handlers from `op.rs`'s table in
  `build.rs` (macro-expanded from a per-op body table kept in one file), and
  call the Rust natives through `extern "C"`. Cost: a C source of the
  loop's arm bodies (about 1 500 lines of macros), a second implementation
  of every op (the checked build, `stats` counters, traps) and keeping the
  two honest (the generator in `gen.rs` compares backends, the fixtures
  run on both). Large; the best single lever if §1.2 is confirmed.
- **Direct threading at load time** (replace opcodes with handler
  addresses in a copy of the code): only with computed goto (C) or `become`.
  Saves the table lookup (`movslq (%r13,%rax,4)`, `addq`) per dispatch.
  Same C core.
- **PGO** for the `loom` binary: in V8's list already. `cargo pgo` /
  `-C profile-generate` over the benchmarks and the fixtures; the dispatch
  loop's block placement and the natives' branch layout are what it
  improves. Sensitive to the `-align-all-nofallthru-blocks` flags.
- **Superinstruction mining** (V8's list): the stats build counts adjacent
  op pairs (`pc_counts`); the pairs worth fusing from the listings are
  `ldf.64 + idx` (a field of a frame aggregate), `add%k + jlt` (§2.1),
  `idx + ld` (`ldx` exists), `mov + calln` argument setup, and the
  `jnek r, 0` after a `ld.u8` (a load-and-branch).
- **Register windows for natives**: `mov r44, rX` pairs before each `calln`
  (word_freq 22.7%, maps 18% moves) come from the window convention; a
  native whose arguments are already consecutive and last used there needs
  none. The allocator has hints for this (§8.5); the remaining ones are
  values live past the call. Worth reading `allocate`'s window choice on
  the listings (`codegen.rs:1183-1228`) before adding ops.

---

## 7. What held up

- The dispatch is a real jump table with the tail duplicated into each arm
  (*verified*: 353 indirect jumps), `fp` in a register, `w` in a register.
- Integer ops fold their width at compile time (`ints.rs` `inline(always)`),
  immediates are 16-bit, loads and stores have base+index forms and a
  checked-index form; traps are one out-of-line block.
- `map.find_*` and `str.hash` are close to what a hand-tuned version is:
  one multiply per word, overlapping tail reads, a finalizer for the index.
- `str.concat2/3/4` already turn all-text templates into one allocation.
- Stack segments never move, are zero-filled lazily (`vec![0; n]` is
  `calloc`), and a boundary call doesn't allocate each time.
- The heap is a size-class allocator with no per-object header.
- The checked build (`checked` + `stats`) catches bad memory accesses
  without costing the release build anything.

## 8. Constraints every change above carries

- **A new op needs the whole chain:** `op.rs` (table, form), `verify.rs`
  (operands, offsets, extension words), `disasm.rs`, the arm in `exec.rs`,
  the selection in `codegen.rs`, `Op::check()` (name prefix classifies it
  as a bounds or an overflow check: keep `cast…`, `add…`, `ldxc`… names or
  extend it), the fixtures at `-O0` and `-O2`, and `bench/counts.txt`
  blessed in the same commit (a fused op *should* move it).
- **`check_counts.rs`** (80%/60% of checks gone) counts by op name: a
  family that replaces `cast.*` or `addk.*` must stay counted, or the gate
  moves for reasons that aren't optimization.
- **`build.rs` `ORDER`:** a native or helper that becomes hot goes into the
  list (`loom_treadle_*` export names), and the benchmarks need an A/B
  because placement moved them by up to 30%.
- **Compile-time gate (1.5×):** anything that makes `-O0` or the front end
  cheaper *raises* the `-O2`/`-O0` ratio; V3's item 6 is at 1.48× with 2%
  margin. Run `compile_time` after each change, and prefer changes in the
  VM and codegen that cost no pass time.
- **Both backends:** text, formatting and map behaviour must stay what
  `runtime/rt` does; the sink handle change (§3.2) is Treadle-only in the
  builder, and the generator (`gen.rs`) compares the two.
- **Each change wants an A/B at ±5% noise:** design §16's rule (kept only if
  it wins 3% on the suite's geomean) applies, and two or three of the items
  above are small enough that they only pay together.

## 9. Not looked at

`loom_host` (the Lua side), `loom_sys`, the verifier's cost at load
(`Vm::new` verifies the whole image on every run: 30 µs for an empty
program, grows with code size: `iterators.loom`'s 11k lines of bytecode is
the case to time), shuttles' frame handling (`frame.rs`), and the Lua
backend's own runtime.
