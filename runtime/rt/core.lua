-- loomrt core.
--
-- Every bundle the Loom compiler emits starts with the runtime (`rt`), which
-- holds the helpers that Loom semantics need on top of plain Lua: checked
-- integer arithmetic, formatting, and unravel reporting. See
-- design/lua-backend.md for what lives here and why.
--
-- Integers. Each Loom integer type has a descriptor made by `rt.int`. Values
-- are Lua integers, always normalized to the type's range. A `u64` above
-- 2^63 is stored as its bit pattern (so it looks negative to Lua) and is
-- compared with `math.ult`.
--
-- Traps. Anything that unravels raises a string that starts with
-- `rt.MARK`, at the position of the Loom code that called into `rt` (the
-- `level` argument of `error`). The host turns that position into a Loom
-- source location through the bundle's source map.

local rt = {}

rt.VERSION = "0.1.0"

local error, tostring, tonumber, type = error, tostring, tonumber, type
local floor, ceil, huge, ult = math.floor, math.ceil, math.huge, math.ult
local maxinteger, mininteger, tointeger = math.maxinteger, math.mininteger, math.tointeger
local sformat, srep, sbyte, spack, sunpack = string.format, string.rep, string.byte, string.pack, string.unpack
local concat = table.concat
local utf8char, utf8len = utf8.char, utf8.len

local MARK = "\31LOOM\31"
rt.MARK = MARK
rt.ult = ult

-- Output goes through the host when there is one (`loom run`), so text
-- reaches the Windows console as UTF-8; plain `io.write` otherwise.
local host = rawget(_G, "__LOOM_HOST")
local write = host and host.write or function(s) io.write(s) end

-- ================================================================ integers

--- The descriptor of an integer type with `bits` bits (1 to 64).
function rt.int(bits, signed, size)
  local d = { kind = "int", bits = bits, signed = signed }
  if signed then
    d.lo = -(1 << (bits - 1))
    d.hi = (1 << (bits - 1)) - 1
  else
    d.lo = 0
    d.hi = bits == 64 and -1 or (1 << bits) - 1
    d.u64 = bits == 64
  end
  d.mask = bits == 64 and -1 or (1 << bits) - 1
  if size then
    d.name = signed and "isize" or "usize"
  else
    d.name = (signed and "i" or "u") .. bits
  end
  -- Whether `a + b` and `a - b` can't overflow a Lua integer for this type.
  d.exact = (signed and bits <= 63) or (not signed and bits <= 62)
  return d
end

-- Two's-complement wrap of any Lua integer into d's range.
local function wrapv(d, r)
  if d.bits == 64 then
    return r
  end
  r = r & d.mask
  if d.signed and r > d.hi then
    r = r - (1 << d.bits)
  end
  return r
end

-- Each *_raw function returns the wrapped Lua result and, when the true
-- result doesn't fit in d, 1 (too big) or -1 (too small).

local function add_raw(d, a, b)
  local r = a + b
  if d.exact then
    if r > d.hi then return r, 1 elseif r < d.lo then return r, -1 end
  elseif d.u64 then
    if ult(r, a) then return r, 1 end
  elseif d.signed then
    if ((a ~ r) & (b ~ r)) < 0 then return r, (a < 0) and -1 or 1 end
  elseif r < 0 then -- usize (u63): the sum passed 2^63
    return r, 1
  end
  return r, nil
end

local function sub_raw(d, a, b)
  local r = a - b
  if d.exact then
    if r > d.hi then return r, 1 elseif r < d.lo then return r, -1 end
  elseif d.u64 then
    if ult(a, b) then return r, -1 end
  elseif d.signed then
    if ((a ~ b) & (a ~ r)) < 0 then return r, (a < 0) and -1 or 1 end
  elseif r < 0 then
    return r, -1
  end
  return r, nil
end

-- The product of two u64 bit patterns, or nil when it doesn't fit in 64 bits.
local function umul(a, b)
  local ah, al = a >> 32, a & 0xFFFFFFFF
  local bh, bl = b >> 32, b & 0xFFFFFFFF
  if ah ~= 0 and bh ~= 0 then return nil end
  local cross = ah * bl + al * bh
  if ult(0xFFFFFFFF, cross) then return nil end
  local low = al * bl
  local r = low + (cross << 32)
  if ult(r, low) then return nil end
  return r
end

