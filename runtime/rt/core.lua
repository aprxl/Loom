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

--- `T ! E`: `{true, v}` or `{false, e}`.
function rt.res(ok, err)
  return { kind = "res", ok = ok, err = err }
end

--- A value of `any W`: the value with its type's vtable, which holds the
--- type's descriptor (`vt.d`) and, by the weft method's number, the function
--- for each method called through `any`.
function rt.any(vt, v)
  return { v, vt }
end

--- The descriptor of `any W` types: the value's own is in its vtable.
rt.anyd = { kind = "any" }

--- A derived `hash`: FNV-1a (64-bit) of the value's debug text, which is
--- the same for equal values.
function rt.hash(d, v)
  local h = -3750763034362895579 -- 0xcbf29ce484222325
  local s = show(d, v, true)
  for i = 1, #s do
    h = (h ~ sbyte(s, i)) * 1099511628211
  end
  return h
end

-- Gauges. `check` is the gauge's own test, which the compiler writes.

--- `v as G`: `v`, if it's in the gauge.
function rt.into_gauge(check, v, name)
  if not check(v) then error(sformat("%svalue out of range for `%s`", MARK, name), 2) end
  return v
end

--- `v as? G`: `v` if it's in the gauge (and there is one), `nil` if not.
function rt.gauge_opt(check, v)
  if v ~= nil and check(v) then return v end
  return nil
end

--- `v as| G`: the nearest value from `lo` to `hi` (either may be nil: no
--- bound). NaN goes to the lower bound, as it does to 0 for integers.
function rt.clamp(d, v, lo, hi)
  if v ~= v then return lo or hi or 0.0 end
  if lo ~= nil and rt.cmp(d, v, lo) < 0 then return lo end
  if hi ~= nil and rt.cmp(d, hi, v) < 0 then return hi end
  return v
end

-- `defer`: the deferred code, in a value for a `<close>` local, which Lua
-- closes however its block ends (an error included).
local deferred = { __close = function(d) d[1]() end }

function rt.defer(f)
  return setmetatable({ f }, deferred)
end

--- Values nothing looks inside, like functions: shared, never copied.
rt.opaque = { kind = "opaque" }

--- Calls weft method number `k` of `any` value `x`.
function rt.dyn(x, k, ...)
  return x[2][k](x[1], ...)
end

-- Warps. A field is `{key, name, descriptor}`, where the key is the field's
-- name, or its 1-based position in a tuple warp or an unnamed payload.
--
-- A warp that writes its own `Show`, `Eq` or `Ord` method gets it stored in
-- its descriptor (`d.show(v, f)`, `d.eq(a, b)`, `d.lt(a, b)`), and `{x}`,
-- `==` and `<` call it. Without one they work field by field.

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

-- Sequences. An array and a list are `{n = len, [1..n]}`; a slice is a view
-- `{t = elements, o = offset, n = len}` of one. Lengths live in `n`, never in
-- Lua's `#`, since an element can be nil.

--- `[T; N]` and `List[T]`.
function rt.seq(elem)
  return { kind = "seq", elem = elem }
end

--- `[T]`.
function rt.slice(elem)
  return { kind = "slice", elem = elem }
end

--- The elements table, offset and length of any sequence.
local function parts(s)
  local t = s.t
  if t then return t, s.o, s.n end
  return s, 0, s.n
end

--- Checks index `i` against length `n`, and makes it 1-based.
function rt.idx(i, n)
  if i >= n then error(sformat("%sindex %d is out of bounds (len %d)", MARK, i, n), 2) end
  return i + 1
end

--- A view of all of an array or a list.
function rt.view(s)
  return { t = s, o = 0, n = s.n }
end

--- `xs[lo..hi]`: a view of part of a sequence. `hi` is nil for `xs[lo..]`.
function rt.subview(s, lo, hi)
  local t, o, n = parts(s)
  if hi == nil then hi = n end
  if lo > hi or hi > n then
    error(sformat("%srange %d..%d is out of bounds (len %d)", MARK, lo, hi, n), 2)
  end
  return { t = t, o = o + lo, n = hi - lo }
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
  elseif kind == "res" then
    if v[1] then return { true, copy(d.ok, v[2]) } end
    return { false, copy(d.err, v[2]) }
  elseif kind == "seq" then
    local n, elem = v.n, d.elem
    local t = { n = n }
    for i = 1, n do
      t[i] = copy(elem, v[i])
    end
    return t
  elseif kind == "map" or kind == "set" then
    return rt.copy_coll(d, v)
  end
  return v
end
rt.copy = copy

--- `n` copies of `v`.
function rt.filled(d, v, n)
  local t = { n = n }
  for i = 1, n do
    t[i] = copy(d, v)
  end
  return t
end

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

local eq

--- Whether every field in `fields` is equal in `a` and `b`.
local function fields_eq(fields, a, b)
  for _, f in ipairs(fields) do
    if not eq(f[3], a[f[1]], b[f[1]]) then return false end
  end
  return true
end

--- Equality by value, for types Lua's `==` doesn't compare that way.
eq = function(d, a, b)
  local kind = d.kind
  if kind == "tuple" then
    for i, item in ipairs(d.items) do
      if not eq(item, a[i], b[i]) then return false end
    end
    return true
  elseif kind == "opt" then
    if a == nil or b == nil then return a == b end
    if d.boxed then return eq(d.inner, a[1], b[1]) end
    return eq(d.inner, a, b)
  elseif kind == "res" then
    if a[1] ~= b[1] then return false end
    return eq(a[1] and d.ok or d.err, a[2], b[2])
  elseif kind == "seq" or kind == "slice" then
    local at, ao, an = parts(a)
    local bt, bo, bn = parts(b)
    if an ~= bn then return false end
    for i = 1, an do
      if not eq(d.elem, at[ao + i], bt[bo + i]) then return false end
    end
    return true
  elseif kind == "map" or kind == "set" then
    -- The same keys (and values for them), in any order.
    if a.n ~= b.n then return false end
    for i = 1, a.n do
      local j = b.idx[a.hks[i]]
      if j == nil then return false end
      if kind == "map" and not eq(d.val, a.vals[i], b.vals[j]) then return false end
    end
    return true
  elseif d.eq then
    return d.eq(a, b)
  elseif kind == "record" then
    return fields_eq(d.fields, a, b)
  elseif kind == "sum" then
    if a.tag ~= b.tag then return false end
    return fields_eq(d.cases[a.tag].fields or {}, a, b) and fields_eq(d.shared, a, b)
  end
  return a == b
