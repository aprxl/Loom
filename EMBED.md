# Embedding Loom in a Rust host

This guide shows how a Rust application embeds Loom: it registers functions,
constants, value warps, handles, enums and wefts, compiles Loom source that uses
them, and calls the `pub` functions of that source from Rust.

Every snippet is an excerpt of a test that passes, named under its heading, so
the code compiles against the current API. The tests live in
`crates/loom_embed/tests/` (`api.rs`, `typed.rs`, `objects.rs`,
`rectangle_example.rs`) and `crates/loom_compiler/tests/host_extern.rs`.

## 1. Overview

### The crates

| Crate | What a host uses it for |
|-------|-------------------------|
| `loom_embed` | The facade: `Embed`, `Program`, `Running`, `Layout`, `WarpRef`, `Handle`, `Literal`, `Scalar`, `EmbedError`. |
| `loom_sys` | `Host` (where a run writes and reads) and `Outcome` (how a run ends). |
| `loom_vm` | `HostCx`, the handle a raw closure gets. `loom_embed` doesn't re-export it, so a raw closure's crate depends on `loom_vm` too. |
| `loom_syntax` | `Diagnostic`, the errors `compile` returns. |

The examples below use the workspace's path dependencies: `loom_embed`,
`loom_sys`, and `loom_vm` when a raw closure names `HostCx`.

### The lifecycle

```
Embed::new(module)                      declare what Loom may use
  -> fun / raw_fun / constant / warp / handle / weft / ...
  -> source(text)                       hardcoded Loom, optional
  -> compile(user)                      -> Program
  -> program.start(host)                -> Running
  -> running.call(name, args)           -> Vec<u64> (the results, as registers)
```

The doc test at the top of `crates/loom_embed/src/lib.rs` runs this whole
path. Each `Embed` method adds to a Loom module named after `Embed::new`, and
the program `use`s that module.

### One program, many runs

`Program::start(&self, host)` takes `&self`, so one compiled program starts any
number of times. Each run gets its own copy of every host closure, made by
cloning it, so a closure's captured state must be `Clone` (and `Send + 'static`).
`test a_program_starts_twice_and_each_run_has_its_own_state` (`api.rs`) checks
that two runs don't share state.

A handle's values (section 4) are shared by every run of the program, and they
live until the program is dropped.

## 2. Host functions

A host function is a Rust closure that a Loom program calls. Two forms exist:
the typed form, which covers almost every case, and the raw form, which gives
the registers themselves.

### 2.1 The typed form: `fun`

Annotate the parameter types. The annotations pick the registration, so there
is no turbofish. Test `every_arity_takes_mixed_scalars_and_returns_a_string`
(`typed.rs`):

```rust
let mut embed = Embed::new("m");
embed.fun("zero", || 7_i64).expect("registers");
embed.fun("neg", |x: i64| -x).expect("registers");
embed.fun("mix", |x: f64, y: f32| x * f64::from(y)).expect("registers");
embed.fun("text", |n: i64, c: char| c.to_string().repeat(n as usize)).expect("registers");
```

| Type | As a parameter | As a result |
|------|----------------|-------------|
| `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64` | one register | one register |
| `bool`, `char` | one register | one register |
| `f32`, `f64` | one register (the bits) | one register (the bits) |
| `&str` | two registers: an address and a length | not allowed |
| `String` | not allowed | a slot the host fills (see 2.2) |

A closure takes up to three arguments, and a handle method (section 4) up to
two, not counting the handle. An unannotated `&str` parameter is rejected: the
doc comment on `Embed::fun` has a `compile_fail` example.

### 2.2 Text in and out: `&str` and `String`

Test `text_arguments_infer_without_a_turbofish` (`typed.rs`):

```rust
let mut embed = Embed::new("m");
embed.fun("cat", |s: &str, n: i64| s.repeat(n as usize)).expect("registers");
embed.fun("len", |s: &str| s.len() as u64).expect("registers");
```

A `&str` argument arrives as its bytes, so `len` counts bytes, not characters.
A `String` result is written into a slot the host fills. The Loom side gets the
string, and the host never returns a value to Loom directly.

### 2.3 The raw form: `raw_fun`

Use `raw_fun` when the typed form can't express the call, such as a host function
that reads its registers or text through `HostCx` itself. You get the registers
and a `HostCx` (from `loom_vm`).

Test `a_raw_function_takes_text_and_returns_a_string` (`typed.rs`):

```rust
embed
    .raw_fun("pad_text", &["str", "i64"], Some("String"), |cx, r| {
        let text = std::str::from_utf8(cx.text(r[1], r[2])?).map_err(|e| e.to_string())?.to_string();
        let n = r[3] as i64;
        cx.new_string(text.repeat(n as usize).as_bytes(), r[0])
    })
    .expect("registers");
