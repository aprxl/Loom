// bench/lexer.loom in idiomatic JavaScript, for comparison (bench/README.md).
// A push lexer as a generator: given each char by next() (null once the
// input is closed), it yields the tokens it finishes, and NEED when it
// wants more.
const NEED = Symbol("need");
const isDigit = (c) => c >= "0" && c <= "9";
const isAlpha = (c) => (c >= "a" && c <= "z") || (c >= "A" && c <= "Z");

function* lex() {
  let next = yield NEED;
  for (;;) {
    const c = next;
    if (c === null) {
      yield ["eof"];
      return;
    }
    if (c === " " || c === "\t" || c === "\n") {
      next = yield NEED;
    } else if (isDigit(c)) {
      let n = 0;
      while (next !== null && isDigit(next)) {
        n = n * 10 + (next.charCodeAt(0) - 48);
        next = yield NEED;
      }
      yield ["int", n];
    } else if (isAlpha(c) || c === "_") {
      let word = "";
      while (next !== null && (isAlpha(next) || isDigit(next) || next === "_")) {
        word += next;
        next = yield NEED;
      }
      yield ["ident", word];
    } else if (c === '"') {
      let text = "";
      next = yield NEED;
      while (next !== null && next !== '"') {
        text += next;
        next = yield NEED;
      }
      next = yield NEED;
      yield ["str", text];
    } else {
      next = yield NEED;
      yield ["punct", c];
    }
  }
}

const line = 'let greeting = "hi there" + 42 * count_7 - (x1 / 1234567);\n';
const source = line.repeat(20_000);
const s = { idents: 0, identChars: 0, ints: 0, intSum: 0, strs: 0, strChars: 0, puncts: 0, eofs: 0 };
function count([kind, v]) {
  switch (kind) {
    case "ident": s.idents++; s.identChars += v.length; break;
    case "int": s.ints++; s.intSum += v; break;
    case "str": s.strs++; s.strChars += v.length; break;
    case "punct": s.puncts++; break;
    default: s.eofs++;
  }
}
const lexer = lex();
lexer.next();
function push(ch) {
  let r = lexer.next(ch);
  while (!r.done && r.value !== NEED) {
    count(r.value);
    r = lexer.next();
  }
}
for (const ch of source) push(ch);
push(null);
console.log(Object.values(s).join(" "));