end
rt.eq = eq

local cmp

--- Compares field by field: the first that differs decides.
local function fields_cmp(fields, a, b)
  for _, f in ipairs(fields) do
    local c = cmp(f[3], a[f[1]], b[f[1]])
    if c ~= 0 then return c end
  end
  return 0
end

--- -1, 0 or 1 as `a` comes before, with, or after `b`: `<` and friends on
--- types Lua can't order itself.
cmp = function(d, a, b)
  local kind = d.kind
  if d.lt then
    if d.lt(a, b) then return -1 end
    if d.lt(b, a) then return 1 end
    return 0
  elseif kind == "tuple" then
    for i, item in ipairs(d.items) do
      local c = cmp(item, a[i], b[i])
      if c ~= 0 then return c end
    end
    return 0
  elseif kind == "opt" then
    if a == nil or b == nil then
      if a == b then return 0 end
      return a == nil and -1 or 1
    end
    if d.boxed then return cmp(d.inner, a[1], b[1]) end
    return cmp(d.inner, a, b)
  elseif kind == "record" then
    return fields_cmp(d.fields, a, b)
  elseif kind == "sum" then
    if a.tag ~= b.tag then return a.tag < b.tag and -1 or 1 end
    local c = fields_cmp(d.cases[a.tag].fields or {}, a, b)
    if c ~= 0 then return c end
    return fields_cmp(d.shared, a, b)
  elseif kind == "int" and d.u64 then
    if a == b then return 0 end
    return ult(a, b) and -1 or 1
  end
  if a < b then return -1 end
  if b < a then return 1 end
  return 0