-- The exact product if it fits in a Lua integer, else nil.
local function mul64(a, b)
  if a == 0 or b == 0 then return 0 end
  if (a == -1 and b == mininteger) or (b == -1 and a == mininteger) then return nil end
  local r = a * b
  if r // b ~= a then return nil end
  return r
end

local function mul_raw(d, a, b)
  if d.u64 then
    local r = umul(a, b)
    if r == nil then return 0, 1 end
    return r, nil
  end
  local r = mul64(a, b)
  if r == nil then return 0, ((a < 0) ~= (b < 0)) and -1 or 1 end
  if r > d.hi then return r, 1 elseif r < d.lo then return r, -1 end
  return r, nil
end

-- The four overflow policies of one operator: trap (`+`), wrap (`+%`),
-- saturate (`+|`) and checked (`+?`, nil when it doesn't fit).
local function policies(raw, op)
  return function(d, a, b)
    local r, o = raw(d, a, b)
    if o then error(MARK .. "arithmetic overflow", 2) end
    return r
  end, function(d, a, b)
    return wrapv(d, op(a, b))
  end, function(d, a, b)
    local r, o = raw(d, a, b)
    if o then return o > 0 and d.hi or d.lo end
    return r
  end, function(d, a, b)
    local r, o = raw(d, a, b)
    if o then return nil end
    return r
  end
end

rt.add, rt.add_wrap, rt.add_sat, rt.add_chk = policies(add_raw, function(a, b) return a + b end)
rt.sub, rt.sub_wrap, rt.sub_sat, rt.sub_chk = policies(sub_raw, function(a, b) return a - b end)
rt.mul, rt.mul_wrap, rt.mul_sat, rt.mul_chk = policies(mul_raw, function(a, b) return a * b end)

-- Unsigned 64-bit division (Hacker's Delight, via signed operations).
local function udiv(a, b)
  if b < 0 then
    return ult(a, b) and 0 or 1
  end
  local q = ((a >> 1) // b) << 1
  local r = a - q * b
  if not ult(r, b) then q = q + 1 end
  return q
end

--- Integer division, truncating toward zero.
function rt.div(d, a, b)
  if b == 0 then error(MARK .. "division by zero", 2) end
  if d.u64 then return udiv(a, b) end
  if d.signed and b == -1 and a == d.lo then error(MARK .. "arithmetic overflow", 2) end
  local q = a // b
  if q < 0 and q * b ~= a then q = q + 1 end
  return q
end

--- The remainder of truncating division: it has the sign of `a`.
function rt.rem(d, a, b)
  if b == 0 then error(MARK .. "division by zero", 2) end
  if d.u64 then return a - udiv(a, b) * b end
  return math.fmod(a, b)
end

function rt.neg(d, a)
  if a == d.lo then error(MARK .. "arithmetic overflow", 2) end
  return -a
end

function rt.bnot(d, a)
  return wrapv(d, ~a)
end

--- `a << n`: the result is truncated to the type's width.
function rt.shl(d, a, n)
  if n < 0 or n >= d.bits then error(MARK .. "shift amount out of range", 2) end
  return wrapv(d, a << n)
end

--- `a >> n`: arithmetic for signed types, logical for unsigned ones.
function rt.shr(d, a, n)
  if n < 0 or n >= d.bits then error(MARK .. "shift amount out of range", 2) end
  if d.signed and a < 0 then return ~((~a) >> n) end
  return a >> n
end

-- ================================================================ conversions

-- Whether `v`, a value of integer type `s`, is a value of type `d`.
local function fits(d, s, v)
  if s.u64 and v < 0 then return d.u64 end -- 2^63 or more
  if d.u64 then return v >= 0 end
  return v >= d.lo and v <= d.hi
end

--- `v as T`: unravels when the value doesn't fit.
function rt.cast(d, s, v)
  if not fits(d, s, v) then error(MARK .. "value out of range for `" .. d.name .. "`", 2) end
  return v
end

function rt.cast_wrap(d, s, v)
  return wrapv(d, v)
end

function rt.cast_sat(d, s, v)
  if fits(d, s, v) then return v end
  if (s.u64 and v < 0) or v > 0 then return d.hi end
  return d.lo
end

function rt.cast_chk(d, s, v)
  if fits(d, s, v) then return v end
  return nil
end

--- An integer of type `s` as a float.
function rt.itof(s, v)
  if s.u64 and v < 0 then return (v >> 1) * 2.0 + (v & 1) end
  return v + 0.0
end

local TWO64 = 2.0 ^ 64

-- A float truncated toward zero, if it's a value of type d; nil otherwise.
local function ftrunc(d, f)
  if f ~= f then return nil end
  local t = f >= 0 and floor(f) or ceil(f)
  if d.u64 then
    if t < 0 or t >= TWO64 then return nil end
    if t >= 2.0 ^ 63 then return tointeger(t - TWO64) end
    return tointeger(t)
  end
  if t < d.lo or t > d.hi then return nil end
  return tointeger(t)
end

--- `f as T` for a float `f`: truncates, and unravels on NaN or out of range.
function rt.ftoi(d, f)
  local v = ftrunc(d, f)
  if v == nil then error(MARK .. "float out of range for `" .. d.name .. "`", 2) end
  return v
end

--- `f as| T`: saturates, and NaN becomes 0.
function rt.ftoi_sat(d, f)
  if f ~= f then return 0 end
  local v = ftrunc(d, f)
  if v ~= nil then return v end
  return f > 0 and d.hi or d.lo
end

function rt.ftoi_chk(d, f)
  return ftrunc(d, f)
end

--- A float rounded to `f32` precision.
function rt.f32(x)
  return (sunpack("f", spack("f", x)))
end

local function valid_char(s, v)
  if s.u64 and v < 0 then return false end
  return v >= 0 and v <= 0x10FFFF and (v < 0xD800 or v > 0xDFFF)
end

function rt.to_char(s, v)
  if not valid_char(s, v) then error(MARK .. "not a valid `char`", 2) end
  return v
end

function rt.to_char_chk(s, v)
  if valid_char(s, v) then return v end
  return nil
end

-- ================================================================ descriptors for other types

rt.f64 = { kind = "float", name = "f64" }
rt.f32t = { kind = "float", name = "f32" }
rt.bool = { kind = "bool" }
rt.char = { kind = "char" }
rt.str = { kind = "str" }
rt.unit = { kind = "unit" }

function rt.tuple(items)
  return { kind = "tuple", items = items }
end

--- `?T`. `boxed` when T's own values can be nil, so `some(v)` is `{v}`.
function rt.opt(inner, boxed)
  return { kind = "opt", inner = inner, boxed = boxed }
end

--- A value of `any W`: the value with its type's descriptor.
function rt.any(d, v)
  return { v, d }
end

-- Warps. A field is `{key, name, descriptor}`, where the key is the field's
-- name, or its 1-based position in a tuple warp or an unnamed payload.

--- A record or tuple warp: `{x = 1, y = 2}`, `{5.0}`.
function rt.record(name, fields, tuple)
  return { kind = "record", name = name, fields = fields, tuple = tuple }
end

--- A warp with cases: `{tag = 2, mm = 3.5}`. `cases[tag]` is
--- `{name, fields}` (fields is nil for a unit case); `shared` are the fields
--- every case has.
function rt.sum(name, cases, shared)
  return { kind = "sum", name = name, cases = cases, shared = shared }
end

--- A warp whose cases have no payloads and no shared fields: its values are
--- the tags themselves, 1 to n.
function rt.enum(name, names)
  return { kind = "enum", name = name, names = names }
end

--- A deep copy of a value that's a table, following its descriptor. Loom
--- values are copied when assigned (for `Copy` types), but Lua shares tables.
local function copy(d, v)
  local kind = d.kind
  if kind == "tuple" then
    local t = {}
    for i, item in ipairs(d.items) do
      t[i] = copy(item, v[i])
    end
    return t
  elseif kind == "record" then
    local t = {}
    for _, f in ipairs(d.fields) do
      t[f[1]] = copy(f[3], v[f[1]])
    end
    return t
  elseif kind == "sum" then
    local t = { tag = v.tag }
    local case = d.cases[v.tag]
    for _, f in ipairs(case.fields or {}) do
      t[f[1]] = copy(f[3], v[f[1]])
    end
    for _, f in ipairs(d.shared) do
      t[f[1]] = copy(f[3], v[f[1]])
    end
    return t
  elseif kind == "opt" then
    if v == nil then return nil end
    if d.boxed then return { copy(d.inner, v[1]) } end
    return copy(d.inner, v)
  end
  return v
end
rt.copy = copy

--- Replaces a `mut` parameter's table contents with a fresh value's, so the
--- caller sees whole-value assignments (`self = Point(0, 0)`).
function rt.overwrite(dst, src)
  for k in pairs(dst) do
    dst[k] = nil
  end
  for k, v in pairs(src) do
    dst[k] = v
  end
end

--- Structural equality, for types Lua's `==` doesn't compare by value.
local function eq(d, a, b)
  if d.kind == "tuple" then
    for i, item in ipairs(d.items) do
      if not eq(item, a[i], b[i]) then return false end
    end
    return true
  elseif d.kind == "opt" then
    if a == nil or b == nil then return a == b end
    if d.boxed then return eq(d.inner, a[1], b[1]) end
    return eq(d.inner, a, b)
  end
  return a == b
end
rt.eq = eq

-- ================================================================ formatting

local function unsigned_decimal(v)
  if v >= 0 then return tostring(v) end
  local q = udiv(v, 10)
  return tostring(q) .. tostring(v - q * 10)
end

-- A decimal in exponent form ("1.25e-7") written out in full, as Rust's
-- Display does.
local function expand(mantissa, exp)
  local sign = ""
  if mantissa:sub(1, 1) == "-" then
    sign, mantissa = "-", mantissa:sub(2)
  end
  local int, frac = mantissa:match("^(%d*)%.?(%d*)$")
  local digits = int .. frac
  local point = #int + exp
  if point <= 0 then return sign .. "0." .. srep("0", -point) .. digits end
  if point >= #digits then return sign .. digits .. srep("0", point - #digits) end
  return sign .. digits:sub(1, point) .. "." .. digits:sub(point + 1)
end

-- Rust-like float Display: "17", "-3", "1.5", "inf", "NaN". Debug adds ".0"
-- to whole numbers. Never Lua's `tostring`, which differs by platform.
local function fmt_float(v, debug)
  if v ~= v then return "NaN" end
  if v == huge then return "inf" end
  if v == -huge then return "-inf" end
  local s
  if v == floor(v) and v > -1e16 and v < 1e16 then
    s = sformat("%d", v)
    if v == 0 and 1 / v < 0 then s = "-0" end
  else
    for prec = 1, 17 do
      s = sformat("%." .. prec .. "g", v)
      if tonumber(s) == v then break end
    end
    local mantissa, exp = s:match("^(-?[%d%.]+)e([-+]%d+)$")
    if mantissa then s = expand(mantissa, tonumber(exp)) end
  end
  if debug and not s:find("[%.a-zA-Z]") then s = s .. ".0" end
  return s
end

local ESCAPES = { ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t", ["\0"] = "\\0", ["\\"] = "\\\\" }

local function escape(s, quote)
  return (s:gsub("[%c\\\"']", function(c)
    if c == quote then return "\\" .. c end
    if c == "\"" or c == "'" then return c end
    return ESCAPES[c] or sformat("\\u{%x}", sbyte(c))
  end))
end

local show

show = function(d, v, debug)
  local kind = d.kind
  if kind == "int" then
    if d.u64 then return unsigned_decimal(v) end
    return tostring(v)
  elseif kind == "float" then
    return fmt_float(v, debug)
  elseif kind == "bool" then
    return v and "true" or "false"
  elseif kind == "char" then
    if debug then return "'" .. escape(utf8char(v), "'") .. "'" end
    return utf8char(v)
  elseif kind == "str" then
    if debug then return "\"" .. escape(v, "\"") .. "\"" end
    return v
  elseif kind == "unit" then
    return "()"
  elseif kind == "tuple" then
    local parts = {}
    for i, item in ipairs(d.items) do
      parts[i] = show(item, v[i], debug)
    end
    if #parts == 1 then return "(" .. parts[1] .. ",)" end
    return "(" .. concat(parts, ", ") .. ")"
  elseif kind == "opt" then
    if v == nil then return "none" end
    local inner = d.boxed and v[1] or v
    if debug then return "some(" .. show(d.inner, inner, true) .. ")" end
    return show(d.inner, inner, false)
  elseif kind == "record" then
    -- The debug form reads like construction: `Point(x: 3, y: 4)`, `Meters(5)`.
    local parts = {}
    for i, f in ipairs(d.fields) do
      local s = show(f[3], v[f[1]], true)
      parts[i] = d.tuple and s or (f[2] .. ": " .. s)
    end
    return d.name .. "(" .. concat(parts, ", ") .. ")"
  elseif kind == "sum" then
    local case = d.cases[v.tag]
    local parts = {}
    for _, f in ipairs(case.fields or {}) do
      local s = show(f[3], v[f[1]], true)
      parts[#parts + 1] = f[2] and (f[2] .. ": " .. s) or s
    end
    for _, f in ipairs(d.shared) do
      parts[#parts + 1] = f[2] .. ": " .. show(f[3], v[f[1]], true)
    end
    if case.fields == nil and #parts == 0 then return case.name end
    return case.name .. "(" .. concat(parts, ", ") .. ")"
  elseif kind == "enum" then
    return d.names[v]
  end
  error("loom internal: no formatter for kind " .. tostring(kind))
end
rt.show = show

-- Format specs, as in `{x:>8}`, `{pi:.3}`, `{op:04X}`, `{v:?}`:
-- [[fill]align][+][#][0][width][.precision][type].
local specs = {}

local function parse_spec(s)
  local p, i = {}, 1
  local c1, c2 = s:sub(1, 1), s:sub(2, 2)
  if c2 == "<" or c2 == ">" or c2 == "^" then
    p.fill, p.align, i = c1, c2, 3
  elseif c1 == "<" or c1 == ">" or c1 == "^" then
    p.align, i = c1, 2
  end
  if s:sub(i, i) == "+" then p.plus, i = true, i + 1 end
  if s:sub(i, i) == "#" then p.alt, i = true, i + 1 end
  if s:sub(i, i) == "0" then p.zero, i = true, i + 1 end
  local width = s:match("^%d+", i)
  if width then p.width, i = tonumber(width), i + #width end
  if s:sub(i, i) == "." then
    local prec = s:match("^%d+", i + 1) or "0"
    p.prec, i = tonumber(prec), i + 1 + #prec
  end
  local ty = s:sub(i)
  if ty ~= "" then p.ty = ty end
  return p
end

local function radix(d, v, ty, alt)
  local bits = v & d.mask
  local s, prefix
  if ty == "x" then
    s, prefix = sformat("%x", bits), "0x"
  elseif ty == "X" then
    s, prefix = sformat("%X", bits), "0x"
  elseif ty == "o" then
    s, prefix = sformat("%o", bits), "0o"
  else
    local out = {}
    repeat
      out[#out + 1] = (bits & 1 == 1) and "1" or "0"
      bits = bits >> 1
    until bits == 0
    s, prefix = string.reverse(concat(out)), "0b"
  end
  return s, alt and prefix or ""
end

--- A value formatted for a template or `print`, with an optional spec.
function rt.fmt(d, v, spec)
  if spec == nil then return show(d, v, false) end
  local p = specs[spec]
  if p == nil then
    p = parse_spec(spec)
    specs[spec] = p
  end
  local numeric = d.kind == "int" or d.kind == "float"
  local body, prefix = nil, ""
  if d.kind == "int" and p.ty and p.ty ~= "?" then
    body, prefix = radix(d, v, p.ty, p.alt)
  elseif d.kind == "float" and p.prec and v == v and v ~= huge and v ~= -huge then
    body = sformat("%." .. p.prec .. "f", v)
  else
    body = show(d, v, p.ty == "?")
  end
  local sign = ""
  if numeric then
    if body:sub(1, 1) == "-" then
      sign, body = "-", body:sub(2)
    elseif p.plus then
      sign = "+"
    end
  end
  local s = sign .. prefix .. body
  local width = p.width
  if width == nil then return s end
  local len = utf8len(s) or #s
  if len >= width then return s end
  if p.zero and numeric then return sign .. prefix .. srep("0", width - len) .. body end
  local fill = p.fill or " "
  local align = p.align or (numeric and ">" or "<")
  local gap = width - len
  if align == "<" then return s .. srep(fill, gap) end
  if align == ">" then return srep(fill, gap) .. s end
  local left = gap // 2
  return srep(fill, left) .. s .. srep(fill, gap - left)
end

--- Joins the parts of a template.
function rt.cat(...)
  return concat({ ... })
end

-- ================================================================ std externs

-- `extern "lua"` functions of std, by path. The compiler only emits calls to
-- functions listed here (see `lower::EXTERNS`).
rt.ext = {}

rt.ext["std.io.print"] = function(x)
  write(show(x[2], x[1], false) .. "\n")
end

rt.ext["std.error.unravel"] = function(message)
  error(MARK .. message, 2)
end

rt.ext["std.error.assert"] = function(condition, message)
  if not condition then error(MARK .. message, 2) end
end

return rt
