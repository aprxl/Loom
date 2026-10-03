-- bench/lexer.loom in idiomatic Lua 5.4: a push lexer in a coroutine,
-- resumed with each char, which yields the tokens it finishes and `NEED`
-- when it wants the next char (`nil` once the input is closed).
local NEED = {}

local function is_digit(c) return c >= "0" and c <= "9" end
local function is_alpha(c) return (c >= "a" and c <= "z") or (c >= "A" and c <= "Z") end
local function is_alnum(c) return is_alpha(c) or is_digit(c) end

local function lex()
  local yield = coroutine.yield
  local next = yield(NEED)
  while true do
    local c = next
    if c == nil then
      yield("eof")
      return
    end
    if c == " " or c == "\t" or c == "\n" then
      next = yield(NEED)
    elseif is_digit(c) then
      local n = 0
      while next ~= nil and is_digit(next) do
        n = n * 10 + (next:byte() - 48)
        next = yield(NEED)
      end
      yield("int", n)
    elseif is_alpha(c) or c == "_" then
      local parts = {}
      while next ~= nil and (is_alnum(next) or next == "_") do
        parts[#parts + 1] = next
        next = yield(NEED)
      end
      yield("ident", table.concat(parts))
    elseif c == '"' then
      local parts = {}
      next = yield(NEED)
      while next ~= nil and next ~= '"' do
        parts[#parts + 1] = next
        next = yield(NEED)
      end
      next = yield(NEED)
      yield("str", table.concat(parts))
    else
      next = yield(NEED)
      yield("punct", c)
    end
  end
end

return function()
  local line = 'let greeting = "hi there" + 42 * count_7 - (x1 / 1234567);\n'
  local source = string.rep(line, 20000)
  local s = { idents = 0, ident_chars = 0, ints = 0, int_sum = 0, strs = 0, str_chars = 0, puncts = 0, eofs = 0 }
  local function count(kind, v)
    if kind == "ident" then
      s.idents = s.idents + 1
      s.ident_chars = s.ident_chars + #v
    elseif kind == "int" then
      s.ints = s.ints + 1
      s.int_sum = s.int_sum + v
    elseif kind == "str" then
      s.strs = s.strs + 1
      s.str_chars = s.str_chars + #v
    elseif kind == "punct" then
      s.puncts = s.puncts + 1
    else
      s.eofs = s.eofs + 1
    end
  end
  local co = coroutine.create(lex)
  local resume = coroutine.resume
  resume(co)
  -- Each char resumes the lexer, which yields tokens until it needs more.
  local function push(ch)
    local _, kind, v = resume(co, ch)
    while kind ~= NEED and kind ~= nil do
      count(kind, v)
      _, kind, v = resume(co)
    end
  end
  for i = 1, #source do
    push(source:sub(i, i))
  end
  push(nil)
  io.write(s.idents, " ", s.ident_chars, " ", s.ints, " ", s.int_sum, " ", s.strs, " ", s.str_chars, " ",
    s.puncts, " ", s.eofs, "\n")
end