end
rt.cmp = cmp

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
-- to whole numbers. Never Lua's `tostring`, which differs by platform. An
-- `f32` (`single`) gets the shortest digits that read back as the same f32.
local function fmt_float(v, debug, single)
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
      local back = tonumber(s)
      if single then back = rt.f32(back) end
      if back == v then break end
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
    return fmt_float(v, debug, d.name == "f32")
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
  elseif kind == "any" then
    return show(v[2].d, v[1], debug)
  elseif kind == "res" then
    if v[1] then return "ok(" .. show(d.ok, v[2], true) .. ")" end
    return "err(" .. show(d.err, v[2], true) .. ")"
  elseif kind == "seq" or kind == "slice" then
    local t, o, n = parts(v)
    local out = {}
    for i = 1, n do
      out[i] = show(d.elem, t[o + i], debug)
    end
    return "[" .. concat(out, ", ") .. "]"
  elseif kind == "map" then
    if v.n == 0 then return "[:]" end
    local out = {}
    for i = 1, v.n do
      out[i] = show(d.key, v.keys[i], debug) .. ": " .. show(d.val, v.vals[i], debug)
    end
    return "[" .. concat(out, ", ") .. "]"
  elseif kind == "set" then
    local out = {}
    for i = 1, v.n do
      out[i] = show(d.key, v.keys[i], debug)
    end
    return "{" .. concat(out, ", ") .. "}"
  elseif kind == "tuple" then
    local parts = {}
    for i, item in ipairs(d.items) do
      parts[i] = show(item, v[i], debug)
    end
    if #parts == 1 then return "(" .. parts[1] .. ",)" end
    return "(" .. concat(parts, ", ") .. ")"
  elseif kind == "opt" then
    if v == nil then return "none" end
    -- Not `d.boxed and v[1] or v`: v[1] can be nil (`some(none)`).
    local inner = v
    if d.boxed then inner = v[1] end
    if debug then return "some(" .. show(d.inner, inner, true) .. ")" end
    return show(d.inner, inner, false)
  elseif not debug and d.show then
    -- A hand-written `show` writes into a buffer (a `Fmt`).
    local f = {}
    d.show(v, f)
    return concat(f)
  elseif kind == "record" then
    -- The debug form reads like construction: `Point(x: 3, y: 4)`, `Meters(5)`.
    -- A derived `Show` writes the same.
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
    -- Fields every case shares are only in the debug form.
    if debug then
      for _, f in ipairs(d.shared) do
        parts[#parts + 1] = f[2] .. ": " .. show(f[3], v[f[1]], true)
      end
    end
    if case.fields == nil and #parts == 0 then return case.name end
    return case.name .. "(" .. concat(parts, ", ") .. ")"
  elseif kind == "enum" then
    return d.names[v]
  end
  error("loom internal: no formatter for kind " .. tostring(kind))
end
rt.show = show

--- A derived `Show.show`: writes `v`'s own form into `f`.
function rt.write_show(d, v, f)
  f[#f + 1] = show(d, v, false)
end

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
  write(show(x[2].d, x[1], false) .. "\n")
end

rt.ext["std.fmt.Fmt.put"] = function(f, x)
  f[#f + 1] = show(x[2].d, x[1], false)
end

rt.ext["std.env.get"] = function(name)
  if host then return host.getenv(name) end
  return os.getenv(name)
end

rt.ext["std.env.args"] = function()
  local src = host and host.args or { [1] = "main" }
  local args = { n = 0 }
  for i, a in ipairs(src) do
    args[i] = a
    args.n = i
  end
  return args
end

-- Waiting and the terminal need the host (`loom run`); a sandboxed run
-- doesn't wait, and has no keys held.
local function needs_host(what)
  error(MARK .. what .. " needs `loom run`: this Lua has no Loom host", 3)
end

rt.ext["std.time.sleep"] = function(d)
  if not host then needs_host("time.sleep") end
  host.sleep(d.nanos)
end

rt.ext["std.term.raw_mode"] = function(on)
  if not host then needs_host("term.raw_mode") end
  local problem = host.raw_mode(on)
  if problem then error(MARK .. problem, 2) end
end

rt.ext["std.term.poll_keys"] = function(keymap)
  if not host then needs_host("term.poll_keys") end
  local bits, problem = host.poll_keys(keymap)
  if problem then error(MARK .. problem, 2) end
  return bits
end

rt.ext["std.error.unravel"] = function(message)
  error(MARK .. message, 2)
end

rt.ext["std.error.assert"] = function(condition, message)
  if not condition then error(MARK .. message, 2) end
end

-- List and slice methods. Each gets the element type's descriptor first.

--- `v` as a `?T` when `d` describes T: boxed when nil is one of T's values.
local function some(d, v)
  if d.kind == "opt" or d.kind == "unit" then return { v } end
  return v
end

local function out_of_bounds(i, n)
  error(sformat("%sindex %d is out of bounds (len %d)", MARK, i, n), 3)
end

local ext = rt.ext

ext["std.list.List.filled"] = rt.filled

ext["std.list.List.push"] = function(_, s, x)
  local n = s.n + 1
  s[n] = x
  s.n = n
end

ext["std.list.List.pop"] = function(d, s)
  local n = s.n
  if n == 0 then return nil end
  local x = s[n]
  s[n] = nil
  s.n = n - 1
  return some(d, x)
end

ext["std.list.List.insert"] = function(_, s, i, x)
  local n = s.n
  if i > n then out_of_bounds(i, n) end
  for j = n, i + 1, -1 do
    s[j + 1] = s[j]
  end
  s[i + 1] = x
  s.n = n + 1
end

ext["std.list.List.remove"] = function(_, s, i)
  local n = s.n
  if i >= n then out_of_bounds(i, n) end
  local x = s[i + 1]
  for j = i + 1, n - 1 do
    s[j] = s[j + 1]
  end
  s[n] = nil
  s.n = n - 1
  return x
end

ext["std.list.List.clear"] = function(_, s)
  for i = 1, s.n do
    s[i] = nil
  end
  s.n = 0
end

ext["std.list.List.truncate"] = function(_, s, k)
  local n = s.n
  if k >= n then return end
  for i = k + 1, n do
    s[i] = nil
  end
  s.n = k
end

ext["std.list.List.extend"] = function(d, s, xs)
  local t, o, m = parts(xs)
  local items = {}
  for i = 1, m do
    items[i] = copy(d, t[o + i])
  end
  local n = s.n
  for i = 1, m do
    s[n + i] = items[i]
  end
  s.n = n + m
end

local function to_list(d, s)
  local t, o, n = parts(s)
  local out = { n = n }
  for i = 1, n do
    out[i] = copy(d, t[o + i])
  end
  return out
end

ext["std.list.List.copy"] = to_list
ext["std.slice.Slice.to_list"] = to_list

ext["std.slice.Slice.len"] = function(_, s)
  return s.n
end

ext["std.slice.Slice.is_empty"] = function(_, s)
  return s.n == 0
end

ext["std.slice.Slice.get"] = function(d, s, i)
  local t, o, n = parts(s)
  if i >= n then return nil end
  return some(d, copy(d, t[o + i + 1]))
end

ext["std.slice.Slice.first"] = function(d, s)
  local t, o, n = parts(s)
  if n == 0 then return nil end
  return some(d, copy(d, t[o + 1]))
end

ext["std.slice.Slice.last"] = function(d, s)
  local t, o, n = parts(s)
  if n == 0 then return nil end
  return some(d, copy(d, t[o + n]))
end

ext["std.slice.Slice.index_of"] = function(d, s, x)
  local t, o, n = parts(s)
  for i = 1, n do
    if eq(d, t[o + i], x) then return i - 1 end
  end
  return nil
end

ext["std.slice.Slice.contains"] = function(d, s, x)
  return ext["std.slice.Slice.index_of"](d, s, x) ~= nil
end

ext["std.slice.Slice.copy_from"] = function(d, s, src)
  local t, o, n = parts(s)
  local st, so, sn = parts(src)
  if sn > n then
    error(sformat("%scan't copy %d elements into a slice of %d", MARK, sn, n), 2)
  end
  -- Through a buffer, in case the two overlap.
  local items = {}
  for i = 1, sn do
    items[i] = copy(d, st[so + i])
  end
  for i = 1, sn do
    t[o + i] = items[i]
  end
end

ext["std.slice.Slice.fill"] = function(d, s, x)
  local t, o, n = parts(s)
  for i = 1, n do
    t[o + i] = copy(d, x)
  end
end

ext["std.slice.Slice.swap"] = function(_, s, i, j)
  local t, o, n = parts(s)
  if i >= n then out_of_bounds(i, n) end
  if j >= n then out_of_bounds(j, n) end
  t[o + i + 1], t[o + j + 1] = t[o + j + 1], t[o + i + 1]
end

ext["std.slice.Slice.reverse"] = function(_, s)
  local t, o, n = parts(s)
  for i = 1, n // 2 do
    local a, b = o + i, o + n + 1 - i
    t[a], t[b] = t[b], t[a]
  end
end

--- A stable merge sort (`table.sort` isn't stable, and can't take nils).
ext["std.slice.Slice.sort"] = function(d, s)
  local t, o, n = parts(s)
  local items, buf = {}, {}
  for i = 1, n do
    items[i] = t[o + i]
  end
  local width = 1
  while width < n do
    for lo = 1, n, 2 * width do
      local mid = math.min(lo + width, n + 1)
      local hi = math.min(lo + 2 * width, n + 1)
      local i, j, k = lo, mid, lo
      while i < mid and j < hi do
        if cmp(d, items[j], items[i]) < 0 then
          buf[k] = items[j]
          j = j + 1
        else
          buf[k] = items[i]
          i = i + 1
        end
        k = k + 1
      end
      while i < mid do
        buf[k] = items[i]
        i, k = i + 1, k + 1
      end
      while j < hi do
        buf[k] = items[j]
        j, k = j + 1, k + 1
      end
    end
    items, buf = buf, items
    width = width * 2
  end
  for i = 1, n do
    t[o + i] = items[i]
  end
end

-- Streams. A `Stream[T]` is a Lua function: each call gives `true, item`,
-- or nothing once there are no more (`true` first, because an item can be
-- nil). Each item is its own value: a stream of a list's elements gives
-- copies, which can be kept or moved. (`collect` and friends copy what they
-- keep as well, which costs a copy but never shares.)

--- Copies of the elements of an array, list or slice whose elements `d`
--- describes.
function rt.seq_stream(d, s)
  local t, o, n = parts(s)
  local i = 0
  return function()
    if i >= n then return nil end
    i = i + 1
    return true, copy(d, t[o + i])
  end
end

--- The values of a range `{lo, hi, inclusive}` whose values `d` describes.
--- An open range (`hi` nil) unravels when it steps past its type's maximum.
function rt.range_stream(d, r)
  local x, hi, incl = r[1], r[2], r[3]
  local u64 = d.u64
  local started, done = false, false
  return function()
    if done then return nil end
    if started then
      if hi == nil and d.kind == "int" then
        local next_x, o = add_raw(d, x, 1)
        if o then error(MARK .. "arithmetic overflow", 2) end
        x = next_x
      else
        x = x + 1
      end
    end
    started = true
    if hi ~= nil then
      local inside
      if u64 then
        inside = incl and not ult(hi, x) or (not incl and ult(x, hi))
      else
        inside = incl and x <= hi or (not incl and x < hi)
      end
      if not inside then
        done = true
        return nil
      end
      -- The last value of an inclusive range may be its type's maximum:
      -- stop before stepping past it.
      if incl and x == hi then done = true end
    end
    return true, x
  end
end

--- The items of a type that weaves `Iter`, from its `next`. `boxed` when
--- `next` gives `{item}` (its items can be nil); `copy_out` when it gives
--- the iterator's new value after the item (a `mut self` that isn't a table).
function rt.iter_stream(next, it, boxed, copy_out)
  return function()
    local o
    if copy_out then
      o, it = next(it)
    else
      o = next(it)
    end
    if o == nil then return nil end
    if boxed then o = o[1] end
    return true, o
  end
end

ext["std.iter.Stream.next"] = function(d, s)
  local ok, v = s()
  if not ok then return nil end
  return some(d, copy(d, v))
end

ext["std.iter.Stream.map"] = function(_, _, s, f)
  return function()
    local ok, v = s()
    if not ok then return nil end
    return true, f(v)
  end
end

ext["std.iter.Stream.filter"] = function(_, s, f)
  return function()
    while true do
      local ok, v = s()
      if not ok then return nil end
      if f(v) then return true, v end
    end
  end
end

ext["std.iter.Stream.take"] = function(_, s, n)
  return function()
    if n <= 0 then return nil end
    n = n - 1
    return s()
  end
end

ext["std.iter.Stream.skip"] = function(_, s, n)
  return function()
    while n > 0 do
      n = n - 1
      if not s() then return nil end
    end
    return s()
  end
end

ext["std.iter.Stream.take_while"] = function(_, s, f)
  local done = false
  return function()
    if done then return nil end
    local ok, v = s()
    if ok and f(v) then return true, v end
    done = true
    return nil
  end
end

ext["std.iter.Stream.skip_while"] = function(_, s, f)
  local skipping = true
  return function()
    if skipping then
      skipping = false
      while true do
        local ok, v = s()
        if not ok then return nil end
        if not f(v) then return true, v end
      end
    end
    return s()
  end
end

ext["std.iter.Stream.indexed"] = function(_, s)
  local i = -1
  return function()
    local ok, v = s()
    if not ok then return nil end
    i = i + 1
    return true, { i, v }
  end
end

ext["std.iter.Stream.zip"] = function(_, _, s, other)
  local done = false
  return function()
    if done then return nil end
    local ok, a = s()
    if ok then
      local ok2, b = other()
      if ok2 then return true, { a, b } end
    end
    done = true
    return nil
  end
end

ext["std.iter.Stream.chain"] = function(_, s, other)
  local first = true
  return function()
    if first then
      local ok, v = s()
      if ok then return true, v end
      first = false
    end
    return other()
  end
end

ext["std.iter.Stream.collect"] = function(d, s)
  local out, n = {}, 0
  while true do
    local ok, v = s()
    if not ok then break end
    n = n + 1
    out[n] = copy(d, v)
  end
  out.n = n
  return out
end

ext["std.iter.Stream.count"] = function(_, s)
  local n = 0
  while s() do
    n = n + 1
  end
  return n
end

ext["std.iter.Stream.last"] = function(d, s)
  local have, last = false, nil
  while true do
    local ok, v = s()
    if not ok then break end
    have, last = true, v
  end
  if not have then return nil end
  return some(d, copy(d, last))
end

ext["std.iter.Stream.each"] = function(_, s, f)
  while true do
    local ok, v = s()
    if not ok then return end
    f(v)
  end
end

ext["std.iter.Stream.fold"] = function(_, _, s, acc, f)
  while true do
    local ok, v = s()
    if not ok then return acc end
    acc = f(acc, v)
  end
end

ext["std.iter.Stream.reduce"] = function(d, s, f)
  local ok, acc = s()
  if not ok then return nil end
  acc = copy(d, acc)
  while true do
    local ok2, v = s()
    if not ok2 then return some(d, acc) end
    acc = f(acc, v)
  end
end

ext["std.iter.Stream.all"] = function(_, s, f)
  while true do
    local ok, v = s()
    if not ok then return true end
    if not f(v) then return false end
  end
end

ext["std.iter.Stream.exists"] = function(_, s, f)
  while true do
    local ok, v = s()
    if not ok then return false end
    if f(v) then return true end
  end
end

ext["std.iter.Stream.find"] = function(d, s, f)
  while true do
    local ok, v = s()
    if not ok then return nil end
    if f(v) then return some(d, copy(d, v)) end
  end
end

ext["std.iter.Stream.position"] = function(_, s, f)
  local i = 0
  while true do
    local ok, v = s()
    if not ok then return nil end
    if f(v) then return i end
    i = i + 1
  end
end

--- `sum` and `product`: integers unravel when the result doesn't fit.
local function total(raw, op, start)
  return function(d, s)
    local float = d.kind == "float"
    local acc = float and start + 0.0 or start
    while true do
      local ok, v = s()
      if not ok then break end
      if float then
        acc = op(acc, v)
      else
        local r, o = raw(d, acc, v)
        if o then error(MARK .. "arithmetic overflow", 2) end
        acc = r
      end
    end
    if d.name == "f32" then acc = rt.f32(acc) end
    return acc
  end
end

ext["std.iter.Stream.sum"] = total(add_raw, function(a, b) return a + b end, 0)
ext["std.iter.Stream.product"] = total(mul_raw, function(a, b) return a * b end, 1)

--- The smallest (`sign` < 0) or largest item. NaNs are skipped.
local function extreme(sign)
  return function(d, s)
    local have, best = false, nil
    local float = d.kind == "float"
    while true do
      local ok, v = s()
      if not ok then break end
      if float and v ~= v then
        -- NaN: not smaller or larger than anything.
      elseif not have then
        have, best = true, v
      else
        local c = cmp(d, v, best)
        if (sign < 0 and c < 0) or (sign > 0 and c >= 0) then best = v end
      end
    end
    if not have then return nil end
    return some(d, copy(d, best))
  end
end

ext["std.iter.Stream.min"] = extreme(-1)
ext["std.iter.Stream.max"] = extreme(1)

ext["std.iter.Stream.join"] = function(d, s, sep)
  local out, n = {}, 0
  while true do
    local ok, v = s()
    if not ok then break end
    n = n + 1
    out[n] = show(d, v, false)
  end
  return concat(out, sep)
end

ext["std.slice.Slice.chunks"] = function(_, s, size)
  if size == 0 then error(MARK .. "chunks of 0 elements", 2) end
  local t, o, n = parts(s)
  local at = 0
  return function()
    if at >= n then return nil end
    local len = n - at
    if len > size then len = size end
    local view = { t = t, o = o + at, n = len }
    at = at + len
    return true, view
  end
end

ext["std.slice.Slice.windows"] = function(_, s, size)
  if size == 0 then error(MARK .. "windows of 0 elements", 2) end
  local t, o, n = parts(s)
  local at = 0
  return function()
    if at + size > n then return nil end
    local view = { t = t, o = o + at, n = size }
    at = at + 1
    return true, view
  end
end

-- Maps and sets, in insertion order: `{keys, vals, hks, idx, n}`. `keys[i]`
-- and `vals[i]` are the i-th pair (a set has no `vals`), `hks[i]` its key's
-- hash key, and `idx[hk]` its position. A key that's a Lua number, boolean
-- or string is its own hash key; any other (a tuple, a record) is its
-- debug text, which is the same for equal values.

local function hkey(d, k)
  local kind = d.kind
  if kind == "int" or kind == "str" or kind == "char" or kind == "bool" or kind == "enum" then return k end
  return show(d, k, true)
end

function rt.map_new()
  return { keys = {}, vals = {}, hks = {}, idx = {}, n = 0 }
end

function rt.set_new()
  return { keys = {}, hks = {}, idx = {}, n = 0 }
end

--- Adds or replaces the value for `k`; gives back the old value (or nil).
local function map_put(dk, m, k, v)
  local h = hkey(dk, k)
  local i = m.idx[h]
  if i ~= nil then
    local old = m.vals[i]
    m.vals[i] = v
    return true, old
  end
  local n = m.n + 1
  m.keys[n], m.hks[n], m.idx[h], m.n = k, h, n, n
  if m.vals then m.vals[n] = v end
  return false, nil
end

--- Takes out the entry at position `i`, moving the later ones down.
local function take_at(m, i)
  local n = m.n
  m.idx[m.hks[i]] = nil
  for j = i, n - 1 do
    m.keys[j], m.hks[j] = m.keys[j + 1], m.hks[j + 1]
    if m.vals then m.vals[j] = m.vals[j + 1] end
    m.idx[m.hks[j]] = j
  end
  m.keys[n], m.hks[n] = nil, nil
  if m.vals then m.vals[n] = nil end
  m.n = n - 1
end

--- `m[k] = v`.
function rt.map_set(dk, m, k, v)
  map_put(dk, m, k, v)
end

--- `m[k]`: the value itself (a place), unravelling if there's none.
function rt.map_at(dk, m, k)
  local i = m.idx[hkey(dk, k)]
  if i == nil then error(sformat("%skey not found: %s", MARK, show(dk, k, true)), 2) end
  return m.vals[i]
end

--- `[k: v, …]`: the keys and values alternate in `kvs`.
function rt.map_of(dk, kvs)
  local m = rt.map_new()
  for i = 1, kvs.n, 2 do
    map_put(dk, m, kvs[i], kvs[i + 1])
  end
  return m
end

local function coll_len(_, _, m)
  return m.n
end

local function coll_empty(_, _, m)
  return m.n == 0
end

ext["std.map.Map.len"] = coll_len
ext["std.map.Map.is_empty"] = coll_empty

ext["std.map.Map.get"] = function(dk, dv, m, k)
  local i = m.idx[hkey(dk, k)]
  if i == nil then return nil end
  return some(dv, copy(dv, m.vals[i]))
end

ext["std.map.Map.has"] = function(dk, _, m, k)
  return m.idx[hkey(dk, k)] ~= nil
end

ext["std.map.Map.insert"] = function(dk, dv, m, k, v)
  local had, old = map_put(dk, m, k, v)
  if not had then return nil end
  return some(dv, old)
end

ext["std.map.Map.remove"] = function(dk, dv, m, k)
  local i = m.idx[hkey(dk, k)]
  if i == nil then return nil end
  local v = m.vals[i]
  take_at(m, i)
  return some(dv, v)
end

local function coll_clear(m)
  m.keys, m.hks, m.idx, m.n = {}, {}, {}, 0
  if m.vals then m.vals = {} end
end

ext["std.map.Map.clear"] = function(_, _, m)
  coll_clear(m)
end

--- Copies of `m`'s entries, one at a time, made by `item(i)`.
local function coll_stream(m, item)
  local i, n = 0, m.n
  return function()
    if i >= n then return nil end
    i = i + 1
    return true, item(i)
  end
end

ext["std.map.Map.keys"] = function(dk, _, m)
  return coll_stream(m, function(i) return copy(dk, m.keys[i]) end)
end

ext["std.map.Map.values"] = function(_, dv, m)
  return coll_stream(m, function(i) return copy(dv, m.vals[i]) end)
end

local function map_pairs(dk, dv, m)
  return coll_stream(m, function(i) return { copy(dk, m.keys[i]), copy(dv, m.vals[i]) } end)
end
ext["std.map.Map.pairs"] = map_pairs
rt.map_stream = map_pairs

--- A stable sort of `items` by `key(item)`, compared as `d` says.
local function sort_by(d, items, n, key)
  for i = 2, n do
    local x = items[i]
    local j = i - 1
    while j >= 1 and cmp(d, key(items[j]), key(x)) > 0 do
      items[j + 1] = items[j]
      j = j - 1
    end
    items[j + 1] = x
  end
end

ext["std.map.Map.sorted"] = function(dk, dv, m)
  local out, n = {}, m.n
  for i = 1, n do
    out[i] = { copy(dk, m.keys[i]), copy(dv, m.vals[i]) }
  end
  sort_by(dk, out, n, function(p) return p[1] end)
  out.n = n
  return out
end

local function copy_map(dk, dv, m)
  local out = rt.map_new()
  for i = 1, m.n do
    map_put(dk, out, copy(dk, m.keys[i]), copy(dv, m.vals[i]))
  end
  return out
end
ext["std.map.Map.copy"] = copy_map

-- Sets.

ext["std.set.Set.of"] = function(d, xs)
  local t, o, n = parts(xs)
  local s = rt.set_new()
  for i = 1, n do
    if s.idx[hkey(d, t[o + i])] == nil then map_put(d, s, copy(d, t[o + i])) end
  end
  return s
end

ext["std.set.Set.len"] = function(_, s)
  return s.n
end

ext["std.set.Set.is_empty"] = function(_, s)
  return s.n == 0
end

ext["std.set.Set.has"] = function(d, s, x)
  return s.idx[hkey(d, x)] ~= nil
end

ext["std.set.Set.insert"] = function(d, s, x)
  if s.idx[hkey(d, x)] ~= nil then return false end
  map_put(d, s, x)
  return true
end

ext["std.set.Set.remove"] = function(d, s, x)
  local i = s.idx[hkey(d, x)]
  if i == nil then return false end
  take_at(s, i)
  return true
end

ext["std.set.Set.clear"] = function(_, s)
  coll_clear(s)
end

ext["std.set.Set.sorted"] = function(d, s)
  local out, n = {}, s.n
  for i = 1, n do
    out[i] = copy(d, s.keys[i])
  end
  sort_by(d, out, n, function(x) return x end)
  out.n = n
  return out
end

local function copy_set(d, s)
  local out = rt.set_new()
  for i = 1, s.n do
    map_put(d, out, copy(d, s.keys[i]))
  end
  return out
end
ext["std.set.Set.copy"] = copy_set

function rt.set_stream(d, s)
  return coll_stream(s, function(i) return copy(d, s.keys[i]) end)
end

--- The descriptor of `Map[K, V]` and of `Set[T]`.
function rt.mapd(dk, dv)
  return { kind = "map", key = dk, val = dv }
end

function rt.setd(d)
  return { kind = "set", key = d }
end

function rt.copy_coll(d, v)
  if d.kind == "map" then return copy_map(d.key, d.val, v) end
  return copy_set(d.key, v)
end

-- Files. A `File` is `{path}`; each write opens, appends and closes, so
-- nothing is left open when the program ends however it ends. A captured
-- run (the tests) gets a file system in memory from the host instead of the
-- disk, starting empty.

local memory = host and host.memory_fs and {} or nil

--- `T ! IoError` for an `io` failure: errno 2 is `NotFound`, 13 `Denied`.
local function io_error(path, message, errno)
  if errno == 2 then return { false, { tag = 1, path = path } } end
  if errno == 13 then return { false, { tag = 2, path = path } } end
  return { false, { tag = 3, message = message or "i/o error" } }
end

local function append(path, text)
  if memory then
    memory[path] = (memory[path] or "") .. text
    return true
  end
  local f, message, errno = io.open(path, "ab")
  if not f then return false, message, errno end
  local ok, werr, wno = f:write(text)
  f:close()
  if not ok then return false, werr, wno end
  return true
end

local function read_all(path)
  if memory then
    local text = memory[path]
    if text == nil then return nil, path .. ": No such file or directory", 2 end
    return text
  end
  local f, message, errno = io.open(path, "rb")
  if not f then return nil, message, errno end
  local text = f:read("a")
  f:close()
  return text
end

ext["std.fs.File.append"] = function(path)
  local ok, message, errno = append(path, "")
  if not ok then return io_error(path, message, errno) end
  return { true, { path = path } }
end

ext["std.fs.File.write_line"] = function(f, line)
  local ok, message, errno = append(f.path, line .. "\n")
  if not ok then return io_error(f.path, message, errno) end
  return { true, nil }
end

ext["std.fs.read_text"] = function(path)
  local text, message, errno = read_all(path)
  if text == nil then return io_error(path, message, errno) end
  return { true, text }
end

ext["std.fs.read"] = function(path)
  local text, message, errno = read_all(path)
  if text == nil then return io_error(path, message, errno) end
  local bytes = { n = #text }
  for i = 1, #text do
    bytes[i] = sbyte(text, i)
  end
  return { true, bytes }
end

-- Eager versions of `map` and `filter` for slices, which make lists.

ext["std.slice.Slice.map"] = function(_, _, s, f)
  local t, o, n = parts(s)
  local out = { n = n }
  for i = 1, n do
    out[i] = f(t[o + i])
  end
  return out
end

ext["std.slice.Slice.filter"] = function(d, s, f)
  local t, o, n = parts(s)
  local out, m = {}, 0
  for i = 1, n do
    local x = t[o + i]
    if f(x) then
      m = m + 1
      out[m] = copy(d, x)
    end
  end
  out.n = m
  return out
end

-- Text, character and number methods. Text is a Lua string; a `mut self`
-- method of `String` returns its result and then the new string, which the
-- caller stores back.

ext["std.string.String.with_capacity"] = function()
  return ""
end

ext["std.string.String.of"] = function(chars)
  local t, o, n = parts(chars)
  local out = {}
  for i = 1, n do
    out[i] = utf8char(t[o + i])
  end
  return concat(out)
end

ext["std.string.String.push"] = function(s, c)
  return nil, s .. utf8char(c)
end

ext["std.string.String.push_str"] = function(s, more)
  return nil, s .. more
end

ext["std.string.String.pop"] = function(s)
  if s == "" then return nil, s end
  local at = utf8.offset(s, -1)
  return utf8.codepoint(s, at), s:sub(1, at - 1)
end

ext["std.string.String.clear"] = function()
  return nil, ""
end

--- `s[lo..hi]`, by bytes; both ends must fall between characters.
function rt.substr(s, lo, hi)
  local n = #s
  if hi == nil then hi = n end
  if lo > hi or hi > n then
    error(sformat("%srange %d..%d is out of bounds (len %d)", MARK, lo, hi, n), 2)
  end
  for _, at in ipairs({ lo, hi }) do
    local b = sbyte(s, at + 1)
    if b and b >= 0x80 and b < 0xC0 then
      error(sformat("%sbyte %d is inside a character", MARK, at), 2)
    end
  end
  return s:sub(lo + 1, hi)
end

ext["std.string.Str.len"] = function(s)
  return #s
end

ext["std.string.Str.is_empty"] = function(s)
  return s == ""
end

ext["std.string.Str.char_count"] = function(s)
  return utf8len(s)
end

ext["std.string.Str.contains"] = function(s, part)
  return string.find(s, part, 1, true) ~= nil
end

ext["std.string.Str.starts_with"] = function(s, prefix)
  return s:sub(1, #prefix) == prefix
end

ext["std.string.Str.ends_with"] = function(s, suffix)
  return suffix == "" or s:sub(-#suffix) == suffix
end

ext["std.string.Str.find"] = function(s, part)
  local at = string.find(s, part, 1, true)
  if at == nil then return nil end
  return at - 1
end

ext["std.string.Str.trim"] = function(s)
  return (s:match("^%s*(.-)%s*$"))
end

ext["std.string.Str.trim_start"] = function(s)
  return (s:match("^%s*(.*)$"))
end

ext["std.string.Str.trim_end"] = function(s)
  return (s:match("^(.-)%s*$"))
end

ext["std.string.Str.to_upper"] = function(s)
  return (s:gsub("%l", string.upper))
end

ext["std.string.Str.to_lower"] = function(s)
  return (s:gsub("%u", string.lower))
end

ext["std.string.Str.replace"] = function(s, from, to)
  if from == "" then return s end
  local pattern = from:gsub("%p", "%%%0")
  return (s:gsub(pattern, (to:gsub("%%", "%%%%"))))
end

ext["std.string.Str.repeat"] = function(s, n)
  return srep(s, n)
end

ext["std.string.Str.split"] = function(s, sep)
  local out, n = {}, 0
  if sep == "" then
    for _, c in utf8.codes(s) do
      n = n + 1
      out[n] = utf8char(c)
    end
  else
    local at = 1
    while true do
      local i, j = string.find(s, sep, at, true)
      n = n + 1
      if i == nil then
        out[n] = s:sub(at)
        break
      end
      out[n] = s:sub(at, i - 1)
      at = j + 1
    end
  end
  out.n = n
  return out
end

ext["std.string.Str.chars"] = function(s)
  local at, n = 1, #s
  return function()
    if at > n then return nil end
    local c = utf8.codepoint(s, at)
    at = at + #utf8char(c)
    return true, c
  end
end

ext["std.string.Str.bytes"] = function(s)
  local at, n = 0, #s
  return function()
    if at >= n then return nil end
    at = at + 1
    return true, sbyte(s, at)
  end
end

ext["std.string.Str.lines"] = function(s)
  local at, n = 1, #s
  return function()
    if at > n then return nil end
    local nl = string.find(s, "\n", at, true)
    local line
    if nl then
      line = s:sub(at, nl - 1)
      at = nl + 1
    else
      line = s:sub(at)
      at = n + 1
    end
    if sbyte(line, -1) == 13 then line = line:sub(1, -2) end
    return true, line
  end
end

ext["std.string.Str.words"] = function(s)
  local at = 1
  return function()
    local i, j = string.find(s, "%S+", at)
    if i == nil then return nil end
    at = j + 1
    return true, s:sub(i, j)
  end
end

local function same(s)
  return s
end
ext["std.string.Str.to_string"] = same
ext["std.string.Str.copy"] = same

-- `NumError`'s cases, by tag.
local EMPTY, INVALID, OVERFLOW = 1, 2, 3

local function parse_int(d, s)
  if s == "" then return { false, EMPTY } end
  local neg, i = false, 1
  local first = s:sub(1, 1)
  if first == "+" or first == "-" then
    neg, i = first == "-", 2
    if #s == 1 then return { false, INVALID } end
  end
  if neg and not d.signed then return { false, INVALID } end
  -- The magnitude, as an unsigned 64-bit value.
  local mag = 0
  for j = i, #s do
    local b = sbyte(s, j)
    if b < 48 or b > 57 then return { false, INVALID } end
    local digit = b - 48
    if ult(udiv(-1 - digit, 10), mag) then return { false, OVERFLOW } end
    mag = mag * 10 + digit
  end
  local limit = neg and -d.lo or d.hi
  if ult(limit, mag) then return { false, OVERFLOW } end
  return { true, neg and -mag or mag }
end

local function parse_float(d, s)
  if s == "" then return { false, EMPTY } end
  local body = s:gsub("^[+-]", "")
  local lower = body:lower()
  local v
  if lower == "inf" or lower == "infinity" then
    v = huge
  elseif lower == "nan" then
    v = 0.0 / 0.0
  else
    local mantissa, exponent = body:match("^([%d.]+)(.*)$")
    local ok = mantissa ~= nil
      and mantissa:find("%d") ~= nil
      and select(2, mantissa:gsub("%.", "")) <= 1
      and (exponent == "" or exponent:match("^[eE][+-]?%d+$") ~= nil)
    if not ok then return { false, INVALID } end
    v = tonumber(body) + 0.0
  end
  if s:sub(1, 1) == "-" then v = -v end
  if d.name == "f32" then v = rt.f32(v) end
  return { true, v }
end

ext["std.string.Str.parse"] = function(d, s)
  if d.kind == "int" then return parse_int(d, s) end
  return parse_float(d, s)
end

-- Characters are code points.

local function between(c, lo, hi)
  return c >= lo and c <= hi
end

local function is_upper(c)
  return between(c, 65, 90)
end

local function is_lower(c)
  return between(c, 97, 122)
end

local function is_digit(c)
  return between(c, 48, 57)
end

ext["std.string.Char.is_digit"] = is_digit
ext["std.string.Char.is_upper"] = is_upper
ext["std.string.Char.is_lower"] = is_lower

ext["std.string.Char.is_alphabetic"] = function(c)
  return is_upper(c) or is_lower(c)
end

ext["std.string.Char.is_alphanumeric"] = function(c)
  return is_upper(c) or is_lower(c) or is_digit(c)
end

ext["std.string.Char.is_whitespace"] = function(c)
  return c == 32 or between(c, 9, 13)
end

ext["std.string.Char.to_upper"] = function(c)
  if is_lower(c) then return c - 32 end
  return c
end

ext["std.string.Char.to_lower"] = function(c)
  if is_upper(c) then return c + 32 end
  return c
end

ext["std.string.Char.to_digit"] = function(c, radix)
  local v
  if is_digit(c) then
    v = c - 48
  elseif is_lower(c) then
    v = c - 87
  elseif is_upper(c) then
    v = c - 55
  else
    return nil
  end
  if v < radix then return v end
  return nil
end

ext["std.string.Char.len_utf8"] = function(c)
  return #utf8char(c)
end

-- Integers get their type's descriptor first.

ext["std.num.Int.abs"] = function(d, v)
  if v >= 0 or not d.signed then return v end
  if v == d.lo then error(MARK .. "arithmetic overflow", 2) end
  return -v
end

local function less(d, a, b)
  if d.u64 then return ult(a, b) end
  return a < b
end

ext["std.num.Int.min"] = function(d, a, b)
  if less(d, b, a) then return b end
  return a
end

ext["std.num.Int.max"] = function(d, a, b)
  if less(d, a, b) then return b end
  return a
end

ext["std.num.Int.pow"] = function(d, v, exp)
  local r = 1
  for _ = 1, exp do
    r = rt.mul_chk(d, r, v)
    if r == nil then error(MARK .. "arithmetic overflow", 2) end
  end
  return r
end

ext["std.num.Int.trailing_zeros"] = function(d, v)
  local bits = v & d.mask
  if bits == 0 then return d.bits end
  local n = 0
  while bits & 1 == 0 do
    bits = bits >> 1
    n = n + 1
  end
  return n
end

ext["std.num.Int.leading_zeros"] = function(d, v)
  local bits = v & d.mask
  local n = d.bits
  while bits ~= 0 do
    bits = bits >> 1
    n = n - 1
  end
  return n
end

ext["std.num.Int.count_ones"] = function(d, v)
  local bits = v & d.mask
  local n = 0
  while bits ~= 0 do
    n = n + (bits & 1)
    bits = bits >> 1
  end
  return n
end

local function to_string(d, v)
  return show(d, v, false)
end
ext["std.num.Int.to_string"] = to_string
ext["std.num.Float.to_string"] = to_string

-- Floats get their type's descriptor first; an `f32` result is rounded to
-- one.

local function float(d, x)
  if d.name == "f32" then return rt.f32(x) end
  return x + 0.0
end

ext["std.num.Float.abs"] = function(d, v)
  return float(d, math.abs(v))
end

ext["std.num.Float.min"] = function(d, a, b)
  if a ~= a then return b end
  if b ~= b then return a end
  return float(d, b < a and b or a)
end

ext["std.num.Float.max"] = function(d, a, b)
  if a ~= a then return b end
  if b ~= b then return a end
  return float(d, b > a and b or a)
end

ext["std.num.Float.sqrt"] = function(d, v)
  return float(d, math.sqrt(v))
end

ext["std.num.Float.floor"] = function(d, v)
  return float(d, floor(v))
end

ext["std.num.Float.ceil"] = function(d, v)
  return float(d, ceil(v))
end

ext["std.num.Float.trunc"] = function(d, v)
  if v >= 0 then return float(d, floor(v)) end
  return float(d, ceil(v))
end

ext["std.num.Float.round"] = function(d, v)
  if v ~= v or v == huge or v == -huge then return v end
  if v >= 0 then
    local f = floor(v)
    if v - f >= 0.5 then f = f + 1 end
    return float(d, f)
  end
  local c = ceil(v)
  if c - v >= 0.5 then c = c - 1 end
  return float(d, c)
end

ext["std.num.Float.pow"] = function(d, v, exp)
  return float(d, v ^ exp)
end

ext["std.num.Float.is_nan"] = function(_, v)
  return v ~= v
end

return rt
