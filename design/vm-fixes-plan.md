# Fixing the V0/V1 review (design/vm-review-v0-v1.md)

**Status (2026-10-03): all 18 cards are done**, each its own commit, each
noted under its item in the review. What differed from the plan:

- Card 8 put slots over 64 KiB on the heap in codegen (a cell in the frame,
  allocated by the prologue, freed at every return and landing pad's end),
  not in the builder; a frame with its slots after the registers can't work,
  since a callee's frame overlays what's above the call window.
- Card 9: the call through a function value with over 255 arguments was
  already an error (reported as "not implemented yet"); a function with over
  65 535 values panicked in the register allocator. Limits are U0103 now.
- Card 12: `"".split(",")` was already one empty part.
- Found on the way, and fixed in commits of their own: `collect()` leaking its
  partial list when the stream unravelled (card 5), a `for` over a stream of
  Strings dropping the stream every iteration and keeping every binding
  until the block ended (cleanup scopes), and a float exactly halfway
  between two shortest decimals printing differently on Lua (card 18).

Work through the cards **in order**, one card per commit (a card that says
"two commits" means two). Each card names its review item (H1, M5, …); read
that item in `design/vm-review-v0-v1.md` first. When a card is done, add a
line `Fixed: <commit subject>` under its item in the review.

If a card turns out bigger than it says, or a step's premise is wrong (code
not where the card says, a test you can't make pass), **stop and write down
what you found** under the item in the review. Don't improvise a redesign.

## Rules (from AGENTS.md; they always apply)

- Read `AGENTS.md` and `design/vm.md` §5, §8 and §9 before the first card.
- **Gates before every commit:**
  - `cargo fmt --all`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
  - `cargo test -p loom --no-default-features --features lua55`
  - `cargo test -p loom_vm --all-features`
  - `cargo test -p loom --features vm-checked,vm-stats`
- One logical change per commit. Read your own diff first. Push after each
  commit. Never amend, force-push or use `git stash`.
- **Both backends agree.** A behavior change goes into Treadle
  (`crates/loom_vm`, `crates/loom_compiler/src/heddle`) *and* the Lua backend
  (`runtime/rt/core.lua`, `crates/loom_host`), with a `tests/run` fixture
  (`NAME.loom` + `NAME.out`, plus `.unravel` for an unravel).
- **Diagnostic codes** come from `loom_syntax::diag::codes` (codes.rs); never
  invent one inline.
- Every new `unsafe` block gets a `// SAFETY:` comment and an
  `#[allow(unsafe_code)]`.
- **Determinism:** anything whose order can reach output uses
  `IndexMap`/`BTreeMap`.
- **Bench counts:** if `bench/counts.txt` moves more than 2% and you meant
  it, run `LOOM_BLESS=1 cargo test -p loom --features vm-stats --test
  bench_counts` and say why in the commit message.
- **Docs:** keep `design/` in sync. The std docs are generated: edit
  `docs/std/data.py`, then run `python docs/gen.py`.
- **Scripts:** write long scripts to files and run them (heredocs mangle
  backslashes). The shell is Git Bash on Windows; C is built with
  clang-cl/lld-link (already configured).

---

## Card 1 (H6): an interrupt before the Lua watch is lost

- **File:** `crates/loom_sys/src/interrupt.rs`.
- **Change:** in `watch`, after pushing the callback, check `FLAG`. If it's
  set, call `f` once.
- **Avoid deadlock:** don't call `f` while holding `WATCHERS`; release the
  lock first.
- **Test:** in `crates/loom/tests/cli.rs`, next to
  `ctrl_c_unwinds_then_exits_130_on_both_backends`, add a case with
  `LOOM_INTERRUPT_AFTER_MS=0` on a `loop {}` with a `defer`. It must print
  the defer, then exit with 130, on both backends.
- **Done when:** that test passes, and it hung before your change (check by
  reverting the fix locally).

## Card 2 (trap 3.1): the dispatch flags must apply everywhere, or fail loudly

- **Flags for every target:** in `.cargo/config.toml`, move the `rustflags`
  out of `[target.x86_64-pc-windows-msvc]` into a `[target.'cfg(all())']`
  table. Keep the `linker` line where it is.
- **Fail loudly when they're missing:** add a `build.rs` to `crates/loom_vm`
  that reads `CARGO_ENCODED_RUSTFLAGS` and emits
  `cargo:rustc-env=LOOM_VM_RUSTFLAGS=...`.
  - Add a test in `crates/loom_vm` that asserts the value contains
    `tail-dup-pred-size`.
  - Its failure message says that `RUSTFLAGS` in the environment replaces
    the config's flags.
- **Record the build in `bench/RESULTS.md`, under "V1":**
  - the `rustc --version`;
  - that PUC Lua is the vendored 5.4 built with clang-cl, which uses
    `switch` dispatch.
- **Done when:** the gates pass, and the new test fails under
  `RUSTFLAGS=-Cdebuginfo=0 cargo test -p loom_vm` (try it once).

## Card 3 (L items): small no-behavior cleanups

All in one commit; none changes output.

- **`crates/loom_vm/src/natives.rs` ~95–120:** the `# Safety` doc sits on
  `owns`. Move it to `text`, and give `owns` a plain doc.
- **Missing `// SAFETY:` comments:**
  - `crates/loom_host/src/run.rs`, `stop`: the hook's `unsafe extern` fn;
  - `crates/loom_vm/src/methods.rs` ~485: the read in `probe`.
- **`heap.rs` `realloc`, same-class path:** before growing `live`, check
  `new - old <= LIMIT - self.live` (when `new > old`); otherwise
  `Err(Trap::OutOfMemory)`.
- **`methods.rs` `map_find_int`:** `debug_assert!(bytes <= 8)`.
- **`methods.rs` `string_of` / `push_char`:** keep the `'\u{FFFD}'`
  fallback, but add `debug_assert!` that the `char` is valid.
- **Heap unit tests:**
  - `realloc` from small to big and big to small keeps the bytes;
  - `realloc` past `LIMIT` gives `OutOfMemory`.

## Card 4 (H3): stack overflow must be fast

- **Smaller default:** `crates/loom_vm/src/lib.rs`, `Config::default`:
  `stack_limit` 1 GiB → 256 MiB (`256 << 20`). Update `design/vm.md` P7 (§14)
  and §9.2, which say 1 GiB.
- **Bounded trace:** `crates/loom_vm/src/exec.rs`, `trace_at`. Keep at most
  the first 64 and the last 64 located frames.
  - Walk all frames, but push only while `out.len() < 64`, and keep the tail
    in a 64-entry ring buffer (`VecDeque`).
  - Then join head and tail.
  - Check how `Vm::call` (lib.rs) and the `LOOM_BACKTRACE` printer in
    `crates/loom/src/main.rs` show the trace. When frames were skipped, show
    one line `... N frames ...`, which means passing the count along: add a
    field to `Fault`.
- **Done when:** the scratch program below unravels with `stack overflow` in
  under 1 s with `cargo build --release`. It took 5.2 s before. Note the time
  in the commit message.

  ```loom
  use std.io.print
  fun down(n: u64) -> u64 { 1 + down(n + 1) }
  fun main() { print("{down(0)}") }
  ```

  `tests/run/unravel_stack` must still pass.

## Card 5 (M5): check for leaks after every outcome

- **The check:** `crates/loom_vm/src/lib.rs`, `run`. Call `vm.leaks()` for
  every outcome, not only `Ok`/`Failed`: unravel, exit and interrupt too.
  - Wrap it so a leak report replaces the outcome only in the checked build
    (it already returns `None` otherwise).
  - `loom test --backend vm` (`crates/loom/src/main.rs`, which calls
    `Vm::call` per test) should check after each test as well, if that's
    simple.
- **Run the checked gate:** `cargo test -p loom --features
  vm-checked,vm-stats`. Expect failures: each is a real leak on an unwind
  path.
  - If there are none, commit.
  - If there are some, commit the check together with the fixes when each
    fix is small (a missing drop in a landing pad, usually in
    `crates/loom_compiler/src/heddle/build/cleanup.rs` or the pad code of
    `build/mod.rs`).
  - If a fix isn't small, mark that fixture in a list in the review and stop.
- **Also run** `cargo test -p loom --features vm-checked --test gen`, which
  catches the generator's unravels.

## Card 6 (H1): a real hash finalizer

- **What to change:** the one-word path, and the structural mix that has to
  agree with it.
  - `crates/loom_vm/src/methods.rs`: `key_hash`.
  - `crates/loom_compiler/src/heddle/build/map.rs`: `key_hash` (~line 88),
    which builds `h ^ (h >> 32)` in Heddle.
  - Both feed `h & mask`, so both must compute the same thing.
- **The finalizer:** splitmix64's (the same one as `crates/loom/tests/gen.rs`
  `Rng::next`, minus the add):
  - `z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9`
  - `z = (z ^ (z >> 27)) * 0x94d049bb133111eb`
  - `z ^ (z >> 31)`
  - In the native: `key_hash(w) = fmix((OFFSET ^ w) * PRIME)`.
  - In the compiler's `key_hash`, apply the same `fmix` to the structural
    hash instead of the `>> 32` fold. Emit it with the existing helpers
    (`emit1(InstKind::Shr…)`, `Bit Xor`, `Arith Mul Wrap U64`).