```

Reading the registers:

- A `String` result takes slot `r[0]`, and the parameters follow it. Here
  `str` is `r[1]` and `r[2]`, and the `i64` is `r[3]`.
- A scalar result is written to `r[0]`, and the parameters start at `r[0]`.
- `cx.text(addr, len)` reads a `&str` argument's bytes.
- `cx.new_string(bytes, dst)` fills a `String` result's slot.

A raw closure's errors: returning `Err(message)` makes the run end with an
unravel, see section 8.

## 3. Constants

### 3.1 Scalar constants

`constant` declares `pub const NAME: T = value` in the module. Test
`a_constant_and_hardcoded_source_go_through_width` (`api.rs`) declares the
constant and then has source use it: `width()` returns `TAB_WIDTH * 2`, which is 8.

```rust
embed.constant("TAB_WIDTH", 4_i64).expect("declares");
```

### 3.2 Warp constants

`constant_warp` declares a constant whose type is a value warp. Each field
gets a `Literal`, and the field's type must match. Test
`a_constant_warp_is_a_loom_constant` (`objects.rs`):

```rust
let point = embed.warp("Point", &[("x", "f64"), ("y", "f64")]).expect("declares");
let x = Literal::of(1.5_f64).expect("a literal");
let y = Literal::of(2.0_f64).expect("a literal");
embed.constant_warp(&point, "ORIGIN", &[("y", y), ("x", x)]).expect("declares");
```

`Literal::of` takes any `Scalar` and refuses a float with no literal (NaN and
the infinities). `constant_warp` refuses a field that doesn't exist, a field
given the wrong type, and a missing field: `a_constant_warp_checks_its_fields`
(`objects.rs`) checks all three.

## 4. Warps

Loom has two kinds of warp that a host can see: value warps, which are copies
of scalars passed by address, and handles, which are ids into a table the host
keeps.

### 4.1 Value warps

`warp` declares a struct of scalar fields. A host function gets it as the
address of a copy, and reads the fields through a `Layout`. Test
`a_host_method_reads_a_warp_literal` (`objects.rs`):

```rust
let point = embed.warp("Point", &[("x", "f64"), ("y", "f64")]).expect("declares");
embed
    .warp_method(&point, "norm", &[], "f64", move |cx, r, l| {
        let bytes = l.bytes(cx, r[0])?;
        let (x, y): (f64, f64) = (l.read(bytes, 0), l.read(bytes, 1));
        r[0] = (x * x + y * y).sqrt().to_reg();
        Ok(())
    })
    .expect("registers");
