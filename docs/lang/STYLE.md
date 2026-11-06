# How the book is written

The language guide (`docs/lang/pages/*.md`) is a small textbook: it teaches
Loom one feature at a time, with a live sample for each, and says how other
languages do the same thing. `00-index.md` and `01-start.md` are the model
for everything below: read them first.

## Voice

- Warm, plain and direct, in the second person: you're showing a friend
  around. Curious rather than salesy. No exclamation marks in a row, no
  "simply", no "powerful", no "blazingly".
- Short paragraphs (two to five sentences). One idea per paragraph.
- A weaving image now and then (threads, cloth, the loom), where it helps
  the idea land; never more than one or two a chapter, never forced.
- Precise. Every claim about Loom is true *today*: check it by running code
  (`target/debug/loom.exe run FILE`) or in `AGENTS.md` (Status), `LEDGER.md`
  and `design/*.md`. If Loom doesn't do something yet, say so plainly and
  link [Not supported yet](page:not-yet). Never describe a feature you
  haven't seen work.

## A chapter

1. An opening paragraph or two: what this chapter is about and why you'd
   care. What problem does the feature solve?
2. One `##` section per feature (each becomes a numbered bead on the
   thread). In each, usually in this order:
   - the idea, in prose, before any code;
   - an `@sample` that shows it (a real program; see below);
   - the rules, as a short bullet list, if there are several;
   - a `@try` box: one concrete edit to make to the sample above it and
     what to look for ("Change `let` to `var`... the compiler says
     M0101"). Encourage breaking things: diagnostics are part of the
     language;
   - an `@compare` box ("Elsewhere"), where a comparison teaches something:
     two to four languages, each tab one or two sentences on the
     *difference*, then a short idiomatic snippet. Not every section needs
     one; most chapters have three to five.
3. `## Where next` at the end, linking the next chapter.

`###` subsections are fine for smaller points. `> ` notes are for asides.

## Samples

- Every piece of Loom on a page is a sample: `docs/lang/samples/NAME.loom`,
  shown with `@sample NAME`. Its last line says what happens to it:
  `//@ run`, `//@ unravel`, `//@ error`, `//@ check`, `//@ check-fail CODE...`,
  `//@ test`, `//@ test-fail` (see `docs/samples.py`). Use the chapter's
  prefix for names (`values_`, `types_`, `own_`, ...). Each sample is shown
  by exactly one page.
- Short and focused: about 10 to 35 lines, one feature, output that shows
  the point. A few comments where they help; the prose does the explaining.
- Every sample runs in the reader's browser, edited or not, so it should
  invite tinkering: obvious things to change, output that changes visibly.
- Make it, run it, then record what it prints:

  ```
  target/debug/loom.exe run docs/lang/samples/NAME.loom
  python docs/gen.py --bless --only NAME --loom target/debug/loom.exe
  python docs/gen.py --check --only NAME,OTHER --loom target/debug/loom.exe
  ```

  `--bless` writes `NAME.out`. Read it: the page shows it under the code.
- To stop showing a sample, delete its `@sample` line, its `.loom` and its
  `.out` together.

## Elsewhere boxes

```
@compare
-- Rust
One or two sentences on how Rust differs, and why.
```rust
fn main() {}
```
-- Python
...
@end
```

- Idiomatic, minimal, correct code in the other language: the snippet a
  good programmer in that language would write. Fences name the language:
  rust, go, swift, kotlin, python, typescript, java, csharp, c, cpp,
  haskell, ocaml, lua, zig, elixir.
- Respectful: say what each language trades for what. Point out where
  Loom borrowed an idea.
- Pick the languages that teach the most for that feature (Rust for
  ownership, Swift/Kotlin for optionals, Python for generators, Go for
  threads...), not the same three every time.

## Markup

The page format is at the top of `docs/lang_pages.py`: `## h2`, `### h3`,
`- bullets`, `1. lists`, `| tables |`, `> notes`, `@try`, `@sample`,
`@compare`/`@end`, `` `code` ``, `**bold**`, `*italics*`, and links:
`[text](page:slug)`, `[text](page:slug#heading-anchor)`,
`[text](std:module)`, `[text](std:module#Item)`, `[text](diag:K0101)`.
A heading's anchor is its text, lower-cased, with runs of other characters
turned into `-`.

Run `python docs/gen.py` to build the pages (it fails on a broken link or
a missing sample), and preview them over `http://localhost:8878/` if the
docs server is running.