- **Hash output changes:** `tests/run/p4_hash.vm.out` (row P4: hashes may
  differ from Lua) will change. Update the file, and keep its comment line.
- **A benchmark:** add `bench/maps.loom` and an idiomatic `bench/lua/maps.lua`
  that print the same.
  - Integer keys `i << 12` (200 000 inserts, then lookups), plus string keys.
  - Add `"maps"` to `BENCHES` in `bench/run.py`, and a line to
    `bench/counts.txt` (bless).
- **Done when:** the `i << 12` case takes under 200 ms on Treadle, release
  build. It was 1966 ms; the Lua backend takes 126 ms. The other benchmarks
  are no slower: run `python bench/run.py --runs 3`.

## Card 7 (H2): O(1) map removal with tombstones (large: two commits)

Do card 6 first.

**Commit A, Treadle** (`crates/loom_compiler/src/heddle/build/map.rs`).

- **Current shape:** the header is 5 words: entries buffer (0), len (8), cap
  (16), index buffer (24), slots (32). An entry starts with its hash word.
- **Find every walk first.** List every place that walks the entries before
  you change anything. Grep `map.rs`, `stream.rs`, `compare.rs`, `glue.rs`
  and `text.rs` for `hdr(header, 8)` and `count_loop` over a map:
  - `drop_entries`, `copy_coll`, `show_entries`, `eq_colls`;
  - the map streams (`map.rs` ~848), `reindex`;
  - `sorted`/`keys`/`values`/`pairs`, and `coll_method`'s cases.