```

- `warp_method` gives the Loom method `norm` on `Point`. The closure gets the
  `Layout` of its warp as its third argument.
- `layout.bytes(cx, addr)` checks the whole value's bytes once per call, and
  `layout.read(bytes, k)` decodes field `k` in declaration order.
- `layout.bytes_mut(cx, addr)` and `layout.write(bytes, k, value)` write
  fields. Test `a_host_method_returns_a_warp_through_the_out_slot`
  (`objects.rs`) shows a method that returns a warp.

Field offsets come from the compiler's layout, not from the host, so a mix of
field sizes works without the host knowing the offsets:
`field_offsets_come_from_the_layout_for_mixed_sizes` (`objects.rs`).

Register conventions for a `warp_method`:

- A scalar result: `r[0]` is `self`'s address, and the parameters follow it.
- A warp or `String` result: `r[0]` is the result slot, `r[1]` is `self`'s
  address, and the parameters follow it.

### 4.2 Handles

A handle is a Rust value the host keeps in a table, and the Loom warp holds
only its id. `handle::<T>("Buffer")` declares the type, `constructor0`,
`constructor1` and `constructor2` make values, and `method` adds methods.
Test `a_handle_is_copy_and_its_methods_see_one_value` (`typed.rs`):

```rust
let mut embed = Embed::new("m");
let buffer = embed.handle::<i64>("Buffer").expect("declares");
embed.constructor1(&buffer, "open", |x: i64| x).expect("registers");
embed
    .method(&buffer, "add", |v: &mut i64, d: i64| {
        *v += d;
        *v
    })
    .expect("registers");
embed.method(&buffer, "get", |v: &mut i64| *v).expect("registers");
```

- The Loom `Buffer` is a `Copy` warp holding the id, so copies share one value.
- A method's first parameter is the value the id names, as `&mut T`.
- A stale id (a value that's gone) is an unravel, see section 8: test
  `a_stale_id_is_an_unravel` (`typed.rs`).
- Loom code can't build a `Buffer` from a raw id: `outside_code_cannot_build_a_handle_from_its_id`
  (`typed.rs`) checks that the id field is private.

A handle method can take text: `a_handle_method_takes_text` (`typed.rs`).

## 5. Enums

### 5.1 Unit enums

A unit enum (every case without fields) crosses as its case's index, in one
register. `unit_enum` declares the Loom enum, and the Rust type of the same name
implements `Scalar` with `to_reg` giving the index. Test
`a_unit_enum_crosses_as_its_index` (`objects.rs`):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Light {
    Red,
    Green,
    Off,
}

impl Scalar for Light {
    const LOOM: &'static str = "Light";
    fn to_reg(self) -> u64 {
        self as u64
    }
    fn from_reg(reg: u64) -> Self {
        match reg {
            0 => Light::Red,
            1 => Light::Green,
            _ => Light::Off,
        }
    }
    fn literal(self) -> Option<String> {
        None
    }
}

let mut embed = Embed::new("m");
embed.unit_enum("Light", &["Red", "Green", "Off"]).expect("declares");
embed
    .fun("next", |l: Light| match l {
        Light::Red => Light::Green,
        Light::Green => Light::Off,
        Light::Off => Light::Red,
    })
    .expect("registers");
```

### 5.2 Payload enums

A payload enum has cases with fields. It crosses by address, the same as a
value warp, and the compiler gives the tag and each case's field offsets.
`enum_warp` declares it, `warp_method` adds methods, and the closures read the
case with `layout.case(bytes)` and a payload field with `layout.read_case`.
Test `a_payload_enum_crosses_with_its_tag_and_payload` (`objects.rs`):

```rust
let shape = embed.enum_warp("Shape", &[("Dot", &[]), ("Circle", &[("r", "f64")])]).expect("declares");
embed
    .warp_method(&shape, "area", &[], "f64", move |cx, r, l| {
        let bytes = l.bytes(cx, r[0])?;
        let area = if l.case(bytes) == 1 {
            let radius: f64 = l.read_case(bytes, 1, 0);
            3.0 * radius * radius
        } else {
            0.0
        };
        r[0] = area.to_reg();
        Ok(())
    })
    .expect("registers");
```

A host can also write a payload enum: the `grown` method in the same test
calls `set_case` and `write_case` on the result slot.

A payload enum with fields shared by every case (a field declared before the
cases) is refused with U0102: `an_enum_with_shared_fields_is_refused`
(`host_extern.rs`).

## 6. Classes: wefts and weaves

A weft is a Loom interface. A host can declare one, and make value warps
satisfy it.

