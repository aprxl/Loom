# Loom in Emacs and Doom

`loom-mode.el` is a self-contained major mode with a lexical highlighting
grammar. It needs Emacs 27.1 or later, with no packages or compiled parsers.
Files ending in `.loom`, including `bolt.loom`, select it automatically.

## Doom installation

Copy `loom-mode.el` into your Doom configuration's `lisp/` directory and add
this to `config.el`:

```elisp
(load! "lisp/loom-mode")
```

Reload your configuration with `M-x doom/reload`, or restart Emacs. An existing
buffer can switch immediately with `M-x loom-mode`. No `doom sync` is needed.
The installed copy is independent; recopy the file after changing this source.

For ordinary Emacs, put the file on `load-path` and `(require 'loom-mode)`.

## Highlighting rules

The lexical rules follow `design/01-syntax.md` through `06-compile-time.md`
and the compiler's lexer and parser:

- Reserved words, PascalCase types, SCREAMING_CASE constants, lowercase
  constructors, `Self`, `self`, holes, fields, `fun` function declarations and calls.
- All integer widths `i1`–`i128` and `u1`–`u128`, pointer-sized integers,
  float types, radix literals, exponents and numeric suffixes. Range dots,
  tuple indices and numeric method receivers keep their token boundaries.
- Overflow operators and casts, optional/result and pointer operators,
  shuttle arrows, attributes and `@region` names.
- Nested block comments and `///` documentation comments.
- Character, ordinary, raw, byte and triple-quoted strings, with escapes.
  Templates color embedded code, nested strings/templates, doubled literal
  braces and format specifications. A colon inside parentheses is code;
  a top-level colon begins the format specification.
- `once` before `warp`, `get` inside `pub(get)`, `take` at the beginning of
  function/lambda parameters, `from` in absorbing case fields, `where` in
  declaration headers/gauges, and `test name(...)` items. These words remain
  identifiers in bindings, named fields, ordinary calls and member access.
- Twill capture letters and wildcard digits in pattern positions. In an
  expression, lowercase hex digits stay numbers. Suffixed and negative hex
  literals stay numbers in patterns as the parser requires.

Standard font-lock faces follow your theme. `loom-number-face`,
`loom-operator-face`, `loom-interpolation-face` and `loom-capture-face` can be
customized with `M-x customize-group RET loom`.

This mode supplies syntax highlighting and comment syntax, not a compiler,
LSP server, formatter or indentation engine. Capitalized names after a dot
use a case/constant face; only semantic resolution can distinguish those from
associated types such as `I.Item`. Pattern and declaration recognition uses
lexical context and balanced groups, rather than a full parser. Incomplete
single-line literals recover at the newline. After an edit the mode rescans
the buffer, so changing an early multiline delimiter updates later faces;
very large generated files may incur more highlighting work.

## Checks

```sh
emacs -Q --batch -L editors/emacs \
  -l editors/emacs/loom-mode.el \
  -l editors/emacs/loom-mode-tests.el \
  -f ert-run-tests-batch-and-exit
```

The ERT suite checks contextual words, template and comment nesting, twill
patterns, numeric boundaries, mode selection, and refontification after edits.
It also fontifies every `.loom` file in `examples/`, `runtime/std/` and `tests/`
and verifies the scanner does not alter source text.