- **The design:**
  - **Hashes:** live entries always have a nonzero hash (OR the hash with 1
    when storing). A dead entry has hash word 0.
  - **Counters:** the word at 8 becomes "used", the entries made, live or
    dead.
    - Add a sixth header word at 40: "live", what `len()` returns.
    - The `Map`/`Set` size in `size_align`/layout must grow to 48 bytes.
      Find where the 40 is stated (grep `40` near `coll`) and keep
      `T.size` for maps consistent with the Lua side (§10.13 of vm.md).
  - **`remove_entry`:** drop the key and value as today, set the hash word
    to 0, `live -= 1`. Leave the index slot pointing at the dead entry;
    `probe` and `find_key` skip it, because the hash compare fails.
  - **When to compact:** if `used - live > live` and `used > 8`, compact.
    Move the live entries down in order, set `used = live`, and `reindex`.
  - **Every walk** skips entries whose hash word is 0.
  - **`push_entry`** appends at `used`.
  - **The native `probe`** (`methods.rs`) needs no change. A dead entry's
    hash is 0, and a live hash never is, but check `h != 0` there anyway.

**Commit B, Lua** (`runtime/rt/core.lua` `take_at`, `map_put`, and every
`for i = 1, m.n` loop over `keys`/`hks`).

- `n` becomes "used"; add `live`. Removal sets `hks[i] = false` and clears
  `idx`.