### 6.1 Declaring a weft and implementing it

`weft` takes each method's name, its parameter types (Loom names), and its
result (`""` for none). `implement` makes a value warp satisfy the weft with
its own warp methods. Test `a_host_method_implements_a_weft_and_any_dispatches_to_it`
(`objects.rs`):

```rust
let circle = embed.warp("Circle", &[("r", "f64")]).expect("declares");
embed
    .weft("Describe", &[("describe", &[], "String"), ("scale", &["f64"], "f64")])
    .expect("declares");
embed.implement(&circle, "Describe").expect("implements");
```

Its methods come from `warp_method` (as in section 4.1). Loom code then calls
them through an `any` of the weft:

```
let d: any Describe = Circle(r: r)
d.describe().len()
```

A warp missing a method of the weft is a compile error:
`a_warp_missing_a_weft_method_is_refused`. Implementing an unknown weft, or the
same weft twice, is an error: `implementing_an_unknown_weft_or_twice_is_an_error`.

### 6.2 Showing a value through a host method

`show_via` makes a value warp show through one of its host methods, a method
that gives a `String`. Templates and `print` then show the value, and
`any Show` dispatches to it. Test `a_warp_shows_through_a_host_method`
(`objects.rs`):

```rust
embed.show_via(&circle, "label").expect("shows");
```

Loom then writes `"{Circle(r: r)}"` and gets the host's text.

### 6.3 A host-declared weave: `weave X into Y`

`weave` declares `weave X into Y` for a value warp, where each method of the
weft calls a warp method of the host's choosing. Test
`a_host_declared_weave_maps_weft_methods_to_host_methods` (`objects.rs`):

```rust
embed.weft("Labeled", &[("label", &[], "String"), ("twice", &["f64"], "f64")]).expect("declares");
embed.weave(&circle, "Labeled", &[("label", "text"), ("twice", "double")]).expect("weaves");
```

`weave` refuses a weft that isn't declared, a weft method with no mapping, and a
warp that already implements or weaves the weft:
`a_weave_needs_a_host_method_for_each_weft_method`.

## 7. Hardcoded Loom, exports, and `Scalar`

### 7.1 Loom source in the module

`source` adds Loom text to the host's module. The text can use everything the
module declares and the host functions. The whole example in section 10 does
this with `doubled_area`. Source goes after the host declarations, so it can
call them.

### 7.2 Calling `pub` functions from Rust

`compile` checks the user's program and the module together. Its `pub`
functions are the exports, and `main` is optional: `compile` builds the program
from its exports. `Running::call(name, args)` calls one by name. This is the
shape of the doc test in `crates/loom_embed/src/lib.rs`, which compiles and runs:

```rust
let program = embed.compile("use m.{add, width}\npub fun calc(a: i64, b: i64) -> i64 { add(a, b) + width() }")?;
let mut run = program.start(host)?;
let out = run.call("calc", &[2, 3])?;
```

- `args` are registers: a scalar is passed as `Scalar::to_reg`.
- The result is a `Vec<u64>` of registers: a scalar is read back with
  `Scalar::from_reg`.
- `call` finds only functions without generics, and the wrong name or the
  wrong argument count is an error (section 8).

### 7.3 `Scalar`

`Scalar` is the trait every scalar crosses with. It has four items:

- `const LOOM: &'static str`: the Loom type's name (`"i64"`, or a unit enum's
  name).
- `fn to_reg(self) -> u64` and `fn from_reg(reg: u64) -> Self`.
- `fn literal(self) -> Option<String>`: the Loom literal, or `None` when there
  isn't one. Used by `constant` and `Literal::of`.

The built-in impls cover the numbers, `bool`, `char`, `f32` and `f64`.
`narrow_signed_integers_round_trip` and `bool_and_char_round_trip` (`api.rs`)
check the conversions. `f32_nan_and_negative_zero_round_trip_bit_exact`
(`typed.rs`) checks that floats keep their bits.

