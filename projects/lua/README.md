# Lua in Loom

A source interpreter for the Lua 5.4 language, implemented in Loom. Each chunk
starts with an empty global table bound to `_ENV`. The guest has no standard
library, including Lua's base library, so there is no `print`, `assert`,
`pairs`, `require`, or `_G`. The CLI handles input and can display the
chunk's return values.

## Run a chunk

From the Loom repository root, using a built `loom` on your PATH:

```sh
loom run projects/lua/main.loom -- --results program.lua one two
```

Use `-` as the file to read source from standard input. Arguments after the
file are strings available to the chunk as `...`. With `--results`, each
returned value occupies one line. The renderer quotes string bytes with
escapes and displays run-local IDs for tables and functions. A chunk that runs
successfully is silent by default. Usage errors exit with status 2. File,
syntax, and runtime errors go to stderr and exit with status 1.

```sh
loom run projects/lua/main.loom -- --results - <<'LUA'
local function counter(start)
  local n = start
  return function(step)
    n = n + step
    return n
  end
end

local next = counter(7)
local first = next(2)
return first, next(5), #"lua"
LUA
```

```text
9
14
3
```

To build a standalone executable with Treadle embedded:

```sh
loom build --exe projects/lua/main.loom -o loom-lua
./loom-lua --results program.lua
```

## Language behavior

The interpreter handles Lua source chunks with byte strings, 64-bit integers
and binary64 floats, tables, lexical bindings and closures, varargs and
multiple results, method-call syntax, numeric and generic `for`, `while`,
`repeat`, block exits, labels, `goto`, and local attributes. It implements
proper tail calls and traces unreachable objects, including cycles.

Metatables, finalizers, coroutines, and userdata need library or host
facilities that this interpreter does not expose. A `<close>` local or
generic loop closing value therefore accepts `nil` or `false`. The CLI loads
source text and supplies no bytecode loader. Source files are read as bytes.
Standard input and command-line arguments use Loom's text interfaces.
Diagnostics, display formatting, and the choice of table border for tables
with holes can differ from PUC Lua. Ordinary calls and deeply nested syntax
are subject to the Loom host's stack limits. See the
[Lua 5.4 language reference](https://www.lua.org/manual/5.4/manual.html#3).

## Implementation

- [lexer.loom](lexer.loom) tokenizes source bytes and records line and column.
  [numbers.loom](numbers.loom) handles numeric literals, conversions, and
  arithmetic.
- [parser.loom](parser.loom) builds the indexed tree in
  [syntax.loom](syntax.loom), resolves variable references to lexical
  bindings, and links and validates jumps before execution.
- [machine.loom](machine.loom) interprets that tree and shares Lua's
  single-result and multiple-result adjustment rules across expressions,
  calls, and assignments. Tail calls reuse one evaluator invocation.
- [heap.loom](heap.loom) models table and closure identity using stable
  handles. At statement boundaries, a tracing collector starts from active
  scopes and temporary values. [strings.loom](strings.loom) interns encoded
  byte strings and keeps literal names and strings alive for the chunk.

```sh
loom test projects/lua/tests.loom
loom test -O0 projects/lua/tests.loom
loom test --backend lua projects/lua/tests.loom
```

The Lua backend is optional for this project. It runs the same Loom
interpreter on Loom's alternate backend. The default Treadle build executes
the interpreter without a Lua VM.