- Loops skip `hks[i] == false`.
- Compact on the same rule.
- `coll_len` returns `live`.

**Tests:** `tests/run/maps` and `map_edges` must pass unchanged. Add the
fixture `tests/run/map_removals`:

- remove from the front, middle and end;
- re-insert after removal (it goes at the end);
- iterate during build-up;
- remove everything, then insert;
- `==` between maps with different removal histories;
- show / `sorted` / `copy` after removals.

Also add a removal loop to `bench/maps.loom`.

**Done when:** 30 000 removes from a 30 000-entry map take under 100 ms on
Treadle and under 300 ms on the Lua backend. They took 4.8 s and 14.7 s.

## Card 8 (H4): big frame slots go to the heap

- **Find the code first** (it's the builder, not codegen):
  - where a function's frame slots are made: grep `fn raw_slot` and
    `fn slot` in `crates/loom_compiler/src/heddle/build/`;
  - how an owned local is freed on every way out, normal exit and unwind
    (`cleanup.rs`, and how `defer` registers cleanups).
- **The change:** when a slot's size is over 4096 bytes:
  - make it with `mem.alloc(size, align)`, the native
    `InstKind::Native { name: "mem.alloc", .. }` as `map.rs` uses
    `mem.calloc`. Use `mem.calloc` when the slot must start zeroed;
  - register a cleanup that frees it (`mem.free`) on every exit, unwind
    included, the same way a `List` local's buffer is freed;
  - the value that stood for the slot's address is now that pointer.
- **Remaining limit:** in `crates/loom_compiler/src/codegen.rs` (~183, ~213),
  keep the 65 535-register error, but make the text say it's Treadle's frame
  limit ("`f` needs more than 65 535 registers: Treadle's frame limit"), not
  "not implemented yet". `treadle.rs` wraps every codegen error as "not
  implemented yet":
  - give codegen errors a way to say "limit". For example, return an enum,
    or prefix a marker the wrapper checks.
  - Use an existing code from codes.rs if one fits. Otherwise add one in the
    U range, following the file's conventions.
- **Test:** the fixture `tests/run/big_local_array` has `var a = [0u8;
  600000]`, sets and reads elements, passes it to a function by reference,
  and unravels out of a function holding one. The checked build must report
  no leak.
- **Done when:** the fixture passes on both backends and the gates pass.

## Card 9 (M10): no silent truncation in the encoder

Each fix fails compilation with a clear message instead of wrapping.

- **Format items:** `crates/loom_compiler/src/heddle/build/mod.rs` ~1063.
  When `self.fmts.len()` would pass `u16::MAX`, report the frame-limit-style
  error from card 8.
- **`CallR`:** `crates/loom_compiler/src/codegen.rs` ~1690. If `params > 255
  || results > 255`, return an error from `emit_func` (as the
  65 535-register case does).
- **Natives:** `codegen.rs` `native()`. If `self.natives.len() >=
  u16::MAX`, error the same way.
- **Tests:** add codegen unit tests (or `tests/fail` fixtures) for the
  `CallR` case: a lambda taking a 300-element tuple. A 65 537-item fixture
  is too big; a unit test on the builder function is enough.

## Card 10 (H5): Ctrl-C inside a coroutine on the Lua backend

- **The bug:** hooks are per `lua_State`. `watch_interrupt`
  (`crates/loom_host/src/run.rs`) sets one only on the main state, and a
  coroutine that's already running never gets it. Coroutines created after
  the hook is set inherit it.
- **The design:**
  1. **Registry:** a `Mutex<Vec<usize>>` of live coroutine states, shared by
     the host and the watcher closure.
  2. **Registering:** a host C function, made with `lua.create_c_function`
     or the raw ffi. It takes the new coroutine as its argument, gets its
     state with `mlua::ffi::lua_tothread(state, 1)`, and pushes the pointer.
     A second function removes it.
  3. **Runtime side:** in `runtime/rt/core.lua`, around `co_create` and
     `resume` (~1285–1330):
     - register each coroutine when it's created;
     - keep a strong reference to it in an rt table while it's registered,
       so it can't be collected while its pointer is in the list;
     - unregister it (and drop the reference) when it finishes, errors, or
       is closed.
  4. **The watcher** sets the `stop` hook on the main state and on every
     registered state.
- **Test:** a cli.rs test. A shuttle whose body loops without yielding,
  consumed by a `for` in `main`, with a `defer` in both and
  `LOOM_INTERRUPT_AFTER_MS=300`. Lua backend only: Treadle can't run
  shuttles until V2. It must exit with 130 within 5 s, running both defers.
- **Docs:** fix `design/lua-backend.md`, which still says the interrupt
  "checks every 10 000 instructions". Describe the async hook, and how it
  reaches coroutines.

## Card 11 (M1): keep files open

- **Treadle** (`crates/loom_vm/src/host.rs`):
  - keep a `HashMap<String, std::fs::File>` of open append handles in `Ctx`
    (`natives.rs`);
  - `append()` reuses the handle;
  - `fs_read` of a path with an open handle flushes it first;
  - handles are dropped (closed) when the `Ctx` drops; that's enough,
    `File` writes aren't buffered by Rust.
- **The Lua host:** grep `crates/loom_host/src` for the `fs` / `write_line`
  host functions and do the same.
- **Done when:** the 20 000-line `File.write_line` loop below goes from
  1.7 s to under 200 ms on both backends, and `tests/run` (sandboxed files)
  still passes.

  ```loom
  use std.io.print
  use std.fs.{File, IoError}
  fun run() -> () ! IoError {
      var f = File.append("out.txt")?
      var i = 0u32
      while i < 20000 { f.write_line("line {i}")?; i += 1 }
  }
  fun main() { run() mend e => print("failed"); print("done") }
  ```

## Card 12 (M2): empty patterns behave as in Rust

- **`replace("", to)`:** `to` goes before every char and at the end. For
  example, `"héllo".replace("", "-")` is `-h-é-l-l-o-`.
  - Treadle: `methods.rs` `replace`, insert at char boundaries.
  - Lua: `core.lua` `Str.replace`, which returns `s` unchanged today. Use
    `utf8.codes`.
- **`"".split(",")`:** check first with `.len()`. It's probably already `[""]`
  (a one-empty-string list shows as `[]`). If so, change nothing; write that
  down in the review.
- **`split("")`:** keep today's one-piece-per-char behavior. Rust's adds
  empty first and last pieces; note this as a deliberate difference in the
  std docs.
- **Docs and test:** document both in `docs/std/data.py`, regenerate, and add
  the cases to `tests/run/strings` or a new fixture.

## Card 13 (M3): document ASCII text methods

- **The docs:** in `docs/std/data.py`, give `to_upper`, `to_lower`, `trim*`,
  `is_whitespace` and the `char` methods (`is_alphabetic`,
  `is_alphanumeric`, `is_upper`, `is_lower`, `to_upper`, `to_lower`) the
  words "ASCII only: other characters are left as they are" (or "are
  `false`").
- **Check the claim on both backends:** first check what `'é'.is_alphabetic()`
  does on each. If they differ, make both ASCII (that's a fix: add a
  fixture).
- **Regenerate and check the guide:** run `python docs/gen.py`, then
  `python docs/gen.py --check --loom target/debug/loom.exe`.

## Card 14 (M8): exit cleanly on the hard paths

- **Raw mode is always undone:** `crates/loom_sys/src/term.rs`. Add `impl
  Drop for Term` that calls `restore()`.
- **Second Ctrl-C:** `crates/loom_sys/src/interrupt.rs`, before
  `std::process::exit(130)`. Call `crossterm::terminal::disable_raw_mode()`
  (ignore the result), and flush stdout.
- **Panics:** `crates/loom/src/main.rs`. Install a panic hook that disables
  raw mode, then chains to the default hook. A panic exits with
  `EXIT_INTERNAL` (70, already defined there) instead of `ExitCode::FAILURE`.
- **Docs:** the exit-code table in `design/lua-backend.md` notes that
  `process.exit(n)` passes `n` through, even 1/101/130.

## Card 15 (M7): Ctrl-C reaches long natives

- **`time.sleep`:** in `crates/loom_vm/src/host.rs`, sleep in slices of at
  most 50 ms. Between slices, check the interrupt flag. Give `Ctx` the
  `Option<Arc<AtomicBool>>` from `Config`, through `Machine::load`. On
  interrupt, return `Err(Trap::Interrupt)`.
  - **Avoid a double report:** `exec.rs`'s `env.stopping` is set when the
    loop raises the interrupt. Move `stopping` into `Ctx`, so a native and
    the loop share it.
  - **The Lua host's sleep** gets the same treatment (it's in
    `crates/loom_host`).
- **`probe` in `methods.rs`:** bound the loop to `slots` iterations. After
  that, return `u64::MAX`; the index is never full by construction.

## Card 16 (M11, M12): state between calls

- **Fresh state per call:** `crates/loom_vm/src/lib.rs` `Vm::call`, or
  `exec.rs` `Machine::call`. Before each call, clear `ctx.msg` and
  `ctx.bufs`, moving the buffers to `spare`.
- **M12, look first:** check, in `crates/loom_compiler/src/heddle/build/text.rs`
  and wherever `msg.str`/`msg.fmt` are emitted, whether every argument of an
  unravel message is evaluated before the first `msg.*` call.
  - If so, say so in a comment there.
  - If not, write it down in the review (don't fix it here).
- **Test:** a `loom test --backend vm` case in `tests/test/`. Copy how the
  existing ones are laid out: an early test unravels in the middle of
  building a message (through a `defer` that unravels), and a later test's
  unravel message comes out clean.

## Card 17 (§5): docs drift

Fix each item in review §5:

- **`exec.rs` module header:** only `pc`/`fp` are in locals; the rest is in
  `Env`.
- **`design/vm.md` §9.1, §9.5:**
  - the tick is a field of `Env`;
  - describe the labeled trap block;
  - the tail-dup flags.
- **`design/vm.md` §19:** say Miri hasn't been run.
- **AGENTS.md item 23:** keep "leaks nothing" only if card 5 made it true
  for unravels.

## Card 18 (trap 3.4): extend the program generator (large)

- **File:** `crates/loom/tests/gen.rs`. Read it whole first.
- **What to add**, each behind its own `rng.chance`, one at a time:
  1. `String` values: literals, templates, `push_str`, `len`, comparison;
  2. `List[String]`: push, index, assign, `remove`;
  3. `Map[String, i32]` and `Map[i32, String]`: insert, `[]=`, `remove`,
     `has`, `get`, iteration;
  4. lambdas that capture locals, passed to `map`/`filter`/`each`.
- **After each kind:** run 300 seeds normally, then
  `cargo test -p loom --features vm-checked --test gen -- --ignored` for the
  20 000 seeds.
- **Commits:** one per kind if any of them finds bugs. Each bug found is its
  own fix commit, with the shrunk program kept as a `tests/run` fixture (the
  generator prints it).