## 8. Errors and refusals

There are four places an error can come from, and each has its own shape.

### 8.1 Registration: `EmbedError`

Every registration method returns `Result<_, EmbedError>`. It refuses:

- a name of the wrong case: a function is `snake_case`, a handle or warp or
  enum is `PascalCase`, a constant is `SCREAMING_CASE`:
  `a_non_snake_case_function_or_non_pascal_handle_is_rejected` (`typed.rs`).
- a name registered twice, whatever its kind (a function and a constant can't
  share one);
- a name with a space, a comment, or a Loom keyword;
- a parameter or result type that doesn't cross into Loom;
- an empty unit enum, or a case name that isn't a `PascalCase` name.

`bad_registrations_are_errors` (`api.rs`) checks a set of these.

### 8.2 Compile: `Vec<Diagnostic>`

`compile` returns the diagnostics of the user's program and the module. A name
the module doesn't export is one of them: `an_unknown_name_comes_back_as_diagnostics`
(`api.rs`).

Treadle refuses what it can't run yet with U0102. The refused cases:

- a `mut` parameter on a host function: `a_mutable_parameter_is_refused`
  (`host_extern.rs`);
- a `List`, a tuple, or a generic parameter on a host function:
  `other_types_are_not_implemented_yet` (`host_extern.rs`);
- an enum with shared fields: `an_enum_with_shared_fields_is_refused`.

`mut self` has no form in the facade: every method takes `self`, and a method
that changes a value does it through the `&mut T` of a handle method, or through
`Layout::write`.

### 8.3 Running: `Outcome`

`start` and `call` return `Result<_, Outcome>`. The outcomes a host sees:

- `Outcome::Unravel { message, .. }`: a host function returned `Err(message)`,
  or a stale handle was used. `an_error_from_a_raw_function_is_an_unravel`
  (`api.rs`) and `a_stale_id_is_an_unravel` (`typed.rs`) check both, the second
  with the message `stale handle 99`.
- `Outcome::Crash { message, .. }`: `call` named no function, named one that
  isn't `pub`, or got the wrong number of arguments.
  `calling_what_is_not_there_is_an_error` (`api.rs`) checks both.

## 9. Performance

A host call is one indirect call into the closure, and the layout's offsets are
resolved once at compile time, so a field read is a fixed-width copy. `layout.bytes`
makes the one checked call per value, and each field read slices the bytes that
call already sized.

Measured against the same method written in Loom (`crates/loom/tests/host_gate.rs`,
release build, best of five runs). Each case runs a fixed loop of host calls, and
the loop's count is in the source of that case (200 000 for the warp cases,
1 000 000 for `count_words` and `count_spaces`, 10 000 000 for `len_of`). The ratio
is Loom's time over the host's, so above 1 means the host is faster. The gate is 0.8.

| Case | Ratio | Gated |
|------|-------|-------|
| `overhead` | 1.01 | yes |
| `to_text` | 0.93 | yes |
| `count_words` | 21.99 | yes |
| `count_spaces` | 30.40 | yes |
| `poly` | 3.79 | yes |
| `work` | 5.50 | yes |
| `a_buffer_append_costs_what_a_loom_list_push_costs` (handle method) | 1.00 | yes |
| `a_mat3_product_costs_what_a_loom_method_costs` (a value-warp method, 3×3 matrix product) | 1.58 | yes |
| `len_of` (a trivial accessor) | 0.68 | no |

A trivial accessor, a method that does almost nothing, is slower on the host:
the call costs more than the work inside it. The gate reports those cases and
doesn't hold them to 0.8.

Real work is faster on the host. The Mat3 product reads two 9-field warps and
writes one with `layout.read` and `layout.write`, and it runs 1.58 times as fast
as the Loom method. To keep that, decode fields through `Layout` rather than by
hand: the offsets are already resolved.

## 10. A full example

