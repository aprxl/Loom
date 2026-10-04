# bench/lexer.loom in idiomatic Python, for comparison (bench/README.md).
# A push lexer as a generator: sent each char (None once the input is
# closed), it yields the tokens it finishes, and NEED when it wants more.
NEED = object()


def lex():
    nxt = yield NEED
    while True:
        c = nxt
        if c is None:
            yield ("eof", None)
            return
        if c in " \t\n":
            nxt = yield NEED
        elif c.isdigit():
            n = 0
            while nxt is not None and nxt.isdigit():
                n = n * 10 + ord(nxt) - 48
                nxt = yield NEED
            yield ("int", n)
        elif c.isalpha() or c == "_":
            parts = []
            while nxt is not None and (nxt.isalnum() or nxt == "_"):
                parts.append(nxt)
                nxt = yield NEED
            yield ("ident", "".join(parts))
        elif c == '"':
            parts = []
            nxt = yield NEED
            while nxt is not None and nxt != '"':
                parts.append(nxt)
                nxt = yield NEED
            nxt = yield NEED
            yield ("str", "".join(parts))
        else:
            nxt = yield NEED
            yield ("punct", c)


line = 'let greeting = "hi there" + 42 * count_7 - (x1 / 1234567);\n'
source = line * 20_000
s = dict(idents=0, ident_chars=0, ints=0, int_sum=0, strs=0, str_chars=0, puncts=0, eofs=0)


def count(tok):
    kind, v = tok
    if kind == "ident":
        s["idents"] += 1
        s["ident_chars"] += len(v)
    elif kind == "int":
        s["ints"] += 1
        s["int_sum"] += v
    elif kind == "str":
        s["strs"] += 1
        s["str_chars"] += len(v)
    elif kind == "punct":
        s["puncts"] += 1
    else:
        s["eofs"] += 1


lexer = lex()
next(lexer)


def push(ch):
    tok = lexer.send(ch)
    while tok is not NEED:
        count(tok)
        try:
            tok = next(lexer)
        except StopIteration:
            return


for ch in source:
    push(ch)
push(None)
print(*s.values())