This is `crates/loom_embed/tests/rectangle_example.rs`, verbatim. It declares a
value warp `Rectangle`, a host method `area`, a host constant `UNIT_SQUARE`, Loom
source that uses them, and a `pub` function that Rust calls.

```rust
//! An embedded Loom instance with a built-in `Rectangle` value warp: a host
//! method, a host constant, Loom source hardcoded into the module, and a `pub`
//! function that Rust calls.

use std::cell::RefCell;
use std::rc::Rc;

use loom_embed::{Embed, Literal, Scalar};
use loom_sys::Host;

#[test]
fn a_rectangle_built_in_is_used_from_loom_and_called_from_rust() {
    let mut embed = Embed::new("shapes");

    let rectangle = embed.warp("Rectangle", &[("w", "f64"), ("h", "f64")]).expect("declares");
    embed
        .warp_method(&rectangle, "area", &[], "f64", move |cx, r, layout| {
            let bytes = layout.bytes(cx, r[0])?;
            let area = layout.read::<f64>(bytes, 0) * layout.read::<f64>(bytes, 1);
            r[0] = area.to_reg();
            Ok(())
        })
        .expect("registers");

    let one = Literal::of(1.0_f64).expect("a literal");
    embed.constant_warp(&rectangle, "UNIT_SQUARE", &[("w", one.clone()), ("h", one)]).expect("declares");

    embed.source("pub fun doubled_area(r: Rectangle) -> f64 { r.area() * 2.0 }").expect("adds");

    let program = embed
        .compile(
            "use shapes.{Rectangle, UNIT_SQUARE, doubled_area}\npub fun report(w: f64, h: f64) -> f64 {\n    Rectangle(w: w, h: h).area() + doubled_area(UNIT_SQUARE) + doubled_area(Rectangle(w: w, h: h))\n}\n",
        )
        .unwrap_or_else(|d| panic!("{d:?}"));

    let host = Host::captured(Rc::new(RefCell::new(Vec::new())));
    let mut run = program.start(host).expect("starts");
    let out = run.call("report", &[3.0_f64.to_reg(), 2.0_f64.to_reg()]).expect("runs");

    // 3 x 2 = 6, the unit square doubled is 2, and the 3 x 2 rectangle doubled is 12.
    assert_eq!(f64::from_reg(out[0]), 20.0);
}
```

### Walkthrough

1. **`Embed::new("shapes")`** names the module. Loom programs write
   `use shapes.{...}` to reach what it declares.
2. **`embed.warp("Rectangle", ...)`** declares `pub warp Rectangle { pub w: f64,
   pub h: f64 }` in the module, and returns a `WarpRef` for the methods and the
   constant below.
3. **`warp_method(... "area" ...)`** adds `pub fun area(self) -> f64` to
   `Rectangle`. The host closure reads `w` and `h` through the layout and writes
   the product into `r[0]`, the scalar result slot.
4. **`Literal::of` and `constant_warp`** declare
   `pub const UNIT_SQUARE: Rectangle = Rectangle(w: 1.0, h: 1.0)`.
5. **`source`** adds `doubled_area`, a Loom function that calls the host method.
   It runs in the same module, so `Rectangle` and `area` are in scope.
6. **`compile`** checks the user's program: it `use`s the module and exports
   `report`. Errors come back as `Vec<Diagnostic>`.
7. **`program.start(host)`** makes a run. `Host::captured` writes output into a
   buffer, which this test doesn't read.
8. **`run.call("report", ...)`** passes `3.0` and `2.0` as registers and gets
   the result as a register. The total is `6 + 2 + 12 = 20`.

The host has no code for `doubled_area`: it is Loom, compiled with the program,
and it calls back into the host method `area`.

## Where to look next

- `crates/loom_embed/src/lib.rs` documents the facade and has a doc test of the
  lifecycle.
- `crates/loom_embed/tests/` shows each feature in use.
- `crates/loom_compiler/tests/host_extern.rs` shows what the compiler accepts and
  refuses for `extern "host"` functions.
- `crates/loom/tests/host_gate.rs` is the performance gate.
