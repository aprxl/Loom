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

-- ================================================================ specialized operations

-- The operations above take the type's descriptor on every call and look
-- inside it. The generated code (design/vm-review-v4.md) instead makes, once
-- at the start of the bundle, one closure for each (operation, type) it uses
-- (`rt.spec`): the bounds are upvalues, and the choice of how to check is
-- made then rather than on every call. Each must do exactly what the
-- operation of the same name does (`rt_int.rs` checks both against the
-- oracle). An unravel is raised one level up, from the closure: where the
-- generated code called it.

local spec = {}

function spec.add(d)
  local lo, hi = d.lo, d.hi
  if d.exact then
    return function(a, b)
      local r = a + b
      if r > hi or r < lo then error(MARK .. "arithmetic overflow", 2) end
      return r
    end
  elseif d.u64 then
    return function(a, b)
      local r = a + b
      if ult(r, a) then error(MARK .. "arithmetic overflow", 2) end
      return r
    end
  elseif d.signed then
    return function(a, b)
      local r = a + b
      if ((a ~ r) & (b ~ r)) < 0 then error(MARK .. "arithmetic overflow", 2) end
      return r
    end
  end
  return function(a, b) -- usize (u63): the sum passed 2^63
    local r = a + b
    if r < 0 then error(MARK .. "arithmetic overflow", 2) end
    return r
  end
end

function spec.sub(d)
  local lo, hi = d.lo, d.hi
  if d.exact then
    return function(a, b)
      local r = a - b
      if r > hi or r < lo then error(MARK .. "arithmetic overflow", 2) end
      return r
    end
  elseif d.u64 then
    return function(a, b)
      if ult(a, b) then error(MARK .. "arithmetic overflow", 2) end
      return a - b
    end
  elseif d.signed then
    return function(a, b)
      local r = a - b
      if ((a ~ b) & (a ~ r)) < 0 then error(MARK .. "arithmetic overflow", 2) end
      return r
    end
  end
  return function(a, b)
    local r = a - b
    if r < 0 then error(MARK .. "arithmetic overflow", 2) end
    return r
  end
end

function spec.mul(d)
  local lo, hi = d.lo, d.hi
  -- Up to 32 bits the product fits a Lua integer, or (an unsigned 32-bit
  -- one past 2^63) wraps to a negative, which is below `lo` too.
  if d.bits <= 32 then
    return function(a, b)
      local r = a * b
      if r > hi or r < lo then error(MARK .. "arithmetic overflow", 2) end
      return r
    end
  end
  return function(a, b)
    local r, o = mul_raw(d, a, b)
    if o then error(MARK .. "arithmetic overflow", 2) end
    return r
  end
end

-- `+%`, `-%`, `*%`: the Lua result, wrapped into the type.
function spec.add_wrap(d)
  local bits, mask, hi = d.bits, d.mask, d.hi
  if bits == 64 then
    return function(a, b) return a + b end
  elseif d.signed then
    local modulus = 1 << bits
    return function(a, b)
      local r = (a + b) & mask
      if r > hi then r = r - modulus end
      return r
    end
  end
  return function(a, b) return (a + b) & mask end
end

function spec.sub_wrap(d)
  local bits, mask, hi = d.bits, d.mask, d.hi
  if bits == 64 then
    return function(a, b) return a - b end
  elseif d.signed then
    local modulus = 1 << bits
    return function(a, b)
      local r = (a - b) & mask
      if r > hi then r = r - modulus end
      return r
    end
  end
  return function(a, b) return (a - b) & mask end
end

function spec.mul_wrap(d)
  local bits, mask, hi = d.bits, d.mask, d.hi
  if bits == 64 then
    return function(a, b) return a * b end
  elseif d.signed then
    local modulus = 1 << bits
    return function(a, b)
      local r = (a * b) & mask
      if r > hi then r = r - modulus end
      return r
    end
  end
  return function(a, b) return (a * b) & mask end
end

function spec.div(d)
  local lo = d.lo
  if d.u64 then
    return function(a, b)
      if b == 0 then error(MARK .. "division by zero", 2) end
      return udiv(a, b)
    end
  elseif not d.signed then
    return function(a, b)
      if b == 0 then error(MARK .. "division by zero", 2) end
      return a // b
    end
  end
  return function(a, b)
    if b == 0 then error(MARK .. "division by zero", 2) end
    if b == -1 and a == lo then error(MARK .. "arithmetic overflow", 2) end
    local q = a // b
    if q < 0 and q * b ~= a then q = q + 1 end
    return q
  end
end

function spec.rem(d)
  if d.u64 then
    return function(a, b)
      if b == 0 then error(MARK .. "division by zero", 2) end
      return a - udiv(a, b) * b
    end
  elseif not d.signed then
    return function(a, b)
      if b == 0 then error(MARK .. "division by zero", 2) end
      return a % b
    end
  end
  -- Lua's `%` has the sign of the divisor, the remainder of a truncating
  -- division that of the dividend: when they differ, take one divisor off.
  return function(a, b)
    if b == 0 then error(MARK .. "division by zero", 2) end
    local r = a % b
    if r ~= 0 and (r ~ a) < 0 then r = r - b end
    return r
  end
end

function spec.shl(d)
  local bits, mask, hi = d.bits, d.mask, d.hi
  if bits == 64 then
    return function(a, n)
      if n < 0 or n >= 64 then error(MARK .. "shift amount out of range", 2) end
      return a << n
    end
  elseif d.signed then
    local modulus = 1 << bits
    return function(a, n)
      if n < 0 or n >= bits then error(MARK .. "shift amount out of range", 2) end
      local r = (a << n) & mask
      if r > hi then r = r - modulus end
      return r
    end
  end
  return function(a, n)
    if n < 0 or n >= bits then error(MARK .. "shift amount out of range", 2) end
    return (a << n) & mask
  end
end

function spec.shr(d)
  local bits = d.bits
  if d.signed then
    return function(a, n)
      if n < 0 or n >= bits then error(MARK .. "shift amount out of range", 2) end
      if a < 0 then return ~((~a) >> n) end
      return a >> n
    end
  end
  return function(a, n)
    if n < 0 or n >= bits then error(MARK .. "shift amount out of range", 2) end
    return a >> n
  end
end

-- `v as T` for a source type `s`: the range test chosen once.
function spec.cast(d, s)
  local lo, hi, name = d.lo, d.hi, d.name
  if s.u64 then
    if d.u64 then return function(v) return v end end
    return function(v)
      if v < 0 or v > hi then error(MARK .. "value out of range for `" .. name .. "`", 2) end
      return v
    end
  elseif d.u64 then
    return function(v)
      if v < 0 then error(MARK .. "value out of range for `" .. name .. "`", 2) end
      return v
    end
  end
  return function(v)
    if v < lo or v > hi then error(MARK .. "value out of range for `" .. name .. "`", 2) end
    return v
  end
end

-- `v as% T`: the wrap into the type.
function spec.cast_wrap(d, s)
  local bits, mask, hi = d.bits, d.mask, d.hi
  if bits == 64 then
    return function(v) return v end
  elseif d.signed then
    local modulus = 1 << bits
    return function(v)
      local r = v & mask
      if r > hi then r = r - modulus end
      return r
    end
  end
  return function(v) return v & mask end
end

-- A copy of a value of type `d`: its copier (see `rt.copier`), or, for a
-- type whose values aren't copied (a scalar), a function that returns it.
function spec.copy(d)
  return rt.copier(d) or function(v) return v end
end

--- The closure for operation `name` (one of `add`, `sub`, `mul`, their
--- `_wrap` forms, `div`, `rem`, `shl`, `shr`) on integer type `d`, which
--- takes the operands only.
function rt.spec(name, d, s)
  return spec[name](d, s)
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

--- An integer of type `s` as a float, rounded once. A u64 of 2^63 or more
--- is halved first, keeping the bit that falls off as a "sticky" low bit so
--- the halved value still rounds the right way.
function rt.itof(s, v)
  if s.u64 and v < 0 then return ((v >> 1) | (v & 1)) * 2.0 end
  return v + 0.0
end

--- An integer of type `s` as an `f32`, rounded once. Through an f64 it
--- would round twice (to 53 bits, then 24) for values past 2^53: those keep
--- 26 bits, with the rest folded into the lowest as a sticky bit.
function rt.itof32(s, v)
  local neg = not s.u64 and v < 0
  local m = neg and -v or v -- the magnitude, as an unsigned 64-bit pattern
  if m >= 0 and m < (1 << 53) then
    local f = rt.f32(m + 0.0)
    return neg and -f or f
  end
  local len, x = 0, m
  while x ~= 0 do
    x = x >> 1
    len = len + 1
  end
  local shift = len - 26
  local top = (m >> shift) | ((m & ((1 << shift) - 1)) ~= 0 and 1 or 0)
  local f = rt.f32(top + 0.0) * 2.0 ^ shift
  return neg and -f or f
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

--- `^T`: a box `{v}`. `inner` is set after the descriptors are made when the
--- type contains itself.
function rt.ptr(inner)
  return { kind = "ptr", inner = inner }
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
  local s = rt.show(d, v, true)
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

--- Unravels for index `i` past length `n`: the generated code checks
--- `i >= n` itself (no call per index) and calls this when it is.
function rt.oob(i, n)
  error(sformat("%sindex %d is out of bounds (len %d)", MARK, i, n), 2)
end

-- `v.(c)`: `v`, which must be the case with tag `tag` (`name` is `.Case`).
-- A value of a sum with only unit cases is its tag.
function rt.as_case(v, tag, name)
  local actual = type(v) == "table" and v.tag or v
  if actual ~= tag then error(sformat("%sthe value isn't %s", MARK, name), 2) end
  return v
end

-- A loop over list `s` (or a slice of it, or a map or set: `what`), which had
-- `n` items when it started: it can't change under the loop. The checker
-- stops the loop's body from changing it, but not a lambda the body calls.
function rt.same_len(s, n, what)
  if s.n ~= n then
    error(sformat("%sthe %s changed while a loop went over it (len %d, now %d)", MARK, what or "list", n, s.n), 2)
  end
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
-- The kinds whose values are never mutated in place: a copy of one is itself.
local flat_kinds = {
  int = true, float = true, bool = true, char = true, str = true, unit = true,
  enum = true, opaque = true,
}

-- The deep copy of a value of descriptor `d`, which a value that's a table
-- is given when it's stored somewhere that owns it. `copy` below is what
-- the generated code calls; it uses a copier made once for each descriptor
-- (`d.cp`): `false` when a copy is the value itself, else a function. A
-- record or a tuple's is a constructor (`{x = v.x, pos = u1(v.pos)}`), with
-- no walk of the descriptor and no tests of kinds; the rest go through
-- `copy_walk`, which finds the copiers of what's inside.
local copy
do -- (a block: the chunk is near Lua's limit of 200 locals)
local copier_of, make_copier

local function copy_walk(d, v)
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
  elseif kind == "ptr" then
    return { copy(d.inner, v[1]) }
  elseif kind == "seq" then
    local n, elem = v.n, d.elem
    local t = { n = n }
    local c = copier_of(elem)
    if c then
      for i = 1, n do
        t[i] = c(v[i])
      end
    else
      for i = 1, n do
        t[i] = v[i]
      end
    end
    return t
  elseif kind == "map" or kind == "set" then
    return rt.copy_coll(d, v)
  elseif kind == "slice" then
    -- A view of copies: only a constant's slices are copied.
    local t, o, n = parts(v)
    local out = { n = n }
    for i = 1, n do
      out[i] = copy(d.elem, t[o + i])
    end
    return { t = out, o = 0, n = n }
  end
  return v
end

copier_of = function(d)
  local c = d.cp
  if c == nil then
    -- A descriptor can contain itself: until this one's is made, what
    -- needs it walks.
    d.cp = function(v) return copy_walk(d, v) end
    c = make_copier(d)
    d.cp = c
  end
  return c
end

-- Appends to `parts` what makes each field of `fields` (each `{key, name,
-- descriptor}`, `keys` its keys) copied in a table constructor, and the
-- copiers of what's inside to `up` (the constructor's upvalues).
local function field_parts(fields, keys, up, parts)
  for i, key in ipairs(keys) do
    local c = copier_of(fields[i][3])
    local k
    if math.type(key) == "integer" then k = "[" .. key .. "]" else k = "[" .. string.format("%q", key) .. "]" end
    if c then
      up[#up + 1] = c
      parts[#parts + 1] = k .. "=u" .. #up .. "(v" .. k .. ")"
    else
      parts[#parts + 1] = k .. "=v" .. k
    end
  end
end

-- The function that `body` (the source of a function of `v`, using the
-- upvalues `u1...`) is, with its upvalues `up`.
local function build(body, up)
  local names = {}
  for i = 1, #up do names[i] = "u" .. i end
  local source = (#up > 0 and ("local " .. table.concat(names, ",") .. " = ...\n") or "")
    .. "return function(v)\n" .. body .. "\nend"
  return load(source)(table.unpack(up))
end

-- A function that makes a table with each field of `fields` copied.
local function constructor(fields, keys)
  local up, parts = {}, {}
  field_parts(fields, keys, up, parts)
  return build("return {" .. table.concat(parts, ",") .. "}", up)
end

-- A sum: the same, for the case its tag says (and the fields every case
-- has).
local function sum_constructor(d)
  local up, body = {}, {}
  local shared_keys = {}
  for i, f in ipairs(d.shared) do shared_keys[i] = f[1] end
  for tag, case in pairs(d.cases) do
    local fields, keys = {}, {}
    for i, f in ipairs(case.fields or {}) do
      fields[i] = f
      keys[i] = f[1]
    end
    local parts = { "tag=" .. tag }
    field_parts(fields, keys, up, parts)
    field_parts(d.shared, shared_keys, up, parts)
    body[#body + 1] = "if v.tag == " .. tag .. " then return {" .. table.concat(parts, ",") .. "} end"
  end
  body[#body + 1] = "return v"
  return build(table.concat(body, "\n"), up)
end

make_copier = function(d)
  local kind = d.kind
  if flat_kinds[kind] or kind == "any" then
    return false
  elseif kind == "record" then
    local keys = {}
    for i, f in ipairs(d.fields) do keys[i] = f[1] end
    return constructor(d.fields, keys)
  elseif kind == "tuple" then
    local fields, keys = {}, {}
    for i, item in ipairs(d.items) do
      fields[i] = { i, nil, item }
      keys[i] = i
    end
    return constructor(fields, keys)
  elseif kind == "sum" then
    return sum_constructor(d)
  end
  return function(v) return copy_walk(d, v) end
end

copy = function(d, v)
  local c = d.cp
  if c == nil then c = copier_of(d) end
  if c then return c(v) end
  return v
end

--- The function that copies values of descriptor `d` (false for a type
--- whose values are never changed in place).
function rt.copier(d)
  return copier_of(d)
end
end
rt.copy = copy

--- `n` copies of `v`.
function rt.filled(d, v, n)
  local t = { n = n }
  if flat_kinds[d.kind] then
    -- Numbers, text, booleans: a value nothing changes in place.
    for i = 1, n do
      t[i] = v
    end
    return t
  end
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

--- What a removed map or set entry's hash is, in `hks`: the entry stays in
--- `keys`, `vals` and `hks` (so removing is O(1)) until more than half of
--- the `top` entries made are removed ones, or something walks the entries.
local DEAD = {}

--- Moves the entries that are left down over the removed ones, in order:
--- afterwards `top` is `n`.
local function compact(m)
  local top, n = m.top, m.n
  if top == n then return end
  local keys, vals, hks, idx = m.keys, m.vals, m.hks, m.idx
  local w = 0
  for i = 1, top do
    local h = hks[i]
    if h ~= DEAD then
      w = w + 1
      if w ~= i then
        keys[w], hks[w] = keys[i], h
        if vals then vals[w] = vals[i] end
        idx[h] = w
      end
    end
  end
  for i = w + 1, top do
    keys[i], hks[i] = nil, nil
    if vals then vals[i] = nil end
  end
  m.top = w
  m.head = 0
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
  elseif kind == "ptr" then
    return eq(d.inner, a[1], b[1])
  elseif kind == "seq" or kind == "slice" then
    local at, ao, an = parts(a)
    local bt, bo, bn = parts(b)
    if an ~= bn then return false end
    for i = 1, an do
      if not eq(d.elem, at[ao + i], bt[bo + i]) then return false end
    end
    return true
  elseif kind == "any" then
    -- A union value: the same type, and equal as that type.
    if a[2].d ~= b[2].d then return false end
    return eq(a[2].d, a[1], b[1])
  elseif kind == "map" or kind == "set" then
    -- The same keys (and values for them), in any order.
    if a.n ~= b.n then return false end
    compact(a)
    compact(b)
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
  elseif kind == "ptr" then
    return cmp(d.inner, a[1], b[1])
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

-- A comparison function for each descriptor, made once (`rt.comparer`):
-- what `cmp` does for it, with the descriptor's walk decided then. A record
-- or tuple's is generated source that compares the fields in turn, the
-- comparisons of what's inside its upvalues. Used where one comparison is
-- made many times: sorting.
do
  local scalar_kinds = { int = true, float = true, str = true, char = true, enum = true }
  local comparer, make_comparer

  -- Appends to `lines` the test of each field of `fields` (each `{key, name,
  -- descriptor}`, `keys` its keys), and to `up` the functions it calls.
  local function field_tests(fields, keys, up, lines)
    for i, key in ipairs(keys) do
      local fd = fields[i][3]
      local k
      if math.type(key) == "integer" then k = "[" .. key .. "]" else k = "[" .. string.format("%q", key) .. "]" end
      if scalar_kinds[fd.kind] and not fd.lt and not fd.u64 then
        lines[#lines + 1] = "do local x, y = a" .. k .. ", b" .. k
          .. " if x < y then return -1 elseif y < x then return 1 end end"
      elseif fd.kind == "int" and fd.u64 and not fd.lt then
        up[#up + 1] = ult
        lines[#lines + 1] = "do local x, y = a" .. k .. ", b" .. k
          .. " if x ~= y then if u" .. #up .. "(x, y) then return -1 end return 1 end end"
      else
        up[#up + 1] = comparer(fd)
        lines[#lines + 1] = "do local r = u" .. #up .. "(a" .. k .. ", b" .. k .. ") if r ~= 0 then return r end end"
      end
    end
  end

  local function build(lines, up)
    local names = {}
    for i = 1, #up do names[i] = "u" .. i end
    local source = (#up > 0 and ("local " .. table.concat(names, ",") .. " = ...\n") or "")
      .. "return function(a, b)\n" .. table.concat(lines, "\n") .. "\nreturn 0\nend"
    return load(source)(table.unpack(up))
  end

  make_comparer = function(d)
    local kind = d.kind
    if d.lt then
      return function(a, b) return cmp(d, a, b) end
    elseif kind == "record" then
      local keys = {}
      for i, f in ipairs(d.fields) do keys[i] = f[1] end
      local up, lines = {}, {}
      field_tests(d.fields, keys, up, lines)
      return build(lines, up)
    elseif kind == "tuple" then
      local fields, keys = {}, {}
      for i, item in ipairs(d.items) do
        fields[i] = { i, nil, item }
        keys[i] = i
      end
      local up, lines = {}, {}
      field_tests(fields, keys, up, lines)
      return build(lines, up)
    elseif kind == "int" and d.u64 then
      return function(a, b)
        if a == b then return 0 end
        return ult(a, b) and -1 or 1
      end
    elseif scalar_kinds[kind] then
      return function(a, b)
        if a < b then return -1 end
        if b < a then return 1 end
        return 0
      end
    end
    return function(a, b) return cmp(d, a, b) end
  end

  comparer = function(d)
    local c = d.cf
    if c == nil then
      -- A descriptor can contain itself: until this one's is made, what
      -- needs it goes through `cmp`.
      d.cf = function(a, b) return cmp(d, a, b) end
      c = make_comparer(d)
      d.cf = c
    end
    return c
  end

  function rt.comparer(d)
    return comparer(d)
  end
end

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

-- When `v` is exactly halfway between two decimals of `prec` digits (as
-- 93731167244648.125 is between ...48.12 and ...48.13: `printf` takes the
-- even one, Rust's Display the larger): the larger, as "d.ddde+XX".
local function tie_up(v, prec)
  local sign, first, rest, exp = sformat("%." .. (prec + 20) .. "e", v):match("^(-?)(%d)%.(%d+)e([-+]%d+)$")
  if not sign then return nil end
  local digits = first .. rest
  if digits:sub(prec + 1, prec + 1) ~= "5" or digits:sub(prec + 2):find("[^0]") then return nil end
  local chars = { digits:byte(1, prec) }
  local i = prec
  while i >= 1 do
    if chars[i] == 57 then
      chars[i] = 48
      i = i - 1
    else
      chars[i] = chars[i] + 1
      break
    end
  end
  local e = tonumber(exp)
  local head
  if i == 0 then
    -- 99…9 carried over: 1 followed by zeros, one power of ten up.
    head = "1" .. srep("0", prec - 1)
    e = e + 1
  else
    head = string.char(table.unpack(chars))
  end
  local mantissa = head:sub(1, 1) .. (prec > 1 and ("." .. head:sub(2)) or "")
  return sign .. mantissa .. "e" .. sformat("%+d", e)
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
      if back == v then
        -- Halfway between two such decimals, Rust's takes the larger.
        local up = tie_up(v, prec)
        if up then
          back = tonumber(up)
          if single then back = rt.f32(back) end
          if back == v then s = up end
        end
        break
      end
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
    compact(v)
    local out = {}
    for i = 1, v.n do
      out[i] = show(d.key, v.keys[i], debug) .. ": " .. show(d.val, v.vals[i], debug)
    end
    return "[" .. concat(out, ", ") .. "]"
  elseif kind == "set" then
    compact(v)
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
  elseif kind == "ptr" then
    return show(d.inner, v[1], debug)
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

-- `extern "host"` functions of std, by path. The compiler only emits calls to
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

-- `process.exit(code)`: unwinds like an unravel (`defer`s run), and the
-- host ends the run with the code.
rt.ext["std.process.exit"] = function(code)
  error(MARK .. "\31exit " .. code, 0)
end

rt.ext["std.io.write_out"] = function(s)
  write(s)
end

rt.ext["std.io.write_err"] = function(s)
  if not host then needs_host("io.write_err") end
  host.write_err(s)
end

rt.ext["std.time.monotonic_nanos"] = function()
  if not host then needs_host("time.now") end
  return host.now()
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
  local compare = rt.comparer(d)
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
        if compare(items[j], items[i]) < 0 then
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
  local len = t.n
  local i = 0
  return function()
    if i >= n then return nil end
    rt.same_len(t, len)
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

local co_create, co_resume, co_yield = coroutine.create, coroutine.resume, coroutine.yield
local co_status, co_close = coroutine.status, coroutine.close

-- Ctrl-C reaches a coroutine only through a hook of its own: the host is
-- told of each shuttle's, and holds it (here) until it's done.
local watch_thread = host and host.watch_thread
local unwatch_thread = host and host.unwatch_thread
local watched = {}

-- ================================================================ threads
-- design/vm.md §11. A thread is a task: a coroutine run by the reference
-- schedule, which is what Treadle does with one worker. `thread e` runs the
-- task at once, until it finishes or parks, and the spawner carries on
-- (a plain nested `resume`). A task parks at `join`, `sched_yield` and the
-- waits of `Pipe` and `Mutex`; waking one appends it to the ready queue.
-- The main program isn't a coroutine: when it parks it runs the queue
-- itself, until it comes up in it.
--
-- A group is a loom block's `{tasks, n, failure, done}`, a Lua local the
-- block's threads reach as an upvalue, so `thread` finds its loom lexically.

local PARK = {}                      -- what a parked task yields
local CANCEL = MARK .. "\31cancel"   -- what a cancelled task unwinds with
local CLOSE = MARK .. "\31close"     -- what a closed shuttle unwinds with
local MAIN = { main = true, sdepth = 0 }
local current = MAIN
local ready, rhead, rtail = {}, 1, 0
local sleepers = {} -- tasks parked in `time.sleep`, each with its `sleep_at`

local function enqueue(t)
  rtail = rtail + 1
  ready[rtail] = t
end

--- Makes `t` ready again, if it was waiting.
local function wake(t)
  if t.waiting then
    t.waiting = false
    t.waiting_on = nil
    enqueue(t)
  end
end

local function unsleep(t)
  for i = 1, #sleepers do
    if sleepers[i] == t then
      table.remove(sleepers, i)
      break
    end
  end
  t.sleep_at = nil
end

--- Whoever runs `t` carries on when it parks or finishes.
local function run(t)
  local prev = current
  current = t
  local ok, err = co_resume(t.co)
  current = prev
  if not ok then error(err, 0) end -- the body catches everything: a bug
end

--- Stops the running task (its waiting is already recorded) until it is
--- woken, which is when it comes up in the ready queue.
local function suspend()
  local cur = current
  if cur.main and cur.sdepth == 0 then
    while true do
      if rhead > rtail and #sleepers > 0 then
        local first = sleepers[1]
        for i = 2, #sleepers do
          if sleepers[i].sleep_at < first.sleep_at then first = sleepers[i] end
        end
        local left = first.sleep_at - host.now()
        if left > 0 then host.sleep(left) end
        unsleep(first)
        wake(first)
      end
      if rhead > rtail then
        -- Nothing can run: the program's own task unravels where it waits,
        -- no longer in anyone's list.
        local on = MAIN.waiting_on
        if on then
          for i, w in ipairs(on.waiters) do
            if w == MAIN then
              table.remove(on.waiters, i)
              break
            end
          end
          MAIN.waiting_on = nil
        end
        MAIN.waiting = false
        error(MARK .. "deadlock: every thread is waiting", 0)
      end
      local t = ready[rhead]
      ready[rhead] = nil
      rhead = rhead + 1
      if t == MAIN then return end
      run(t)
    end
  end
  co_yield(PARK)
end

--- `suspend`, and a cancelled task goes no further.
local function park()
  suspend()
  if current.cancelled then error(CANCEL, 0) end
end

rt.ext["std.time.sleep"] = function(d)
  if not host then needs_host("time.sleep") end
  local cur = current
  if cur.main and cur.sdepth == 0 and rhead > rtail and #sleepers == 0 then
    host.sleep(d.nanos) -- nothing else can run
    return
  end
  cur.sleep_at = host.now() + d.nanos
  cur.waiting = true
  sleepers[#sleepers + 1] = cur
  park()
end

rt.ext["std.io.read_line"] = function()
  if not host then needs_host("io.read_line") end
  local line = host.read_line()
  if current.cancelled then error(CANCEL, 0) end
  return line
end

rt.ext["std.io.read_all"] = function()
  if not host then needs_host("io.read_all") end
  local text = host.read_all()
  if current.cancelled then error(CANCEL, 0) end
  return text
end

--- Waits for `t` to finish. Not a cancellation point: a task leaving a loom
--- waits for its threads on the way out of a cancel too.
local function wait_for(t)
  local cur = current
  while not t.done do
    cur.waiting = true
    cur.waiting_on = t
    t.waiters[#t.waiters + 1] = cur
    suspend()
  end
end

local function cancel(t)
  if t.done or t.cancelled then return end
  t.cancelled = true
  local on = t.waiting_on
  if on then
    for i, w in ipairs(on.waiters) do
      if w == t then
        table.remove(on.waiters, i)
        break
      end
    end
  end
  if t.sleep_at then unsleep(t) end
  wake(t)
end

local function cancel_all(g)
  for i = 1, g.n do cancel(g.tasks[i]) end
end

local function wait_all(g)
  for i = 1, g.n do wait_for(g.tasks[i]) end
end

--- The group has failed: what's left is cancelled and waited for, and then
--- the parent unravels with the failure.
local function abort(g)
  cancel_all(g)
  wait_all(g)
  error(g.failure, 0)
end

local function thread_main(t, fn)
  return function()
    local ok, r = pcall(fn)
    t.done = true
    if ok then
      t.result = r
    elseif r == CANCEL then
      t.cancelled = true
    else
      local g = t.group
      if not g.failure then
        -- Where it was raised is the thread's: the parent reports it where
        -- it sees it (design/vm.md §11.2), whatever shape the code had.
        if type(r) == "string" then
          r = r:gsub("^bundle:%d+: ", "")
          -- An unravel (not an exit or Ctrl-C) says where the thread
          -- started; a nested thread's note is the outer thread's.
          if t.at and r:sub(1, #MARK) == MARK and r:sub(#MARK + 1, #MARK + 1) ~= "\31" then
            r = r:gsub("\31thread %d+$", "") .. "\31thread " .. t.at
          end
        end
        g.failure = r
        for i = 1, g.n do
          if g.tasks[i] ~= t then cancel(g.tasks[i]) end
        end
      end
    end
    if t.watched then unwatch_thread(t.co) end
    local ws = t.waiters
    t.waiters = {}
    for i = 1, #ws do wake(ws[i]) end
  end
end

function rt.loom()
  return { tasks = {}, n = 0, failure = nil, done = false }
end

function rt.spawn(g, fn)
  local t = { group = g, waiters = {}, done = false, sdepth = 0 }
  -- The line of the bundle that spawns it (the caller of the caller of
  -- `error`), which the host maps to the `thread` expression.
  t.at = select(2, pcall(error, "", 3)):match("^bundle:(%d+):")
  g.n = g.n + 1
  g.tasks[g.n] = t
  if g.failure then
    -- spawned into a failed group: it starts cancelled and never runs
    t.done, t.cancelled = true, true
    return t
  end
  t.co = co_create(thread_main(t, fn))
  if watch_thread then
    t.watched = true
    watch_thread(t.co)
  end
  run(t)
  return t
end

function rt.join(h)
  local cur = current
  if cur.cancelled then error(CANCEL, 0) end
  local g = h.group
  if h.taken then error(MARK .. "this thread was joined already", 2) end
  while true do
    if g.failure then return abort(g) end
    if h.done then break end
    cur.waiting = true
    cur.waiting_on = h
    h.waiters[#h.waiters + 1] = cur
    park()
  end
  local r = h.result
  h.result = nil
  h.taken = true
  return r
end

function rt.sched_yield()
  local cur = current
  if cur.cancelled then error(CANCEL, 0) end
  enqueue(cur)
  park()
end

-- A loom block's guard, a `<close>` local: when the block ends, the threads
-- are waited for. Falling off the end only waits (and then raises a
-- failure); leaving by `return`, `break`, `?` or an unravel cancels first.
-- A failure in flight in the parent beats a thread's.
local loom_guard = {
  __close = function(c, err)
    local g = c[1]
    if g.done and err == nil and not current.cancelled then
      if g.failure then return abort(g) end
      -- Waiting can unravel (a deadlock): then this is an unwind, which
      -- cancels what's left and waits for it before it goes on.
      local ok, e = pcall(wait_all, g)
      if not ok then
        cancel_all(g)
        wait_all(g)
        error(e, 0)
      end
      if g.failure then return abort(g) end
      return
    end
    cancel_all(g)
    wait_all(g)
    if err == nil and g.failure then error(g.failure, 0) end
  end,
}

function rt.loom_guard(g)
  return setmetatable({ g }, loom_guard)
end

rt.ext["std.sync.Spun.join"] = function(_, h)
  return rt.join(h)
end

rt.ext["std.sync.sched_yield"] = function()
  rt.sched_yield()
end

-- `Atomic[T]` is `{value}`. With one worker nothing else runs between these
-- steps, so they're plain: sequentially consistent by being one at a time.
local atomic = rt.ext
atomic["std.sync.Atomic.load"] = function(_, a)
  return a[1]
end
atomic["std.sync.Atomic.store"] = function(_, a, v)
  a[1] = v
end
atomic["std.sync.Atomic.swap"] = function(_, a, v)
  local before = a[1]
  a[1] = v
  return before
end
atomic["std.sync.Atomic.compare_exchange"] = function(_, a, expected, desired)
  local found = a[1]
  if found == expected then
    a[1] = desired
    return { true, found }
  end
  return { false, found }
end
atomic["std.sync.Atomic.add"] = function(d, a, n)
  local before = a[1]
  local r, over = add_raw(d, before, n)
  if over then error(MARK .. "arithmetic overflow", 2) end
  a[1] = r
  return before
end
atomic["std.sync.Atomic.sub"] = function(d, a, n)
  local before = a[1]
  local r, over = sub_raw(d, before, n)
  if over then error(MARK .. "arithmetic overflow", 2) end
  a[1] = r
  return before
end

-- `mem.swap` and `mem.replace`: a table is changed in place (the caller's
-- `mut` argument is the table); anything else, a pointer included, comes back
-- after the result, like the scalar `mut` parameters of the other runtime
-- functions. A pointer's box is never changed: the pointers themselves move.
local function in_place(d, x)
  return type(x) == "table" and d.kind ~= "opt" and d.kind ~= "ptr"
end

rt.ext["std.mem.swap"] = function(d, a, b)
  if in_place(d, a) and in_place(d, b) then
    local t = {}
    for k, v in pairs(a) do t[k] = v end
    rt.overwrite(a, b)
    rt.overwrite(b, t)
    return nil, a, b
  end
  return nil, b, a
end

rt.ext["std.mem.replace"] = function(d, dest, value)
  if in_place(d, dest) and in_place(d, value) then
    local old = {}
    for k, v in pairs(dest) do old[k] = v end
    rt.overwrite(dest, value)
    return old, dest
  end
  return dest, value
end

-- Arenas. Lua has a collector, so a spool only has to stay out of the way: an
-- arena is `{capacity}`, `put` makes a box like `^e` does, and `reset` frees
-- nothing.
rt.ext["std.mem.Arena.reset"] = function() end
rt.ext["std.mem.Arena.put"] = function(_, _, v)
  return { v }
end

-- Its first chunk is made with the arena, so an arena past the host's memory
-- limit (8 GiB, as on Treadle) can't be made.
function rt.arena_new(capacity)
  if capacity > (8 << 30) then error(MARK .. "out of memory", 2) end
  return { capacity }
end

rt.ext["std.mem.SharedArena.reset"] = rt.ext["std.mem.Arena.reset"]
rt.ext["std.mem.SharedArena.put"] = rt.ext["std.mem.Arena.put"]

-- Pools. `{slots, in use}`: Lua has a collector, so a pool only counts, and
-- is full when the count says so. `slot` is the bytes each slot takes on
-- Treadle; a pool past the host's memory limit can't be made, as with arenas.
function rt.pool_new(slot, slots)
  if slots > (8 << 30) // slot then error(MARK .. "out of memory", 2) end
  return { slots }
end

rt.ext["std.mem.Pool.put"] = function(_, p, v)
  local live = (p[2] or 0) + 1
  if live > p[1] then error(MARK .. "the pool is full", 2) end
  p[2] = live
  return { v }
end
rt.ext["std.mem.Pool.free"] = function(_, p)
  p[2] = (p[2] or 1) - 1
end
rt.ext["std.mem.Pool.reset"] = function(_, p)
  p[2] = 0
end

-- Shuttles. A shuttle is `{co, ...}`: a coroutine running the function's
-- body, which `yield v` suspends with `true, v` and `pull` with `false`.
-- The driver resumes a pending `pull` with `true, input` for a push, or
-- `false` once the shuttle is closed.
--
-- A burst is `{sh, input, closing}`: going through it runs the shuttle,
-- handing over its input at the first `pull`, and ends at the next one (or
-- when the shuttle does). Pushing again drops what's left of the previous
-- burst, having run it. An unravel inside the shuttle closes its coroutine,
-- which runs its pending `defer`s, and carries on in the driver.


function rt.shuttle(body)
  -- Under a `pcall`, an unwind runs the body's guards in its own coroutine,
  -- where a park can yield to the driver.
  local co = co_create(function(...)
    local ok, e = pcall(body, ...)
    if not ok then error(e, 0) end
  end)
  if watch_thread then
    watched[co] = true
    watch_thread(co)
  end
  return { co = co, done = false, at_pull = false, closed = false, burst = nil }
end

--- `sh` is over: it needs no more interrupts.
local function finish(sh)
  sh.done = true
  if watched[sh.co] then
    watched[sh.co] = nil
    unwatch_thread(sh.co)
  end
end

function rt.out(v)
  if current.cancelled then error(CANCEL, 0) end
  if co_yield(true, v) == CLOSE then error(CLOSE, 0) end
end

function rt.pull(boxed)
  if current.cancelled then error(CANCEL, 0) end
  local has, v = co_yield(false)
  if has == CLOSE then error(CLOSE, 0) end
  if not has then return nil end
  if boxed then return { v } end
  return v
end

-- Resumes `sh` with `...`: gives "out" and the value, "pull", or "done".
local function resume(sh, ...)
  sh.started = true
  local cur = current
  cur.sdepth = cur.sdepth + 1
  local ok, is_out, v = co_resume(sh.co, ...)
  cur.sdepth = cur.sdepth - 1
  -- A park inside the body is the task's: it parks where the driver is,
  -- and the body goes on when the task is woken.
  while ok and is_out == PARK do
    suspend()
    cur.sdepth = cur.sdepth + 1
    ok, is_out, v = co_resume(sh.co)
    cur.sdepth = cur.sdepth - 1
  end
  if not ok then
    finish(sh)
    co_close(sh.co)
    error(is_out, 0)
  end
  if co_status(sh.co) == "dead" then
    finish(sh)
    return "done"
  end
  if is_out then return "out", v end
  return "pull"
end

--- Closes `sh` where it waits: resumed with CLOSE, it unwinds from its
--- `yield` or `pull` in its own coroutine, running its `defer`s and loom
--- guards, and a park on the way is the task's, as in `resume`. Gives what
--- a cleanup raised, if anything.
local function close_shuttle(sh)
  finish(sh)
  if not sh.started then
    local ok, e = co_close(sh.co)
    return not ok and e or nil
  end
  local cur = current
  cur.sdepth = cur.sdepth + 1
  local ok, e = co_resume(sh.co, CLOSE)
  cur.sdepth = cur.sdepth - 1
  while ok and e == PARK do
    suspend()
    cur.sdepth = cur.sdepth + 1
    ok, e = co_resume(sh.co)
    cur.sdepth = cur.sdepth - 1
  end
  co_close(sh.co)
  if not ok and e ~= CLOSE then return e end
  return nil
end

-- A projection lends one place for a statement: run to its first `yield`,
-- which gives the place as `{table, key}`, and on to its end once the
-- statement is done.
-- A lent place is a `<close>` local of the statement's block: leaving it
-- any way (an unravel too) closes the projection, which runs the `defer`s
-- it has reached, as dropping a shuttle does.
local lent_place = {
  __close = function(place, err)
    local sh = place.sh
    if sh.done then return end
    local e = close_shuttle(sh)
    if e and err == nil then error(e, 0) end
  end,
}

function rt.lend(sh)
  local what, place = resume(sh)
  if what ~= "out" then error(MARK .. "a projection ended without lending a place", 2) end
  place.sh = sh
  return setmetatable(place, lent_place)
end

function rt.lend_end(place)
  if place.sh.done then return end
  if resume(place.sh) == "out" then error(MARK .. "a projection lent more than one place", 2) end
end

-- `Mutex[T]` is `{value, holder, waiters}`: the thread that has it, and the
-- ones waiting for it, first first. `lock()` is a projection: the value's
-- place for a statement, held until the projection ends or is closed.
local function mutex_acquire(m)
  local cur = current
  if m.holder == cur then error(MARK .. "this thread holds the lock already", 0) end
  local ws = m.waiters
  if not ws then
    ws = {}
    m.waiters = ws
  end
  while m.holder ~= nil do
    cur.waiting = true
    cur.waiting_on = m
    ws[#ws + 1] = cur
    park()
  end
  m.holder = cur
end

local unlocks = {
  __close = function(g)
    local m = g[1]
    if m.holder ~= current then return end
    m.holder = nil
    local w = m.waiters and table.remove(m.waiters, 1)
    if w then
      w.waiting_on = nil
      wake(w)
    end
  end,
}

-- `Pipe[T]` is a ring of `cap` items from `head`, `n` of them, and the
-- threads waiting to send (when it's full) or receive (when it's empty), in
-- the order they came. A woken thread looks again: it may have lost.
function rt.pipe_new(cap)
  if cap == 0 then error(MARK .. "a pipe holds at least one item", 2) end
  local p = { cap = cap, head = 1, n = 0, closed = false, items = {}, senders = {}, receivers = {} }
  p.sending = { waiters = p.senders }
  p.receiving = { waiters = p.receivers }
  return p
end

local function pipe_wake(list)
  local w = table.remove(list, 1)
  if w then
    w.waiting_on = nil
    wake(w)
  end
end

local function pipe_wait(on, list)
  local cur = current
  cur.waiting = true
  cur.waiting_on = on
  list[#list + 1] = cur
  park()
end

ext["std.sync.Pipe.send"] = function(_, p, item)
  while true do
    if p.closed then error(MARK .. "send on a closed pipe", 2) end
    if p.n < p.cap then break end
    pipe_wait(p.sending, p.senders)
  end
  p.items[(p.head - 1 + p.n) % p.cap + 1] = item
  p.n = p.n + 1
  pipe_wake(p.receivers)
end

ext["std.sync.Pipe.recv"] = function(d, p)
  while true do
    if p.n > 0 then
      local x = p.items[p.head]
      p.items[p.head] = nil
      p.head = p.head % p.cap + 1
      p.n = p.n - 1
      pipe_wake(p.senders)
      return some(d, x)
    end
    if p.closed then return nil end
    pipe_wait(p.receiving, p.receivers)
  end
end

ext["std.sync.Pipe.close"] = function(_, p)
  p.closed = true
  local all = {}
  for _, w in ipairs(p.senders) do all[#all + 1] = w end
  for _, w in ipairs(p.receivers) do all[#all + 1] = w end
  p.senders, p.receivers = {}, {}
  p.sending.waiters, p.receiving.waiters = p.senders, p.receivers
  for _, w in ipairs(all) do
    w.waiting_on = nil
    wake(w)
  end
end

ext["std.sync.Mutex.lock"] = function(_, m)
  return rt.shuttle(function()
    mutex_acquire(m)
    local held <close> = setmetatable({ m }, unlocks)
    co_yield(true, { m, 1 })
  end)
end

-- The next item of burst `b`: `true, item`, or nothing once it's over.
local function burst_next(b)
  local sh = b.sh
  while not b.over and not sh.done do
    local what, v
    if not sh.at_pull then
      what, v = resume(sh)
    elseif not b.delivered then
      b.delivered = true
      sh.at_pull = false
      if b.closing then
        what, v = resume(sh, false)
      else
        what, v = resume(sh, true, b.input)
      end
    elseif sh.closed then
      sh.at_pull = false
      what, v = resume(sh, false)
    else
      break
    end
    if what == "out" then return true, v end
    if what == "pull" then sh.at_pull = true end
  end
  b.over = true
  if sh.burst == b then sh.burst = nil end
  return nil
end

local function drain(b)
  while burst_next(b) do end
end

local function new_burst(sh, input, closing)
  if sh.burst then drain(sh.burst) end
  local b = { sh = sh, input = input, closing = closing, delivered = false, over = false }
  sh.burst = b
  return b
end

ext["std.iter.Shuttle.push"] = function(_, _, sh, input)
  if sh.closed then error(MARK .. "can't push into a closed shuttle", 2) end
  return new_burst(sh, input, false)
end

-- What's left of the burst before runs first, as for a push: its `pull`s
-- still wait for input.
ext["std.iter.Shuttle.close"] = function(_, _, sh)
  if sh.burst then drain(sh.burst) end
  sh.closed = true
  return new_burst(sh, nil, true)
end

ext["std.iter.Burst.drain"] = function(_, b)
  drain(b)
end

function rt.burst_stream(b)
  return function() return burst_next(b) end
end

--- A generator's items (it has no `pull` to stop at).
function rt.shuttle_stream(sh)
  local b = { sh = sh, delivered = true, over = false }
  return function() return burst_next(b) end
end

-- Closing a generator runs its pending `defer`s: one that unravels
-- unravels here, in whoever closed it, unless something already is.
local closes_shuttle = {
  __close = function(c, err)
    if c.sh.done then return end
    local e = close_shuttle(c.sh)
    if e and err == nil then error(e, 0) end
  end,
}

--- `for x in generator`: the stream, and a value that closes the generator
--- when the loop ends (early, its `defer`s run).
function rt.shuttle_loop(sh)
  return rt.shuttle_stream(sh), nil, nil, setmetatable({ sh = sh }, closes_shuttle)
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

ext["std.iter.Stream.filter_map"] = function(_, du, s, f)
  local boxed = du.kind == "opt" or du.kind == "unit"
  return function()
    while true do
      local ok, v = s()
      if not ok then return nil end
      local y = f(v)
      if y ~= nil then
        if boxed then y = y[1] end
        return true, y
      end
    end
  end
end

ext["std.iter.Stream.inspect"] = function(_, s, f)
  return function()
    local ok, v = s()
    if not ok then return nil end
    f(v)
    return true, v
  end
end

ext["std.iter.Stream.step_by"] = function(_, s, n)
  if n == 0 then error(MARK .. "step_by of 0 items", 2) end
  local started = false
  return function()
    if started then
      for _ = 2, n do
        if not s() then return nil end
      end
    end
    started = true
    return s()
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
    local single = d.name == "f32"
    local acc = float and start + 0.0 or start
    while true do
      local ok, v = s()
      if not ok then break end
      if float then
        -- An f32 sum rounds at every step, as f32 additions do.
        acc = op(acc, v)
        if single then acc = rt.f32(acc) end
      else
        local r, o = raw(d, acc, v)
        if o then error(MARK .. "arithmetic overflow", 2) end
        acc = r
      end
    end
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

-- Ranges of `size` values at a time, as `{lo, last, true}`: `last` is in
-- the range, which a narrow type's end past it (256 for a `u8`) would not be.
ext["std.iter.Range.chunks"] = function(d, r, size)
  if size == 0 then error(MARK .. "chunks of 0 values", 2) end
  local at, hi, incl = r[1], r[2], r[3]
  if hi == nil then error(MARK .. "chunks of a range with no end", 2) end
  local u64 = d.u64
  if incl then
    local over = hi + 1
    if (u64 and over == 0) or (not u64 and over < hi) then
      error(MARK .. "chunks of a range that includes its type's maximum", 2)
    end
    hi = over
  end
  return function()
    local inside
    if u64 then inside = ult(at, hi) else inside = at < hi end
    if not inside then return nil end
    local step, left = size, hi - at
    if ult(left, step) then step = left end
    local from = at
    at = at + step
    return true, { from, at - 1, true }
  end
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

-- The pieces are views of the list itself, which is what lets a thread
-- change its own.
ext["std.slice.Slice.chunks_mut"] = ext["std.slice.Slice.chunks"]

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

-- A number for each descriptor, handed out as needed: `i32 | i64` keys
-- 1 and 1 differ by their types, which their text alone doesn't show.
local next_uid = 0
local function uid(d)
  if d.uid == nil then
    next_uid = next_uid + 1
    d.uid = next_uid
  end
  return d.uid
end

local function hkey(d, k)
  local kind = d.kind
  if kind == "int" or kind == "str" or kind == "char" or kind == "bool" or kind == "enum" then return k end
  if kind == "any" then return uid(k[2].d) .. ":" .. show(k[2].d, k[1], true) end
  return show(d, k, true)
end

function rt.map_new()
  return { keys = {}, vals = {}, hks = {}, idx = {}, n = 0, top = 0 }
end

function rt.set_new()
  return { keys = {}, hks = {}, idx = {}, n = 0, top = 0 }
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
  local n = m.top + 1
  m.keys[n], m.hks[n], m.idx[h], m.top, m.n = k, h, n, n, m.n + 1
  if m.vals then m.vals[n] = v end
  return false, nil
end

--- Takes out the entry at position `i`: it stays as a removed one until
--- there are more of them than entries left.
local function take_at(m, i)
  m.idx[m.hks[i]] = nil
  m.hks[i], m.keys[i] = DEAD, false
  if m.vals then m.vals[i] = false end
  local n = m.n - 1
  m.n = n
  if m.top - n > n and m.top > 8 then compact(m) end
end

--- `m[k] = v`.
function rt.map_set(dk, m, k, v)
  map_put(dk, m, k, v)
end

-- The operations on a map, specialized to its key type (`rt.spec`): for an
-- integer, text, char, bool or enum key the key is its own hash, so the
-- hash isn't worked out by looking at the descriptor on every call.
function spec.plain(dk)
  local kind = dk.kind
  return kind == "int" or kind == "str" or kind == "char" or kind == "bool" or kind == "enum"
end

function spec.map_has(dk)
  if spec.plain(dk) then
    return function(m, k)
      return m.idx[k] ~= nil
    end
  end
  return function(m, k)
    return m.idx[hkey(dk, k)] ~= nil
  end
end

function spec.map_at(dk)
  if spec.plain(dk) then
    return function(m, k)
      local i = m.idx[k]
      if i == nil then error(sformat("%skey not found: %s", MARK, show(dk, k, true)), 2) end
      return m.vals[i]
    end
  end
  return function(m, k)
    return rt.map_at(dk, m, k)
  end
end

function spec.map_set(dk)
  if spec.plain(dk) then
    return function(m, k, v)
      local i = m.idx[k]
      if i ~= nil then
        m.vals[i] = v
        return
      end
      local n = m.top + 1
      m.keys[n], m.hks[n], m.idx[k], m.top, m.n = k, k, n, n, m.n + 1
      m.vals[n] = v
    end
  end
  return function(m, k, v)
    map_put(dk, m, k, v)
  end
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

-- A place for the value of `k`: the entry is made first when there's none
-- (the shuttle yields `{values, slot}`, which is what a place is).
ext["std.map.Map.entry"] = function(dk, _, m, k, default)
  return rt.shuttle(function()
    local i = m.idx[hkey(dk, k)]
    if i == nil then
      map_put(dk, m, k, default)
      i = m.idx[hkey(dk, k)]
    end
    co_yield(true, { m.vals, i })
  end)
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
  m.keys, m.hks, m.idx, m.n, m.top, m.head = {}, {}, {}, 0, 0, 0
  if m.vals then m.vals = {} end
end

ext["std.map.Map.clear"] = function(_, _, m)
  coll_clear(m)
end

--- Copies of `m`'s entries, one at a time, made by `item(i)`. It skips the
--- removed entries, and `m.head` remembers the first entry that isn't one.
local function coll_stream(m, item)
  local i, n = m.head or 0, m.n
  local hks = m.hks
  while i < m.top and hks[i + 1] == DEAD do
    i = i + 1
  end
  m.head = i
  return function()
    rt.same_len(m, n, m.vals and "map" or "set")
    local hks = m.hks
    while i < m.top do
      i = i + 1
      if hks[i] ~= DEAD then return true, item(i) end
    end
    return nil
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
  compact(m)
  local out, n = {}, m.n
  for i = 1, n do
    out[i] = { copy(dk, m.keys[i]), copy(dv, m.vals[i]) }
  end
  sort_by(dk, out, n, function(p) return p[1] end)
  out.n = n
  return out
end

local function copy_map(dk, dv, m)
  compact(m)
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
  compact(s)
  local out, n = {}, s.n
  for i = 1, n do
    out[i] = copy(d, s.keys[i])
  end
  sort_by(d, out, n, function(x) return x end)
  out.n = n
  return out
end

local function copy_set(d, s)
  compact(s)
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

-- Files. A `File` is `{path}`; a write appends to the file, which stays open
-- (line buffered, so each line is written at once; a few at a time, and all
-- closed when the state is, however the program ends). A captured run (the
-- tests) gets a file system in memory from the host instead of the disk,
-- starting empty.

local memory = host and host.memory_fs and {} or nil

--- `T ! IoError` for an `io` failure: errno 2 is `NotFound`, 13 `Denied`.
local function io_error(path, message, errno)
  if errno == 2 then return { false, { tag = 1, path = path } } end
  if errno == 13 then return { false, { tag = 2, path = path } } end
  return { false, { tag = 3, message = message or "i/o error" } }
end

local open_files, open_count, MAX_OPEN = {}, 0, 64

local function append(path, text)
  if memory then
    memory[path] = (memory[path] or "") .. text
    return true
  end
  local f = open_files[path]
  if not f then
    -- Not many at once: a program writing to thousands of files would run
    -- out of what the system gives.
    if open_count >= MAX_OPEN then
      for p, h in pairs(open_files) do
        h:close()
        open_files[p] = nil
      end
      open_count = 0
    end
    local message, errno
    f, message, errno = io.open(path, "ab")
    if not f then return false, message, errno end
    f:setvbuf("line")
    open_files[path] = f
    open_count = open_count + 1
  end
  local ok, werr, wno = f:write(text)
  if not ok then return false, werr, wno end
  return true
end

local function read_all(path)
  if memory then
    local text = memory[path]
    if text == nil then return nil, path .. ": No such file or directory", 2 end
    return text
  end
  local h = open_files[path]
  if h then h:flush() end
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

ext["std.fs.write_text"] = function(path, text)
  if memory then
    memory[path] = text
    return { true, nil }
  end
  local h = open_files[path]
  if h then
    h:close()
    open_files[path] = nil
    open_count = open_count - 1
  end
  local f, message, errno = io.open(path, "wb")
  if not f then return io_error(path, message, errno) end
  local ok, werr, wno = f:write(text)
  f:close()
  if not ok then return io_error(path, werr, wno) end
  return { true, nil }
end

ext["std.fs.exists"] = function(path)
  if memory then return memory[path] ~= nil end
  local f = io.open(path, "rb")
  if f then
    f:close()
    return true
  end
  return false
end

ext["std.fs.create_dir_all"] = function(path)
  if memory then return { true, nil } end
  local ok, message, errno = host.make_dir(path)
  if not ok then return io_error(path, message, errno) end
  return { true, nil }
end

local function move_file(from, to, keep)
  if memory then
    local text = memory[from]
    if text == nil then return io_error(from, nil, 2) end
    memory[to] = text
    if not keep then memory[from] = nil end
    return { true, nil }
  end
  for _, path in ipairs({ from, to }) do
    local h = open_files[path]
    if h then
      h:close()
      open_files[path] = nil
      open_count = open_count - 1
    end
  end
  local ok, message, errno = host.move_file(from, to, keep)
  if not ok then return io_error(from, message, errno) end
  return { true, nil }
end

ext["std.fs.rename"] = function(from, to)
  return move_file(from, to, false)
end

ext["std.fs.copy"] = function(from, to)
  return move_file(from, to, true)
end

ext["std.fs.read_dir"] = function(path)
  local names, message, errno
  if memory then
    local prefix = (path == "" or path:sub(-1) == "/") and path or path .. "/"
    local seen = {}
    names = {}
    for key in pairs(memory) do
      if key:sub(1, #prefix) == prefix then
        local name = key:sub(#prefix + 1):match("^[^/]*")
        if not seen[name] then
          seen[name] = true
          names[#names + 1] = name
        end
      end
    end
    table.sort(names)
  else
    names, message, errno = host.list_dir(path)
    if not names then return io_error(path, message, errno) end
  end
  local list = { n = #names }
  for i = 1, #names do
    list[i] = names[i]
  end
  return { true, list }
end

ext["std.fs.file_kind"] = function(path)
  if memory then return memory[path] ~= nil and 1 or 0 end
  return (host.file_info(path))
end

ext["std.fs.file_size"] = function(path)
  if memory then return memory[path] and #memory[path] or 0 end
  return select(2, host.file_info(path))
end

ext["std.time.unix_nanos"] = function()
  if not host then needs_host("time.unix") end
  return host.unix_nanos()
end

ext["std.process.id"] = function()
  if not host then needs_host("process.id") end
  return host.pid()
end

ext["std.env.current_dir"] = function()
  if not host then needs_host("env.cwd") end
  return host.cwd()
end

ext["std.term.columns"] = function()
  if not host then needs_host("term.info") end
  return host.term_info(0)
end

ext["std.term.rows"] = function()
  if not host then needs_host("term.info") end
  return host.term_info(1)
end

ext["std.term.is_tty"] = function()
  if not host then needs_host("term.info") end
  return host.term_info(2) ~= 0
end

ext["std.fs.remove_file"] = function(path)
  if memory then
    if memory[path] == nil then return io_error(path, nil, 2) end
    memory[path] = nil
    return { true, nil }
  end
  local h = open_files[path]
  if h then
    h:close()
    open_files[path] = nil
    open_count = open_count - 1
  end
  local ok, message, errno = os.remove(path)
  if not ok then return io_error(path, message, errno) end
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

-- BEGIN unicode tables (generated by crates/loom_vm/tests/unicode_tables.rs)
local UNICODE = {
  alphabetic = "41-5a,61-7a,aa,b5,ba,c0-d6,d8-f6,f8-2c1,2c6-2d1,2e0-2e4,2ec,2ee,345,363-374,376-377,37a-37d,37f,386,388-38a,38c,38e-3a1,3a3-3f5,3f7-481,48a-52f,531-556,559,560-588,5b0-5bd,5bf,5c1-5c2,5c4-5c5,5c7,5d0-5ea,5ef-5f2,610-61a,620-657,659-65f,66e-6d3,6d5-6dc,6e1-6e8,6ed-6ef,6fa-6fc,6ff,710-73f,74d-7b1,7ca-7ea,7f4-7f5,7fa,800-817,81a-82c,840-858,860-86a,870-887,889-88f,897,8a0-8c9,8d4-8df,8e3-8e9,8f0-93b,93d-94c,94e-950,955-963,971-983,985-98c,98f-990,993-9a8,9aa-9b0,9b2,9b6-9b9,9bd-9c4,9c7-9c8,9cb-9cc,9ce,9d7,9dc-9dd,9df-9e3,9f0-9f1,9fc,a01-a03,a05-a0a,a0f-a10,a13-a28,a2a-a30,a32-a33,a35-a36,a38-a39,a3e-a42,a47-a48,a4b-a4c,a51,a59-a5c,a5e,a70-a75,a81-a83,a85-a8d,a8f-a91,a93-aa8,aaa-ab0,ab2-ab3,ab5-ab9,abd-ac5,ac7-ac9,acb-acc,ad0,ae0-ae3,af9-afc,b01-b03,b05-b0c,b0f-b10,b13-b28,b2a-b30,b32-b33,b35-b39,b3d-b44,b47-b48,b4b-b4c,b56-b57,b5c-b5d,b5f-b63,b71,b82-b83,b85-b8a,b8e-b90,b92-b95,b99-b9a,b9c,b9e-b9f,ba3-ba4,ba8-baa,bae-bb9,bbe-bc2,bc6-bc8,bca-bcc,bd0,bd7,c00-c0c,c0e-c10,c12-c28,c2a-c39,c3d-c44,c46-c48,c4a-c4c,c55-c56,c58-c5a,c5c-c5d,c60-c63,c80-c83,c85-c8c,c8e-c90,c92-ca8,caa-cb3,cb5-cb9,cbd-cc4,cc6-cc8,cca-ccc,cd5-cd6,cdc-cde,ce0-ce3,cf1-cf3,d00-d0c,d0e-d10,d12-d3a,d3d-d44,d46-d48,d4a-d4c,d4e,d54-d57,d5f-d63,d7a-d7f,d81-d83,d85-d96,d9a-db1,db3-dbb,dbd,dc0-dc6,dcf-dd4,dd6,dd8-ddf,df2-df3,e01-e3a,e40-e46,e4d,e81-e82,e84,e86-e8a,e8c-ea3,ea5,ea7-eb9,ebb-ebd,ec0-ec4,ec6,ecd,edc-edf,f00,f40-f47,f49-f6c,f71-f83,f88-f97,f99-fbc,1000-1036,1038,103b-103f,1050-108f,109a-109d,10a0-10c5,10c7,10cd,10d0-10fa,10fc-1248,124a-124d,1250-1256,1258,125a-125d,1260-1288,128a-128d,1290-12b0,12b2-12b5,12b8-12be,12c0,12c2-12c5,12c8-12d6,12d8-1310,1312-1315,1318-135a,1380-138f,13a0-13f5,13f8-13fd,1401-166c,166f-167f,1681-169a,16a0-16ea,16ee-16f8,1700-1713,171f-1733,1740-1753,1760-176c,176e-1770,1772-1773,1780-17b3,17b6-17c8,17d7,17dc,1820-1878,1880-18aa,18b0-18f5,1900-191e,1920-192b,1930-1938,1950-196d,1970-1974,1980-19ab,19b0-19c9,1a00-1a1b,1a20-1a5e,1a61-1a74,1aa7,1abf-1ac0,1acc-1ace,1b00-1b33,1b35-1b43,1b45-1b4c,1b80-1ba9,1bac-1baf,1bba-1be5,1be7-1bf1,1c00-1c36,1c4d-1c4f,1c5a-1c7d,1c80-1c8a,1c90-1cba,1cbd-1cbf,1ce9-1cec,1cee-1cf3,1cf5-1cf6,1cfa,1d00-1dbf,1dd3-1df4,1e00-1f15,1f18-1f1d,1f20-1f45,1f48-1f4d,1f50-1f57,1f59,1f5b,1f5d,1f5f-1f7d,1f80-1fb4,1fb6-1fbc,1fbe,1fc2-1fc4,1fc6-1fcc,1fd0-1fd3,1fd6-1fdb,1fe0-1fec,1ff2-1ff4,1ff6-1ffc,2071,207f,2090-209c,2102,2107,210a-2113,2115,2119-211d,2124,2126,2128,212a-212d,212f-2139,213c-213f,2145-2149,214e,2160-2188,24b6-24e9,2c00-2ce4,2ceb-2cee,2cf2-2cf3,2d00-2d25,2d27,2d2d,2d30-2d67,2d6f,2d80-2d96,2da0-2da6,2da8-2dae,2db0-2db6,2db8-2dbe,2dc0-2dc6,2dc8-2dce,2dd0-2dd6,2dd8-2dde,2de0-2dff,2e2f,3005-3007,3021-3029,3031-3035,3038-303c,3041-3096,309d-309f,30a1-30fa,30fc-30ff,3105-312f,3131-318e,31a0-31bf,31f0-31ff,3400-4dbf,4e00-a48c,a4d0-a4fd,a500-a60c,a610-a61f,a62a-a62b,a640-a66e,a674-a67b,a67f-a6ef,a717-a71f,a722-a788,a78b-a7dc,a7f1-a805,a807-a827,a840-a873,a880-a8c3,a8c5,a8f2-a8f7,a8fb,a8fd-a8ff,a90a-a92a,a930-a952,a960-a97c,a980-a9b2,a9b4-a9bf,a9cf,a9e0-a9ef,a9fa-a9fe,aa00-aa36,aa40-aa4d,aa60-aa76,aa7a-aabe,aac0,aac2,aadb-aadd,aae0-aaef,aaf2-aaf5,ab01-ab06,ab09-ab0e,ab11-ab16,ab20-ab26,ab28-ab2e,ab30-ab5a,ab5c-ab69,ab70-abea,ac00-d7a3,d7b0-d7c6,d7cb-d7fb,f900-fa6d,fa70-fad9,fb00-fb06,fb13-fb17,fb1d-fb28,fb2a-fb36,fb38-fb3c,fb3e,fb40-fb41,fb43-fb44,fb46-fbb1,fbd3-fd3d,fd50-fd8f,fd92-fdc7,fdf0-fdfb,fe70-fe74,fe76-fefc,ff21-ff3a,ff41-ff5a,ff66-ffbe,ffc2-ffc7,ffca-ffcf,ffd2-ffd7,ffda-ffdc,10000-1000b,1000d-10026,10028-1003a,1003c-1003d,1003f-1004d,10050-1005d,10080-100fa,10140-10174,10280-1029c,102a0-102d0,10300-1031f,1032d-1034a,10350-1037a,10380-1039d,103a0-103c3,103c8-103cf,103d1-103d5,10400-1049d,104b0-104d3,104d8-104fb,10500-10527,10530-10563,10570-1057a,1057c-1058a,1058c-10592,10594-10595,10597-105a1,105a3-105b1,105b3-105b9,105bb-105bc,105c0-105f3,10600-10736,10740-10755,10760-10767,10780-10785,10787-107b0,107b2-107ba,10800-10805,10808,1080a-10835,10837-10838,1083c,1083f-10855,10860-10876,10880-1089e,108e0-108f2,108f4-108f5,10900-10915,10920-10939,10940-10959,10980-109b7,109be-109bf,10a00-10a03,10a05-10a06,10a0c-10a13,10a15-10a17,10a19-10a35,10a60-10a7c,10a80-10a9c,10ac0-10ac7,10ac9-10ae4,10b00-10b35,10b40-10b55,10b60-10b72,10b80-10b91,10c00-10c48,10c80-10cb2,10cc0-10cf2,10d00-10d27,10d4a-10d65,10d69,10d6f-10d85,10e80-10ea9,10eab-10eac,10eb0-10eb1,10ec2-10ec7,10efa-10efc,10f00-10f1c,10f27,10f30-10f45,10f70-10f81,10fb0-10fc4,10fe0-10ff6,11000-11045,11071-11075,11080-110b8,110c2,110d0-110e8,11100-11132,11144-11147,11150-11172,11176,11180-111bf,111c1-111c4,111ce-111cf,111da,111dc,11200-11211,11213-11234,11237,1123e-11241,11280-11286,11288,1128a-1128d,1128f-1129d,1129f-112a8,112b0-112e8,11300-11303,11305-1130c,1130f-11310,11313-11328,1132a-11330,11332-11333,11335-11339,1133d-11344,11347-11348,1134b-1134c,11350,11357,1135d-11363,11380-11389,1138b,1138e,11390-113b5,113b7-113c0,113c2,113c5,113c7-113ca,113cc-113cd,113d1,113d3,11400-11441,11443-11445,11447-1144a,1145f-11461,11480-114c1,114c4-114c5,114c7,11580-115b5,115b8-115be,115d8-115dd,11600-1163e,11640,11644,11680-116b5,116b8,11700-1171a,1171d-1172a,11740-11746,11800-11838,118a0-118df,118ff-11906,11909,1190c-11913,11915-11916,11918-11935,11937-11938,1193b-1193c,1193f-11942,119a0-119a7,119aa-119d7,119da-119df,119e1,119e3-119e4,11a00-11a32,11a35-11a3e,11a50-11a97,11a9d,11ab0-11af8,11b60-11b67,11bc0-11be0,11c00-11c08,11c0a-11c36,11c38-11c3e,11c40,11c72-11c8f,11c92-11ca7,11ca9-11cb6,11d00-11d06,11d08-11d09,11d0b-11d36,11d3a,11d3c-11d3d,11d3f-11d41,11d43,11d46-11d47,11d60-11d65,11d67-11d68,11d6a-11d8e,11d90-11d91,11d93-11d96,11d98,11db0-11ddb,11ee0-11ef6,11f00-11f10,11f12-11f3a,11f3e-11f40,11fb0,12000-12399,12400-1246e,12480-12543,12f90-12ff0,13000-1342f,13441-13446,13460-143fa,14400-14646,16100-1612e,16800-16a38,16a40-16a5e,16a70-16abe,16ad0-16aed,16b00-16b2f,16b40-16b43,16b63-16b77,16b7d-16b8f,16d40-16d6c,16e40-16e7f,16ea0-16eb8,16ebb-16ed3,16f00-16f4a,16f4f-16f87,16f8f-16f9f,16fe0-16fe1,16fe3,16ff0-16ff6,17000-18cd5,18cff-18d1e,18d80-18df2,1aff0-1aff3,1aff5-1affb,1affd-1affe,1b000-1b122,1b132,1b150-1b152,1b155,1b164-1b167,1b170-1b2fb,1bc00-1bc6a,1bc70-1bc7c,1bc80-1bc88,1bc90-1bc99,1bc9e,1d400-1d454,1d456-1d49c,1d49e-1d49f,1d4a2,1d4a5-1d4a6,1d4a9-1d4ac,1d4ae-1d4b9,1d4bb,1d4bd-1d4c3,1d4c5-1d505,1d507-1d50a,1d50d-1d514,1d516-1d51c,1d51e-1d539,1d53b-1d53e,1d540-1d544,1d546,1d54a-1d550,1d552-1d6a5,1d6a8-1d6c0,1d6c2-1d6da,1d6dc-1d6fa,1d6fc-1d714,1d716-1d734,1d736-1d74e,1d750-1d76e,1d770-1d788,1d78a-1d7a8,1d7aa-1d7c2,1d7c4-1d7cb,1df00-1df1e,1df25-1df2a,1e000-1e006,1e008-1e018,1e01b-1e021,1e023-1e024,1e026-1e02a,1e030-1e06d,1e08f,1e100-1e12c,1e137-1e13d,1e14e,1e290-1e2ad,1e2c0-1e2eb,1e4d0-1e4eb,1e5d0-1e5ed,1e5f0,1e6c0-1e6de,1e6e0-1e6f5,1e6fe-1e6ff,1e7e0-1e7e6,1e7e8-1e7eb,1e7ed-1e7ee,1e7f0-1e7fe,1e800-1e8c4,1e900-1e943,1e947,1e94b,1ee00-1ee03,1ee05-1ee1f,1ee21-1ee22,1ee24,1ee27,1ee29-1ee32,1ee34-1ee37,1ee39,1ee3b,1ee42,1ee47,1ee49,1ee4b,1ee4d-1ee4f,1ee51-1ee52,1ee54,1ee57,1ee59,1ee5b,1ee5d,1ee5f,1ee61-1ee62,1ee64,1ee67-1ee6a,1ee6c-1ee72,1ee74-1ee77,1ee79-1ee7c,1ee7e,1ee80-1ee89,1ee8b-1ee9b,1eea1-1eea3,1eea5-1eea9,1eeab-1eebb,1f130-1f149,1f150-1f169,1f170-1f189,20000-2a6df,2a700-2b81d,2b820-2cead,2ceb0-2ebe0,2ebf0-2ee5d,2f800-2fa1d,30000-3134a,31350-33479",
  numeric = "30-39,b2-b3,b9,bc-be,660-669,6f0-6f9,7c0-7c9,966-96f,9e6-9ef,9f4-9f9,a66-a6f,ae6-aef,b66-b6f,b72-b77,be6-bf2,c66-c6f,c78-c7e,ce6-cef,d58-d5e,d66-d78,de6-def,e50-e59,ed0-ed9,f20-f33,1040-1049,1090-1099,1369-137c,16ee-16f0,17e0-17e9,17f0-17f9,1810-1819,1946-194f,19d0-19da,1a80-1a89,1a90-1a99,1b50-1b59,1bb0-1bb9,1c40-1c49,1c50-1c59,2070,2074-2079,2080-2089,2150-2182,2185-2189,2460-249b,24ea-24ff,2776-2793,2cfd,3007,3021-3029,3038-303a,3192-3195,3220-3229,3248-324f,3251-325f,3280-3289,32b1-32bf,a620-a629,a6e6-a6ef,a830-a835,a8d0-a8d9,a900-a909,a9d0-a9d9,a9f0-a9f9,aa50-aa59,abf0-abf9,ff10-ff19,10107-10133,10140-10178,1018a-1018b,102e1-102fb,10320-10323,10341,1034a,103d1-103d5,104a0-104a9,10858-1085f,10879-1087f,108a7-108af,108fb-108ff,10916-1091b,109bc-109bd,109c0-109cf,109d2-109ff,10a40-10a48,10a7d-10a7e,10a9d-10a9f,10aeb-10aef,10b58-10b5f,10b78-10b7f,10ba9-10baf,10cfa-10cff,10d30-10d39,10d40-10d49,10e60-10e7e,10f1d-10f26,10f51-10f54,10fc5-10fcb,11052-1106f,110f0-110f9,11136-1113f,111d0-111d9,111e1-111f4,112f0-112f9,11450-11459,114d0-114d9,11650-11659,116c0-116c9,116d0-116e3,11730-1173b,118e0-118f2,11950-11959,11bf0-11bf9,11c50-11c6c,11d50-11d59,11da0-11da9,11de0-11de9,11f50-11f59,11fc0-11fd4,12400-1246e,16130-16139,16a60-16a69,16ac0-16ac9,16b50-16b59,16b5b-16b61,16d70-16d79,16e80-16e96,16ff4-16ff6,1ccf0-1ccf9,1d2c0-1d2d3,1d2e0-1d2f3,1d360-1d378,1d7ce-1d7ff,1e140-1e149,1e2f0-1e2f9,1e4f0-1e4f9,1e5f1-1e5fa,1e8c7-1e8cf,1e950-1e959,1ec71-1ecab,1ecad-1ecaf,1ecb1-1ecb4,1ed01-1ed2d,1ed2f-1ed3d,1f100-1f10c,1fbf0-1fbf9",
  upper = "41-5a,c0-d6,d8-de,100,102,104,106,108,10a,10c,10e,110,112,114,116,118,11a,11c,11e,120,122,124,126,128,12a,12c,12e,130,132,134,136,139,13b,13d,13f,141,143,145,147,14a,14c,14e,150,152,154,156,158,15a,15c,15e,160,162,164,166,168,16a,16c,16e,170,172,174,176,178-179,17b,17d,181-182,184,186-187,189-18b,18e-191,193-194,196-198,19c-19d,19f-1a0,1a2,1a4,1a6-1a7,1a9,1ac,1ae-1af,1b1-1b3,1b5,1b7-1b8,1bc,1c4,1c7,1ca,1cd,1cf,1d1,1d3,1d5,1d7,1d9,1db,1de,1e0,1e2,1e4,1e6,1e8,1ea,1ec,1ee,1f1,1f4,1f6-1f8,1fa,1fc,1fe,200,202,204,206,208,20a,20c,20e,210,212,214,216,218,21a,21c,21e,220,222,224,226,228,22a,22c,22e,230,232,23a-23b,23d-23e,241,243-246,248,24a,24c,24e,370,372,376,37f,386,388-38a,38c,38e-38f,391-3a1,3a3-3ab,3cf,3d2-3d4,3d8,3da,3dc,3de,3e0,3e2,3e4,3e6,3e8,3ea,3ec,3ee,3f4,3f7,3f9-3fa,3fd-42f,460,462,464,466,468,46a,46c,46e,470,472,474,476,478,47a,47c,47e,480,48a,48c,48e,490,492,494,496,498,49a,49c,49e,4a0,4a2,4a4,4a6,4a8,4aa,4ac,4ae,4b0,4b2,4b4,4b6,4b8,4ba,4bc,4be,4c0-4c1,4c3,4c5,4c7,4c9,4cb,4cd,4d0,4d2,4d4,4d6,4d8,4da,4dc,4de,4e0,4e2,4e4,4e6,4e8,4ea,4ec,4ee,4f0,4f2,4f4,4f6,4f8,4fa,4fc,4fe,500,502,504,506,508,50a,50c,50e,510,512,514,516,518,51a,51c,51e,520,522,524,526,528,52a,52c,52e,531-556,10a0-10c5,10c7,10cd,13a0-13f5,1c89,1c90-1cba,1cbd-1cbf,1e00,1e02,1e04,1e06,1e08,1e0a,1e0c,1e0e,1e10,1e12,1e14,1e16,1e18,1e1a,1e1c,1e1e,1e20,1e22,1e24,1e26,1e28,1e2a,1e2c,1e2e,1e30,1e32,1e34,1e36,1e38,1e3a,1e3c,1e3e,1e40,1e42,1e44,1e46,1e48,1e4a,1e4c,1e4e,1e50,1e52,1e54,1e56,1e58,1e5a,1e5c,1e5e,1e60,1e62,1e64,1e66,1e68,1e6a,1e6c,1e6e,1e70,1e72,1e74,1e76,1e78,1e7a,1e7c,1e7e,1e80,1e82,1e84,1e86,1e88,1e8a,1e8c,1e8e,1e90,1e92,1e94,1e9e,1ea0,1ea2,1ea4,1ea6,1ea8,1eaa,1eac,1eae,1eb0,1eb2,1eb4,1eb6,1eb8,1eba,1ebc,1ebe,1ec0,1ec2,1ec4,1ec6,1ec8,1eca,1ecc,1ece,1ed0,1ed2,1ed4,1ed6,1ed8,1eda,1edc,1ede,1ee0,1ee2,1ee4,1ee6,1ee8,1eea,1eec,1eee,1ef0,1ef2,1ef4,1ef6,1ef8,1efa,1efc,1efe,1f08-1f0f,1f18-1f1d,1f28-1f2f,1f38-1f3f,1f48-1f4d,1f59,1f5b,1f5d,1f5f,1f68-1f6f,1fb8-1fbb,1fc8-1fcb,1fd8-1fdb,1fe8-1fec,1ff8-1ffb,2102,2107,210b-210d,2110-2112,2115,2119-211d,2124,2126,2128,212a-212d,2130-2133,213e-213f,2145,2160-216f,2183,24b6-24cf,2c00-2c2f,2c60,2c62-2c64,2c67,2c69,2c6b,2c6d-2c70,2c72,2c75,2c7e-2c80,2c82,2c84,2c86,2c88,2c8a,2c8c,2c8e,2c90,2c92,2c94,2c96,2c98,2c9a,2c9c,2c9e,2ca0,2ca2,2ca4,2ca6,2ca8,2caa,2cac,2cae,2cb0,2cb2,2cb4,2cb6,2cb8,2cba,2cbc,2cbe,2cc0,2cc2,2cc4,2cc6,2cc8,2cca,2ccc,2cce,2cd0,2cd2,2cd4,2cd6,2cd8,2cda,2cdc,2cde,2ce0,2ce2,2ceb,2ced,2cf2,a640,a642,a644,a646,a648,a64a,a64c,a64e,a650,a652,a654,a656,a658,a65a,a65c,a65e,a660,a662,a664,a666,a668,a66a,a66c,a680,a682,a684,a686,a688,a68a,a68c,a68e,a690,a692,a694,a696,a698,a69a,a722,a724,a726,a728,a72a,a72c,a72e,a732,a734,a736,a738,a73a,a73c,a73e,a740,a742,a744,a746,a748,a74a,a74c,a74e,a750,a752,a754,a756,a758,a75a,a75c,a75e,a760,a762,a764,a766,a768,a76a,a76c,a76e,a779,a77b,a77d-a77e,a780,a782,a784,a786,a78b,a78d,a790,a792,a796,a798,a79a,a79c,a79e,a7a0,a7a2,a7a4,a7a6,a7a8,a7aa-a7ae,a7b0-a7b4,a7b6,a7b8,a7ba,a7bc,a7be,a7c0,a7c2,a7c4-a7c7,a7c9,a7cb-a7cc,a7ce,a7d0,a7d2,a7d4,a7d6,a7d8,a7da,a7dc,a7f5,ff21-ff3a,10400-10427,104b0-104d3,10570-1057a,1057c-1058a,1058c-10592,10594-10595,10c80-10cb2,10d50-10d65,118a0-118bf,16e40-16e5f,16ea0-16eb8,1d400-1d419,1d434-1d44d,1d468-1d481,1d49c,1d49e-1d49f,1d4a2,1d4a5-1d4a6,1d4a9-1d4ac,1d4ae-1d4b5,1d4d0-1d4e9,1d504-1d505,1d507-1d50a,1d50d-1d514,1d516-1d51c,1d538-1d539,1d53b-1d53e,1d540-1d544,1d546,1d54a-1d550,1d56c-1d585,1d5a0-1d5b9,1d5d4-1d5ed,1d608-1d621,1d63c-1d655,1d670-1d689,1d6a8-1d6c0,1d6e2-1d6fa,1d71c-1d734,1d756-1d76e,1d790-1d7a8,1d7ca,1e900-1e921,1f130-1f149,1f150-1f169,1f170-1f189",
  lower = "61-7a,aa,b5,ba,df-f6,f8-ff,101,103,105,107,109,10b,10d,10f,111,113,115,117,119,11b,11d,11f,121,123,125,127,129,12b,12d,12f,131,133,135,137-138,13a,13c,13e,140,142,144,146,148-149,14b,14d,14f,151,153,155,157,159,15b,15d,15f,161,163,165,167,169,16b,16d,16f,171,173,175,177,17a,17c,17e-180,183,185,188,18c-18d,192,195,199-19b,19e,1a1,1a3,1a5,1a8,1aa-1ab,1ad,1b0,1b4,1b6,1b9-1ba,1bd-1bf,1c6,1c9,1cc,1ce,1d0,1d2,1d4,1d6,1d8,1da,1dc-1dd,1df,1e1,1e3,1e5,1e7,1e9,1eb,1ed,1ef-1f0,1f3,1f5,1f9,1fb,1fd,1ff,201,203,205,207,209,20b,20d,20f,211,213,215,217,219,21b,21d,21f,221,223,225,227,229,22b,22d,22f,231,233-239,23c,23f-240,242,247,249,24b,24d,24f-293,296-2b8,2c0-2c1,2e0-2e4,345,371,373,377,37a-37d,390,3ac-3ce,3d0-3d1,3d5-3d7,3d9,3db,3dd,3df,3e1,3e3,3e5,3e7,3e9,3eb,3ed,3ef-3f3,3f5,3f8,3fb-3fc,430-45f,461,463,465,467,469,46b,46d,46f,471,473,475,477,479,47b,47d,47f,481,48b,48d,48f,491,493,495,497,499,49b,49d,49f,4a1,4a3,4a5,4a7,4a9,4ab,4ad,4af,4b1,4b3,4b5,4b7,4b9,4bb,4bd,4bf,4c2,4c4,4c6,4c8,4ca,4cc,4ce-4cf,4d1,4d3,4d5,4d7,4d9,4db,4dd,4df,4e1,4e3,4e5,4e7,4e9,4eb,4ed,4ef,4f1,4f3,4f5,4f7,4f9,4fb,4fd,4ff,501,503,505,507,509,50b,50d,50f,511,513,515,517,519,51b,51d,51f,521,523,525,527,529,52b,52d,52f,560-588,10d0-10fa,10fc-10ff,13f8-13fd,1c80-1c88,1c8a,1d00-1dbf,1e01,1e03,1e05,1e07,1e09,1e0b,1e0d,1e0f,1e11,1e13,1e15,1e17,1e19,1e1b,1e1d,1e1f,1e21,1e23,1e25,1e27,1e29,1e2b,1e2d,1e2f,1e31,1e33,1e35,1e37,1e39,1e3b,1e3d,1e3f,1e41,1e43,1e45,1e47,1e49,1e4b,1e4d,1e4f,1e51,1e53,1e55,1e57,1e59,1e5b,1e5d,1e5f,1e61,1e63,1e65,1e67,1e69,1e6b,1e6d,1e6f,1e71,1e73,1e75,1e77,1e79,1e7b,1e7d,1e7f,1e81,1e83,1e85,1e87,1e89,1e8b,1e8d,1e8f,1e91,1e93,1e95-1e9d,1e9f,1ea1,1ea3,1ea5,1ea7,1ea9,1eab,1ead,1eaf,1eb1,1eb3,1eb5,1eb7,1eb9,1ebb,1ebd,1ebf,1ec1,1ec3,1ec5,1ec7,1ec9,1ecb,1ecd,1ecf,1ed1,1ed3,1ed5,1ed7,1ed9,1edb,1edd,1edf,1ee1,1ee3,1ee5,1ee7,1ee9,1eeb,1eed,1eef,1ef1,1ef3,1ef5,1ef7,1ef9,1efb,1efd,1eff-1f07,1f10-1f15,1f20-1f27,1f30-1f37,1f40-1f45,1f50-1f57,1f60-1f67,1f70-1f7d,1f80-1f87,1f90-1f97,1fa0-1fa7,1fb0-1fb4,1fb6-1fb7,1fbe,1fc2-1fc4,1fc6-1fc7,1fd0-1fd3,1fd6-1fd7,1fe0-1fe7,1ff2-1ff4,1ff6-1ff7,2071,207f,2090-209c,210a,210e-210f,2113,212f,2134,2139,213c-213d,2146-2149,214e,2170-217f,2184,24d0-24e9,2c30-2c5f,2c61,2c65-2c66,2c68,2c6a,2c6c,2c71,2c73-2c74,2c76-2c7d,2c81,2c83,2c85,2c87,2c89,2c8b,2c8d,2c8f,2c91,2c93,2c95,2c97,2c99,2c9b,2c9d,2c9f,2ca1,2ca3,2ca5,2ca7,2ca9,2cab,2cad,2caf,2cb1,2cb3,2cb5,2cb7,2cb9,2cbb,2cbd,2cbf,2cc1,2cc3,2cc5,2cc7,2cc9,2ccb,2ccd,2ccf,2cd1,2cd3,2cd5,2cd7,2cd9,2cdb,2cdd,2cdf,2ce1,2ce3-2ce4,2cec,2cee,2cf3,2d00-2d25,2d27,2d2d,a641,a643,a645,a647,a649,a64b,a64d,a64f,a651,a653,a655,a657,a659,a65b,a65d,a65f,a661,a663,a665,a667,a669,a66b,a66d,a681,a683,a685,a687,a689,a68b,a68d,a68f,a691,a693,a695,a697,a699,a69b-a69d,a723,a725,a727,a729,a72b,a72d,a72f-a731,a733,a735,a737,a739,a73b,a73d,a73f,a741,a743,a745,a747,a749,a74b,a74d,a74f,a751,a753,a755,a757,a759,a75b,a75d,a75f,a761,a763,a765,a767,a769,a76b,a76d,a76f-a778,a77a,a77c,a77f,a781,a783,a785,a787,a78c,a78e,a791,a793-a795,a797,a799,a79b,a79d,a79f,a7a1,a7a3,a7a5,a7a7,a7a9,a7af,a7b5,a7b7,a7b9,a7bb,a7bd,a7bf,a7c1,a7c3,a7c8,a7ca,a7cd,a7cf,a7d1,a7d3,a7d5,a7d7,a7d9,a7db,a7f1-a7f4,a7f6,a7f8-a7fa,ab30-ab5a,ab5c-ab69,ab70-abbf,fb00-fb06,fb13-fb17,ff41-ff5a,10428-1044f,104d8-104fb,10597-105a1,105a3-105b1,105b3-105b9,105bb-105bc,10780,10783-10785,10787-107b0,107b2-107ba,10cc0-10cf2,10d70-10d85,118c0-118df,16e60-16e7f,16ebb-16ed3,1d41a-1d433,1d44e-1d454,1d456-1d467,1d482-1d49b,1d4b6-1d4b9,1d4bb,1d4bd-1d4c3,1d4c5-1d4cf,1d4ea-1d503,1d51e-1d537,1d552-1d56b,1d586-1d59f,1d5ba-1d5d3,1d5ee-1d607,1d622-1d63b,1d656-1d66f,1d68a-1d6a5,1d6c2-1d6da,1d6dc-1d6e1,1d6fc-1d714,1d716-1d71b,1d736-1d74e,1d750-1d755,1d770-1d788,1d78a-1d78f,1d7aa-1d7c2,1d7c4-1d7c9,1d7cb,1df00-1df09,1df0b-1df1e,1df25-1df2a,1e030-1e06d,1e922-1e943",
  to_upper = "b5:39c,df:53.53,e0:c0,e1:c1,e2:c2,e3:c3,e4:c4,e5:c5,e6:c6,e7:c7,e8:c8,e9:c9,ea:ca,eb:cb,ec:cc,ed:cd,ee:ce,ef:cf,f0:d0,f1:d1,f2:d2,f3:d3,f4:d4,f5:d5,f6:d6,f8:d8,f9:d9,fa:da,fb:db,fc:dc,fd:dd,fe:de,ff:178,101:100,103:102,105:104,107:106,109:108,10b:10a,10d:10c,10f:10e,111:110,113:112,115:114,117:116,119:118,11b:11a,11d:11c,11f:11e,121:120,123:122,125:124,127:126,129:128,12b:12a,12d:12c,12f:12e,131:49,133:132,135:134,137:136,13a:139,13c:13b,13e:13d,140:13f,142:141,144:143,146:145,148:147,149:2bc.4e,14b:14a,14d:14c,14f:14e,151:150,153:152,155:154,157:156,159:158,15b:15a,15d:15c,15f:15e,161:160,163:162,165:164,167:166,169:168,16b:16a,16d:16c,16f:16e,171:170,173:172,175:174,177:176,17a:179,17c:17b,17e:17d,17f:53,180:243,183:182,185:184,188:187,18c:18b,192:191,195:1f6,199:198,19a:23d,19b:a7dc,19e:220,1a1:1a0,1a3:1a2,1a5:1a4,1a8:1a7,1ad:1ac,1b0:1af,1b4:1b3,1b6:1b5,1b9:1b8,1bd:1bc,1bf:1f7,1c5:1c4,1c6:1c4,1c8:1c7,1c9:1c7,1cb:1ca,1cc:1ca,1ce:1cd,1d0:1cf,1d2:1d1,1d4:1d3,1d6:1d5,1d8:1d7,1da:1d9,1dc:1db,1dd:18e,1df:1de,1e1:1e0,1e3:1e2,1e5:1e4,1e7:1e6,1e9:1e8,1eb:1ea,1ed:1ec,1ef:1ee,1f0:4a.30c,1f2:1f1,1f3:1f1,1f5:1f4,1f9:1f8,1fb:1fa,1fd:1fc,1ff:1fe,201:200,203:202,205:204,207:206,209:208,20b:20a,20d:20c,20f:20e,211:210,213:212,215:214,217:216,219:218,21b:21a,21d:21c,21f:21e,223:222,225:224,227:226,229:228,22b:22a,22d:22c,22f:22e,231:230,233:232,23c:23b,23f:2c7e,240:2c7f,242:241,247:246,249:248,24b:24a,24d:24c,24f:24e,250:2c6f,251:2c6d,252:2c70,253:181,254:186,256:189,257:18a,259:18f,25b:190,25c:a7ab,260:193,261:a7ac,263:194,264:a7cb,265:a78d,266:a7aa,268:197,269:196,26a:a7ae,26b:2c62,26c:a7ad,26f:19c,271:2c6e,272:19d,275:19f,27d:2c64,280:1a6,282:a7c5,283:1a9,287:a7b1,288:1ae,289:244,28a:1b1,28b:1b2,28c:245,292:1b7,29d:a7b2,29e:a7b0,345:399,371:370,373:372,377:376,37b:3fd,37c:3fe,37d:3ff,390:399.308.301,3ac:386,3ad:388,3ae:389,3af:38a,3b0:3a5.308.301,3b1:391,3b2:392,3b3:393,3b4:394,3b5:395,3b6:396,3b7:397,3b8:398,3b9:399,3ba:39a,3bb:39b,3bc:39c,3bd:39d,3be:39e,3bf:39f,3c0:3a0,3c1:3a1,3c2:3a3,3c3:3a3,3c4:3a4,3c5:3a5,3c6:3a6,3c7:3a7,3c8:3a8,3c9:3a9,3ca:3aa,3cb:3ab,3cc:38c,3cd:38e,3ce:38f,3d0:392,3d1:398,3d5:3a6,3d6:3a0,3d7:3cf,3d9:3d8,3db:3da,3dd:3dc,3df:3de,3e1:3e0,3e3:3e2,3e5:3e4,3e7:3e6,3e9:3e8,3eb:3ea,3ed:3ec,3ef:3ee,3f0:39a,3f1:3a1,3f2:3f9,3f3:37f,3f5:395,3f8:3f7,3fb:3fa,430:410,431:411,432:412,433:413,434:414,435:415,436:416,437:417,438:418,439:419,43a:41a,43b:41b,43c:41c,43d:41d,43e:41e,43f:41f,440:420,441:421,442:422,443:423,444:424,445:425,446:426,447:427,448:428,449:429,44a:42a,44b:42b,44c:42c,44d:42d,44e:42e,44f:42f,450:400,451:401,452:402,453:403,454:404,455:405,456:406,457:407,458:408,459:409,45a:40a,45b:40b,45c:40c,45d:40d,45e:40e,45f:40f,461:460,463:462,465:464,467:466,469:468,46b:46a,46d:46c,46f:46e,471:470,473:472,475:474,477:476,479:478,47b:47a,47d:47c,47f:47e,481:480,48b:48a,48d:48c,48f:48e,491:490,493:492,495:494,497:496,499:498,49b:49a,49d:49c,49f:49e,4a1:4a0,4a3:4a2,4a5:4a4,4a7:4a6,4a9:4a8,4ab:4aa,4ad:4ac,4af:4ae,4b1:4b0,4b3:4b2,4b5:4b4,4b7:4b6,4b9:4b8,4bb:4ba,4bd:4bc,4bf:4be,4c2:4c1,4c4:4c3,4c6:4c5,4c8:4c7,4ca:4c9,4cc:4cb,4ce:4cd,4cf:4c0,4d1:4d0,4d3:4d2,4d5:4d4,4d7:4d6,4d9:4d8,4db:4da,4dd:4dc,4df:4de,4e1:4e0,4e3:4e2,4e5:4e4,4e7:4e6,4e9:4e8,4eb:4ea,4ed:4ec,4ef:4ee,4f1:4f0,4f3:4f2,4f5:4f4,4f7:4f6,4f9:4f8,4fb:4fa,4fd:4fc,4ff:4fe,501:500,503:502,505:504,507:506,509:508,50b:50a,50d:50c,50f:50e,511:510,513:512,515:514,517:516,519:518,51b:51a,51d:51c,51f:51e,521:520,523:522,525:524,527:526,529:528,52b:52a,52d:52c,52f:52e,561:531,562:532,563:533,564:534,565:535,566:536,567:537,568:538,569:539,56a:53a,56b:53b,56c:53c,56d:53d,56e:53e,56f:53f,570:540,571:541,572:542,573:543,574:544,575:545,576:546,577:547,578:548,579:549,57a:54a,57b:54b,57c:54c,57d:54d,57e:54e,57f:54f,580:550,581:551,582:552,583:553,584:554,585:555,586:556,587:535.552,10d0:1c90,10d1:1c91,10d2:1c92,10d3:1c93,10d4:1c94,10d5:1c95,10d6:1c96,10d7:1c97,10d8:1c98,10d9:1c99,10da:1c9a,10db:1c9b,10dc:1c9c,10dd:1c9d,10de:1c9e,10df:1c9f,10e0:1ca0,10e1:1ca1,10e2:1ca2,10e3:1ca3,10e4:1ca4,10e5:1ca5,10e6:1ca6,10e7:1ca7,10e8:1ca8,10e9:1ca9,10ea:1caa,10eb:1cab,10ec:1cac,10ed:1cad,10ee:1cae,10ef:1caf,10f0:1cb0,10f1:1cb1,10f2:1cb2,10f3:1cb3,10f4:1cb4,10f5:1cb5,10f6:1cb6,10f7:1cb7,10f8:1cb8,10f9:1cb9,10fa:1cba,10fd:1cbd,10fe:1cbe,10ff:1cbf,13f8:13f0,13f9:13f1,13fa:13f2,13fb:13f3,13fc:13f4,13fd:13f5,1c80:412,1c81:414,1c82:41e,1c83:421,1c84:422,1c85:422,1c86:42a,1c87:462,1c88:a64a,1c8a:1c89,1d79:a77d,1d7d:2c63,1d8e:a7c6,1e01:1e00,1e03:1e02,1e05:1e04,1e07:1e06,1e09:1e08,1e0b:1e0a,1e0d:1e0c,1e0f:1e0e,1e11:1e10,1e13:1e12,1e15:1e14,1e17:1e16,1e19:1e18,1e1b:1e1a,1e1d:1e1c,1e1f:1e1e,1e21:1e20,1e23:1e22,1e25:1e24,1e27:1e26,1e29:1e28,1e2b:1e2a,1e2d:1e2c,1e2f:1e2e,1e31:1e30,1e33:1e32,1e35:1e34,1e37:1e36,1e39:1e38,1e3b:1e3a,1e3d:1e3c,1e3f:1e3e,1e41:1e40,1e43:1e42,1e45:1e44,1e47:1e46,1e49:1e48,1e4b:1e4a,1e4d:1e4c,1e4f:1e4e,1e51:1e50,1e53:1e52,1e55:1e54,1e57:1e56,1e59:1e58,1e5b:1e5a,1e5d:1e5c,1e5f:1e5e,1e61:1e60,1e63:1e62,1e65:1e64,1e67:1e66,1e69:1e68,1e6b:1e6a,1e6d:1e6c,1e6f:1e6e,1e71:1e70,1e73:1e72,1e75:1e74,1e77:1e76,1e79:1e78,1e7b:1e7a,1e7d:1e7c,1e7f:1e7e,1e81:1e80,1e83:1e82,1e85:1e84,1e87:1e86,1e89:1e88,1e8b:1e8a,1e8d:1e8c,1e8f:1e8e,1e91:1e90,1e93:1e92,1e95:1e94,1e96:48.331,1e97:54.308,1e98:57.30a,1e99:59.30a,1e9a:41.2be,1e9b:1e60,1ea1:1ea0,1ea3:1ea2,1ea5:1ea4,1ea7:1ea6,1ea9:1ea8,1eab:1eaa,1ead:1eac,1eaf:1eae,1eb1:1eb0,1eb3:1eb2,1eb5:1eb4,1eb7:1eb6,1eb9:1eb8,1ebb:1eba,1ebd:1ebc,1ebf:1ebe,1ec1:1ec0,1ec3:1ec2,1ec5:1ec4,1ec7:1ec6,1ec9:1ec8,1ecb:1eca,1ecd:1ecc,1ecf:1ece,1ed1:1ed0,1ed3:1ed2,1ed5:1ed4,1ed7:1ed6,1ed9:1ed8,1edb:1eda,1edd:1edc,1edf:1ede,1ee1:1ee0,1ee3:1ee2,1ee5:1ee4,1ee7:1ee6,1ee9:1ee8,1eeb:1eea,1eed:1eec,1eef:1eee,1ef1:1ef0,1ef3:1ef2,1ef5:1ef4,1ef7:1ef6,1ef9:1ef8,1efb:1efa,1efd:1efc,1eff:1efe,1f00:1f08,1f01:1f09,1f02:1f0a,1f03:1f0b,1f04:1f0c,1f05:1f0d,1f06:1f0e,1f07:1f0f,1f10:1f18,1f11:1f19,1f12:1f1a,1f13:1f1b,1f14:1f1c,1f15:1f1d,1f20:1f28,1f21:1f29,1f22:1f2a,1f23:1f2b,1f24:1f2c,1f25:1f2d,1f26:1f2e,1f27:1f2f,1f30:1f38,1f31:1f39,1f32:1f3a,1f33:1f3b,1f34:1f3c,1f35:1f3d,1f36:1f3e,1f37:1f3f,1f40:1f48,1f41:1f49,1f42:1f4a,1f43:1f4b,1f44:1f4c,1f45:1f4d,1f50:3a5.313,1f51:1f59,1f52:3a5.313.300,1f53:1f5b,1f54:3a5.313.301,1f55:1f5d,1f56:3a5.313.342,1f57:1f5f,1f60:1f68,1f61:1f69,1f62:1f6a,1f63:1f6b,1f64:1f6c,1f65:1f6d,1f66:1f6e,1f67:1f6f,1f70:1fba,1f71:1fbb,1f72:1fc8,1f73:1fc9,1f74:1fca,1f75:1fcb,1f76:1fda,1f77:1fdb,1f78:1ff8,1f79:1ff9,1f7a:1fea,1f7b:1feb,1f7c:1ffa,1f7d:1ffb,1f80:1f08.399,1f81:1f09.399,1f82:1f0a.399,1f83:1f0b.399,1f84:1f0c.399,1f85:1f0d.399,1f86:1f0e.399,1f87:1f0f.399,1f88:1f08.399,1f89:1f09.399,1f8a:1f0a.399,1f8b:1f0b.399,1f8c:1f0c.399,1f8d:1f0d.399,1f8e:1f0e.399,1f8f:1f0f.399,1f90:1f28.399,1f91:1f29.399,1f92:1f2a.399,1f93:1f2b.399,1f94:1f2c.399,1f95:1f2d.399,1f96:1f2e.399,1f97:1f2f.399,1f98:1f28.399,1f99:1f29.399,1f9a:1f2a.399,1f9b:1f2b.399,1f9c:1f2c.399,1f9d:1f2d.399,1f9e:1f2e.399,1f9f:1f2f.399,1fa0:1f68.399,1fa1:1f69.399,1fa2:1f6a.399,1fa3:1f6b.399,1fa4:1f6c.399,1fa5:1f6d.399,1fa6:1f6e.399,1fa7:1f6f.399,1fa8:1f68.399,1fa9:1f69.399,1faa:1f6a.399,1fab:1f6b.399,1fac:1f6c.399,1fad:1f6d.399,1fae:1f6e.399,1faf:1f6f.399,1fb0:1fb8,1fb1:1fb9,1fb2:1fba.399,1fb3:391.399,1fb4:386.399,1fb6:391.342,1fb7:391.342.399,1fbc:391.399,1fbe:399,1fc2:1fca.399,1fc3:397.399,1fc4:389.399,1fc6:397.342,1fc7:397.342.399,1fcc:397.399,1fd0:1fd8,1fd1:1fd9,1fd2:399.308.300,1fd3:399.308.301,1fd6:399.342,1fd7:399.308.342,1fe0:1fe8,1fe1:1fe9,1fe2:3a5.308.300,1fe3:3a5.308.301,1fe4:3a1.313,1fe5:1fec,1fe6:3a5.342,1fe7:3a5.308.342,1ff2:1ffa.399,1ff3:3a9.399,1ff4:38f.399,1ff6:3a9.342,1ff7:3a9.342.399,1ffc:3a9.399,214e:2132,2170:2160,2171:2161,2172:2162,2173:2163,2174:2164,2175:2165,2176:2166,2177:2167,2178:2168,2179:2169,217a:216a,217b:216b,217c:216c,217d:216d,217e:216e,217f:216f,2184:2183,24d0:24b6,24d1:24b7,24d2:24b8,24d3:24b9,24d4:24ba,24d5:24bb,24d6:24bc,24d7:24bd,24d8:24be,24d9:24bf,24da:24c0,24db:24c1,24dc:24c2,24dd:24c3,24de:24c4,24df:24c5,24e0:24c6,24e1:24c7,24e2:24c8,24e3:24c9,24e4:24ca,24e5:24cb,24e6:24cc,24e7:24cd,24e8:24ce,24e9:24cf,2c30:2c00,2c31:2c01,2c32:2c02,2c33:2c03,2c34:2c04,2c35:2c05,2c36:2c06,2c37:2c07,2c38:2c08,2c39:2c09,2c3a:2c0a,2c3b:2c0b,2c3c:2c0c,2c3d:2c0d,2c3e:2c0e,2c3f:2c0f,2c40:2c10,2c41:2c11,2c42:2c12,2c43:2c13,2c44:2c14,2c45:2c15,2c46:2c16,2c47:2c17,2c48:2c18,2c49:2c19,2c4a:2c1a,2c4b:2c1b,2c4c:2c1c,2c4d:2c1d,2c4e:2c1e,2c4f:2c1f,2c50:2c20,2c51:2c21,2c52:2c22,2c53:2c23,2c54:2c24,2c55:2c25,2c56:2c26,2c57:2c27,2c58:2c28,2c59:2c29,2c5a:2c2a,2c5b:2c2b,2c5c:2c2c,2c5d:2c2d,2c5e:2c2e,2c5f:2c2f,2c61:2c60,2c65:23a,2c66:23e,2c68:2c67,2c6a:2c69,2c6c:2c6b,2c73:2c72,2c76:2c75,2c81:2c80,2c83:2c82,2c85:2c84,2c87:2c86,2c89:2c88,2c8b:2c8a,2c8d:2c8c,2c8f:2c8e,2c91:2c90,2c93:2c92,2c95:2c94,2c97:2c96,2c99:2c98,2c9b:2c9a,2c9d:2c9c,2c9f:2c9e,2ca1:2ca0,2ca3:2ca2,2ca5:2ca4,2ca7:2ca6,2ca9:2ca8,2cab:2caa,2cad:2cac,2caf:2cae,2cb1:2cb0,2cb3:2cb2,2cb5:2cb4,2cb7:2cb6,2cb9:2cb8,2cbb:2cba,2cbd:2cbc,2cbf:2cbe,2cc1:2cc0,2cc3:2cc2,2cc5:2cc4,2cc7:2cc6,2cc9:2cc8,2ccb:2cca,2ccd:2ccc,2ccf:2cce,2cd1:2cd0,2cd3:2cd2,2cd5:2cd4,2cd7:2cd6,2cd9:2cd8,2cdb:2cda,2cdd:2cdc,2cdf:2cde,2ce1:2ce0,2ce3:2ce2,2cec:2ceb,2cee:2ced,2cf3:2cf2,2d00:10a0,2d01:10a1,2d02:10a2,2d03:10a3,2d04:10a4,2d05:10a5,2d06:10a6,2d07:10a7,2d08:10a8,2d09:10a9,2d0a:10aa,2d0b:10ab,2d0c:10ac,2d0d:10ad,2d0e:10ae,2d0f:10af,2d10:10b0,2d11:10b1,2d12:10b2,2d13:10b3,2d14:10b4,2d15:10b5,2d16:10b6,2d17:10b7,2d18:10b8,2d19:10b9,2d1a:10ba,2d1b:10bb,2d1c:10bc,2d1d:10bd,2d1e:10be,2d1f:10bf,2d20:10c0,2d21:10c1,2d22:10c2,2d23:10c3,2d24:10c4,2d25:10c5,2d27:10c7,2d2d:10cd,a641:a640,a643:a642,a645:a644,a647:a646,a649:a648,a64b:a64a,a64d:a64c,a64f:a64e,a651:a650,a653:a652,a655:a654,a657:a656,a659:a658,a65b:a65a,a65d:a65c,a65f:a65e,a661:a660,a663:a662,a665:a664,a667:a666,a669:a668,a66b:a66a,a66d:a66c,a681:a680,a683:a682,a685:a684,a687:a686,a689:a688,a68b:a68a,a68d:a68c,a68f:a68e,a691:a690,a693:a692,a695:a694,a697:a696,a699:a698,a69b:a69a,a723:a722,a725:a724,a727:a726,a729:a728,a72b:a72a,a72d:a72c,a72f:a72e,a733:a732,a735:a734,a737:a736,a739:a738,a73b:a73a,a73d:a73c,a73f:a73e,a741:a740,a743:a742,a745:a744,a747:a746,a749:a748,a74b:a74a,a74d:a74c,a74f:a74e,a751:a750,a753:a752,a755:a754,a757:a756,a759:a758,a75b:a75a,a75d:a75c,a75f:a75e,a761:a760,a763:a762,a765:a764,a767:a766,a769:a768,a76b:a76a,a76d:a76c,a76f:a76e,a77a:a779,a77c:a77b,a77f:a77e,a781:a780,a783:a782,a785:a784,a787:a786,a78c:a78b,a791:a790,a793:a792,a794:a7c4,a797:a796,a799:a798,a79b:a79a,a79d:a79c,a79f:a79e,a7a1:a7a0,a7a3:a7a2,a7a5:a7a4,a7a7:a7a6,a7a9:a7a8,a7b5:a7b4,a7b7:a7b6,a7b9:a7b8,a7bb:a7ba,a7bd:a7bc,a7bf:a7be,a7c1:a7c0,a7c3:a7c2,a7c8:a7c7,a7ca:a7c9,a7cd:a7cc,a7cf:a7ce,a7d1:a7d0,a7d3:a7d2,a7d5:a7d4,a7d7:a7d6,a7d9:a7d8,a7db:a7da,a7f6:a7f5,ab53:a7b3,ab70:13a0,ab71:13a1,ab72:13a2,ab73:13a3,ab74:13a4,ab75:13a5,ab76:13a6,ab77:13a7,ab78:13a8,ab79:13a9,ab7a:13aa,ab7b:13ab,ab7c:13ac,ab7d:13ad,ab7e:13ae,ab7f:13af,ab80:13b0,ab81:13b1,ab82:13b2,ab83:13b3,ab84:13b4,ab85:13b5,ab86:13b6,ab87:13b7,ab88:13b8,ab89:13b9,ab8a:13ba,ab8b:13bb,ab8c:13bc,ab8d:13bd,ab8e:13be,ab8f:13bf,ab90:13c0,ab91:13c1,ab92:13c2,ab93:13c3,ab94:13c4,ab95:13c5,ab96:13c6,ab97:13c7,ab98:13c8,ab99:13c9,ab9a:13ca,ab9b:13cb,ab9c:13cc,ab9d:13cd,ab9e:13ce,ab9f:13cf,aba0:13d0,aba1:13d1,aba2:13d2,aba3:13d3,aba4:13d4,aba5:13d5,aba6:13d6,aba7:13d7,aba8:13d8,aba9:13d9,abaa:13da,abab:13db,abac:13dc,abad:13dd,abae:13de,abaf:13df,abb0:13e0,abb1:13e1,abb2:13e2,abb3:13e3,abb4:13e4,abb5:13e5,abb6:13e6,abb7:13e7,abb8:13e8,abb9:13e9,abba:13ea,abbb:13eb,abbc:13ec,abbd:13ed,abbe:13ee,abbf:13ef,fb00:46.46,fb01:46.49,fb02:46.4c,fb03:46.46.49,fb04:46.46.4c,fb05:53.54,fb06:53.54,fb13:544.546,fb14:544.535,fb15:544.53b,fb16:54e.546,fb17:544.53d,ff41:ff21,ff42:ff22,ff43:ff23,ff44:ff24,ff45:ff25,ff46:ff26,ff47:ff27,ff48:ff28,ff49:ff29,ff4a:ff2a,ff4b:ff2b,ff4c:ff2c,ff4d:ff2d,ff4e:ff2e,ff4f:ff2f,ff50:ff30,ff51:ff31,ff52:ff32,ff53:ff33,ff54:ff34,ff55:ff35,ff56:ff36,ff57:ff37,ff58:ff38,ff59:ff39,ff5a:ff3a,10428:10400,10429:10401,1042a:10402,1042b:10403,1042c:10404,1042d:10405,1042e:10406,1042f:10407,10430:10408,10431:10409,10432:1040a,10433:1040b,10434:1040c,10435:1040d,10436:1040e,10437:1040f,10438:10410,10439:10411,1043a:10412,1043b:10413,1043c:10414,1043d:10415,1043e:10416,1043f:10417,10440:10418,10441:10419,10442:1041a,10443:1041b,10444:1041c,10445:1041d,10446:1041e,10447:1041f,10448:10420,10449:10421,1044a:10422,1044b:10423,1044c:10424,1044d:10425,1044e:10426,1044f:10427,104d8:104b0,104d9:104b1,104da:104b2,104db:104b3,104dc:104b4,104dd:104b5,104de:104b6,104df:104b7,104e0:104b8,104e1:104b9,104e2:104ba,104e3:104bb,104e4:104bc,104e5:104bd,104e6:104be,104e7:104bf,104e8:104c0,104e9:104c1,104ea:104c2,104eb:104c3,104ec:104c4,104ed:104c5,104ee:104c6,104ef:104c7,104f0:104c8,104f1:104c9,104f2:104ca,104f3:104cb,104f4:104cc,104f5:104cd,104f6:104ce,104f7:104cf,104f8:104d0,104f9:104d1,104fa:104d2,104fb:104d3,10597:10570,10598:10571,10599:10572,1059a:10573,1059b:10574,1059c:10575,1059d:10576,1059e:10577,1059f:10578,105a0:10579,105a1:1057a,105a3:1057c,105a4:1057d,105a5:1057e,105a6:1057f,105a7:10580,105a8:10581,105a9:10582,105aa:10583,105ab:10584,105ac:10585,105ad:10586,105ae:10587,105af:10588,105b0:10589,105b1:1058a,105b3:1058c,105b4:1058d,105b5:1058e,105b6:1058f,105b7:10590,105b8:10591,105b9:10592,105bb:10594,105bc:10595,10cc0:10c80,10cc1:10c81,10cc2:10c82,10cc3:10c83,10cc4:10c84,10cc5:10c85,10cc6:10c86,10cc7:10c87,10cc8:10c88,10cc9:10c89,10cca:10c8a,10ccb:10c8b,10ccc:10c8c,10ccd:10c8d,10cce:10c8e,10ccf:10c8f,10cd0:10c90,10cd1:10c91,10cd2:10c92,10cd3:10c93,10cd4:10c94,10cd5:10c95,10cd6:10c96,10cd7:10c97,10cd8:10c98,10cd9:10c99,10cda:10c9a,10cdb:10c9b,10cdc:10c9c,10cdd:10c9d,10cde:10c9e,10cdf:10c9f,10ce0:10ca0,10ce1:10ca1,10ce2:10ca2,10ce3:10ca3,10ce4:10ca4,10ce5:10ca5,10ce6:10ca6,10ce7:10ca7,10ce8:10ca8,10ce9:10ca9,10cea:10caa,10ceb:10cab,10cec:10cac,10ced:10cad,10cee:10cae,10cef:10caf,10cf0:10cb0,10cf1:10cb1,10cf2:10cb2,10d70:10d50,10d71:10d51,10d72:10d52,10d73:10d53,10d74:10d54,10d75:10d55,10d76:10d56,10d77:10d57,10d78:10d58,10d79:10d59,10d7a:10d5a,10d7b:10d5b,10d7c:10d5c,10d7d:10d5d,10d7e:10d5e,10d7f:10d5f,10d80:10d60,10d81:10d61,10d82:10d62,10d83:10d63,10d84:10d64,10d85:10d65,118c0:118a0,118c1:118a1,118c2:118a2,118c3:118a3,118c4:118a4,118c5:118a5,118c6:118a6,118c7:118a7,118c8:118a8,118c9:118a9,118ca:118aa,118cb:118ab,118cc:118ac,118cd:118ad,118ce:118ae,118cf:118af,118d0:118b0,118d1:118b1,118d2:118b2,118d3:118b3,118d4:118b4,118d5:118b5,118d6:118b6,118d7:118b7,118d8:118b8,118d9:118b9,118da:118ba,118db:118bb,118dc:118bc,118dd:118bd,118de:118be,118df:118bf,16e60:16e40,16e61:16e41,16e62:16e42,16e63:16e43,16e64:16e44,16e65:16e45,16e66:16e46,16e67:16e47,16e68:16e48,16e69:16e49,16e6a:16e4a,16e6b:16e4b,16e6c:16e4c,16e6d:16e4d,16e6e:16e4e,16e6f:16e4f,16e70:16e50,16e71:16e51,16e72:16e52,16e73:16e53,16e74:16e54,16e75:16e55,16e76:16e56,16e77:16e57,16e78:16e58,16e79:16e59,16e7a:16e5a,16e7b:16e5b,16e7c:16e5c,16e7d:16e5d,16e7e:16e5e,16e7f:16e5f,16ebb:16ea0,16ebc:16ea1,16ebd:16ea2,16ebe:16ea3,16ebf:16ea4,16ec0:16ea5,16ec1:16ea6,16ec2:16ea7,16ec3:16ea8,16ec4:16ea9,16ec5:16eaa,16ec6:16eab,16ec7:16eac,16ec8:16ead,16ec9:16eae,16eca:16eaf,16ecb:16eb0,16ecc:16eb1,16ecd:16eb2,16ece:16eb3,16ecf:16eb4,16ed0:16eb5,16ed1:16eb6,16ed2:16eb7,16ed3:16eb8,1e922:1e900,1e923:1e901,1e924:1e902,1e925:1e903,1e926:1e904,1e927:1e905,1e928:1e906,1e929:1e907,1e92a:1e908,1e92b:1e909,1e92c:1e90a,1e92d:1e90b,1e92e:1e90c,1e92f:1e90d,1e930:1e90e,1e931:1e90f,1e932:1e910,1e933:1e911,1e934:1e912,1e935:1e913,1e936:1e914,1e937:1e915,1e938:1e916,1e939:1e917,1e93a:1e918,1e93b:1e919,1e93c:1e91a,1e93d:1e91b,1e93e:1e91c,1e93f:1e91d,1e940:1e91e,1e941:1e91f,1e942:1e920,1e943:1e921",
  to_lower = "c0:e0,c1:e1,c2:e2,c3:e3,c4:e4,c5:e5,c6:e6,c7:e7,c8:e8,c9:e9,ca:ea,cb:eb,cc:ec,cd:ed,ce:ee,cf:ef,d0:f0,d1:f1,d2:f2,d3:f3,d4:f4,d5:f5,d6:f6,d8:f8,d9:f9,da:fa,db:fb,dc:fc,dd:fd,de:fe,100:101,102:103,104:105,106:107,108:109,10a:10b,10c:10d,10e:10f,110:111,112:113,114:115,116:117,118:119,11a:11b,11c:11d,11e:11f,120:121,122:123,124:125,126:127,128:129,12a:12b,12c:12d,12e:12f,130:69.307,132:133,134:135,136:137,139:13a,13b:13c,13d:13e,13f:140,141:142,143:144,145:146,147:148,14a:14b,14c:14d,14e:14f,150:151,152:153,154:155,156:157,158:159,15a:15b,15c:15d,15e:15f,160:161,162:163,164:165,166:167,168:169,16a:16b,16c:16d,16e:16f,170:171,172:173,174:175,176:177,178:ff,179:17a,17b:17c,17d:17e,181:253,182:183,184:185,186:254,187:188,189:256,18a:257,18b:18c,18e:1dd,18f:259,190:25b,191:192,193:260,194:263,196:269,197:268,198:199,19c:26f,19d:272,19f:275,1a0:1a1,1a2:1a3,1a4:1a5,1a6:280,1a7:1a8,1a9:283,1ac:1ad,1ae:288,1af:1b0,1b1:28a,1b2:28b,1b3:1b4,1b5:1b6,1b7:292,1b8:1b9,1bc:1bd,1c4:1c6,1c5:1c6,1c7:1c9,1c8:1c9,1ca:1cc,1cb:1cc,1cd:1ce,1cf:1d0,1d1:1d2,1d3:1d4,1d5:1d6,1d7:1d8,1d9:1da,1db:1dc,1de:1df,1e0:1e1,1e2:1e3,1e4:1e5,1e6:1e7,1e8:1e9,1ea:1eb,1ec:1ed,1ee:1ef,1f1:1f3,1f2:1f3,1f4:1f5,1f6:195,1f7:1bf,1f8:1f9,1fa:1fb,1fc:1fd,1fe:1ff,200:201,202:203,204:205,206:207,208:209,20a:20b,20c:20d,20e:20f,210:211,212:213,214:215,216:217,218:219,21a:21b,21c:21d,21e:21f,220:19e,222:223,224:225,226:227,228:229,22a:22b,22c:22d,22e:22f,230:231,232:233,23a:2c65,23b:23c,23d:19a,23e:2c66,241:242,243:180,244:289,245:28c,246:247,248:249,24a:24b,24c:24d,24e:24f,370:371,372:373,376:377,37f:3f3,386:3ac,388:3ad,389:3ae,38a:3af,38c:3cc,38e:3cd,38f:3ce,391:3b1,392:3b2,393:3b3,394:3b4,395:3b5,396:3b6,397:3b7,398:3b8,399:3b9,39a:3ba,39b:3bb,39c:3bc,39d:3bd,39e:3be,39f:3bf,3a0:3c0,3a1:3c1,3a3:3c3,3a4:3c4,3a5:3c5,3a6:3c6,3a7:3c7,3a8:3c8,3a9:3c9,3aa:3ca,3ab:3cb,3cf:3d7,3d8:3d9,3da:3db,3dc:3dd,3de:3df,3e0:3e1,3e2:3e3,3e4:3e5,3e6:3e7,3e8:3e9,3ea:3eb,3ec:3ed,3ee:3ef,3f4:3b8,3f7:3f8,3f9:3f2,3fa:3fb,3fd:37b,3fe:37c,3ff:37d,400:450,401:451,402:452,403:453,404:454,405:455,406:456,407:457,408:458,409:459,40a:45a,40b:45b,40c:45c,40d:45d,40e:45e,40f:45f,410:430,411:431,412:432,413:433,414:434,415:435,416:436,417:437,418:438,419:439,41a:43a,41b:43b,41c:43c,41d:43d,41e:43e,41f:43f,420:440,421:441,422:442,423:443,424:444,425:445,426:446,427:447,428:448,429:449,42a:44a,42b:44b,42c:44c,42d:44d,42e:44e,42f:44f,460:461,462:463,464:465,466:467,468:469,46a:46b,46c:46d,46e:46f,470:471,472:473,474:475,476:477,478:479,47a:47b,47c:47d,47e:47f,480:481,48a:48b,48c:48d,48e:48f,490:491,492:493,494:495,496:497,498:499,49a:49b,49c:49d,49e:49f,4a0:4a1,4a2:4a3,4a4:4a5,4a6:4a7,4a8:4a9,4aa:4ab,4ac:4ad,4ae:4af,4b0:4b1,4b2:4b3,4b4:4b5,4b6:4b7,4b8:4b9,4ba:4bb,4bc:4bd,4be:4bf,4c0:4cf,4c1:4c2,4c3:4c4,4c5:4c6,4c7:4c8,4c9:4ca,4cb:4cc,4cd:4ce,4d0:4d1,4d2:4d3,4d4:4d5,4d6:4d7,4d8:4d9,4da:4db,4dc:4dd,4de:4df,4e0:4e1,4e2:4e3,4e4:4e5,4e6:4e7,4e8:4e9,4ea:4eb,4ec:4ed,4ee:4ef,4f0:4f1,4f2:4f3,4f4:4f5,4f6:4f7,4f8:4f9,4fa:4fb,4fc:4fd,4fe:4ff,500:501,502:503,504:505,506:507,508:509,50a:50b,50c:50d,50e:50f,510:511,512:513,514:515,516:517,518:519,51a:51b,51c:51d,51e:51f,520:521,522:523,524:525,526:527,528:529,52a:52b,52c:52d,52e:52f,531:561,532:562,533:563,534:564,535:565,536:566,537:567,538:568,539:569,53a:56a,53b:56b,53c:56c,53d:56d,53e:56e,53f:56f,540:570,541:571,542:572,543:573,544:574,545:575,546:576,547:577,548:578,549:579,54a:57a,54b:57b,54c:57c,54d:57d,54e:57e,54f:57f,550:580,551:581,552:582,553:583,554:584,555:585,556:586,10a0:2d00,10a1:2d01,10a2:2d02,10a3:2d03,10a4:2d04,10a5:2d05,10a6:2d06,10a7:2d07,10a8:2d08,10a9:2d09,10aa:2d0a,10ab:2d0b,10ac:2d0c,10ad:2d0d,10ae:2d0e,10af:2d0f,10b0:2d10,10b1:2d11,10b2:2d12,10b3:2d13,10b4:2d14,10b5:2d15,10b6:2d16,10b7:2d17,10b8:2d18,10b9:2d19,10ba:2d1a,10bb:2d1b,10bc:2d1c,10bd:2d1d,10be:2d1e,10bf:2d1f,10c0:2d20,10c1:2d21,10c2:2d22,10c3:2d23,10c4:2d24,10c5:2d25,10c7:2d27,10cd:2d2d,13a0:ab70,13a1:ab71,13a2:ab72,13a3:ab73,13a4:ab74,13a5:ab75,13a6:ab76,13a7:ab77,13a8:ab78,13a9:ab79,13aa:ab7a,13ab:ab7b,13ac:ab7c,13ad:ab7d,13ae:ab7e,13af:ab7f,13b0:ab80,13b1:ab81,13b2:ab82,13b3:ab83,13b4:ab84,13b5:ab85,13b6:ab86,13b7:ab87,13b8:ab88,13b9:ab89,13ba:ab8a,13bb:ab8b,13bc:ab8c,13bd:ab8d,13be:ab8e,13bf:ab8f,13c0:ab90,13c1:ab91,13c2:ab92,13c3:ab93,13c4:ab94,13c5:ab95,13c6:ab96,13c7:ab97,13c8:ab98,13c9:ab99,13ca:ab9a,13cb:ab9b,13cc:ab9c,13cd:ab9d,13ce:ab9e,13cf:ab9f,13d0:aba0,13d1:aba1,13d2:aba2,13d3:aba3,13d4:aba4,13d5:aba5,13d6:aba6,13d7:aba7,13d8:aba8,13d9:aba9,13da:abaa,13db:abab,13dc:abac,13dd:abad,13de:abae,13df:abaf,13e0:abb0,13e1:abb1,13e2:abb2,13e3:abb3,13e4:abb4,13e5:abb5,13e6:abb6,13e7:abb7,13e8:abb8,13e9:abb9,13ea:abba,13eb:abbb,13ec:abbc,13ed:abbd,13ee:abbe,13ef:abbf,13f0:13f8,13f1:13f9,13f2:13fa,13f3:13fb,13f4:13fc,13f5:13fd,1c89:1c8a,1c90:10d0,1c91:10d1,1c92:10d2,1c93:10d3,1c94:10d4,1c95:10d5,1c96:10d6,1c97:10d7,1c98:10d8,1c99:10d9,1c9a:10da,1c9b:10db,1c9c:10dc,1c9d:10dd,1c9e:10de,1c9f:10df,1ca0:10e0,1ca1:10e1,1ca2:10e2,1ca3:10e3,1ca4:10e4,1ca5:10e5,1ca6:10e6,1ca7:10e7,1ca8:10e8,1ca9:10e9,1caa:10ea,1cab:10eb,1cac:10ec,1cad:10ed,1cae:10ee,1caf:10ef,1cb0:10f0,1cb1:10f1,1cb2:10f2,1cb3:10f3,1cb4:10f4,1cb5:10f5,1cb6:10f6,1cb7:10f7,1cb8:10f8,1cb9:10f9,1cba:10fa,1cbd:10fd,1cbe:10fe,1cbf:10ff,1e00:1e01,1e02:1e03,1e04:1e05,1e06:1e07,1e08:1e09,1e0a:1e0b,1e0c:1e0d,1e0e:1e0f,1e10:1e11,1e12:1e13,1e14:1e15,1e16:1e17,1e18:1e19,1e1a:1e1b,1e1c:1e1d,1e1e:1e1f,1e20:1e21,1e22:1e23,1e24:1e25,1e26:1e27,1e28:1e29,1e2a:1e2b,1e2c:1e2d,1e2e:1e2f,1e30:1e31,1e32:1e33,1e34:1e35,1e36:1e37,1e38:1e39,1e3a:1e3b,1e3c:1e3d,1e3e:1e3f,1e40:1e41,1e42:1e43,1e44:1e45,1e46:1e47,1e48:1e49,1e4a:1e4b,1e4c:1e4d,1e4e:1e4f,1e50:1e51,1e52:1e53,1e54:1e55,1e56:1e57,1e58:1e59,1e5a:1e5b,1e5c:1e5d,1e5e:1e5f,1e60:1e61,1e62:1e63,1e64:1e65,1e66:1e67,1e68:1e69,1e6a:1e6b,1e6c:1e6d,1e6e:1e6f,1e70:1e71,1e72:1e73,1e74:1e75,1e76:1e77,1e78:1e79,1e7a:1e7b,1e7c:1e7d,1e7e:1e7f,1e80:1e81,1e82:1e83,1e84:1e85,1e86:1e87,1e88:1e89,1e8a:1e8b,1e8c:1e8d,1e8e:1e8f,1e90:1e91,1e92:1e93,1e94:1e95,1e9e:df,1ea0:1ea1,1ea2:1ea3,1ea4:1ea5,1ea6:1ea7,1ea8:1ea9,1eaa:1eab,1eac:1ead,1eae:1eaf,1eb0:1eb1,1eb2:1eb3,1eb4:1eb5,1eb6:1eb7,1eb8:1eb9,1eba:1ebb,1ebc:1ebd,1ebe:1ebf,1ec0:1ec1,1ec2:1ec3,1ec4:1ec5,1ec6:1ec7,1ec8:1ec9,1eca:1ecb,1ecc:1ecd,1ece:1ecf,1ed0:1ed1,1ed2:1ed3,1ed4:1ed5,1ed6:1ed7,1ed8:1ed9,1eda:1edb,1edc:1edd,1ede:1edf,1ee0:1ee1,1ee2:1ee3,1ee4:1ee5,1ee6:1ee7,1ee8:1ee9,1eea:1eeb,1eec:1eed,1eee:1eef,1ef0:1ef1,1ef2:1ef3,1ef4:1ef5,1ef6:1ef7,1ef8:1ef9,1efa:1efb,1efc:1efd,1efe:1eff,1f08:1f00,1f09:1f01,1f0a:1f02,1f0b:1f03,1f0c:1f04,1f0d:1f05,1f0e:1f06,1f0f:1f07,1f18:1f10,1f19:1f11,1f1a:1f12,1f1b:1f13,1f1c:1f14,1f1d:1f15,1f28:1f20,1f29:1f21,1f2a:1f22,1f2b:1f23,1f2c:1f24,1f2d:1f25,1f2e:1f26,1f2f:1f27,1f38:1f30,1f39:1f31,1f3a:1f32,1f3b:1f33,1f3c:1f34,1f3d:1f35,1f3e:1f36,1f3f:1f37,1f48:1f40,1f49:1f41,1f4a:1f42,1f4b:1f43,1f4c:1f44,1f4d:1f45,1f59:1f51,1f5b:1f53,1f5d:1f55,1f5f:1f57,1f68:1f60,1f69:1f61,1f6a:1f62,1f6b:1f63,1f6c:1f64,1f6d:1f65,1f6e:1f66,1f6f:1f67,1f88:1f80,1f89:1f81,1f8a:1f82,1f8b:1f83,1f8c:1f84,1f8d:1f85,1f8e:1f86,1f8f:1f87,1f98:1f90,1f99:1f91,1f9a:1f92,1f9b:1f93,1f9c:1f94,1f9d:1f95,1f9e:1f96,1f9f:1f97,1fa8:1fa0,1fa9:1fa1,1faa:1fa2,1fab:1fa3,1fac:1fa4,1fad:1fa5,1fae:1fa6,1faf:1fa7,1fb8:1fb0,1fb9:1fb1,1fba:1f70,1fbb:1f71,1fbc:1fb3,1fc8:1f72,1fc9:1f73,1fca:1f74,1fcb:1f75,1fcc:1fc3,1fd8:1fd0,1fd9:1fd1,1fda:1f76,1fdb:1f77,1fe8:1fe0,1fe9:1fe1,1fea:1f7a,1feb:1f7b,1fec:1fe5,1ff8:1f78,1ff9:1f79,1ffa:1f7c,1ffb:1f7d,1ffc:1ff3,2126:3c9,212a:6b,212b:e5,2132:214e,2160:2170,2161:2171,2162:2172,2163:2173,2164:2174,2165:2175,2166:2176,2167:2177,2168:2178,2169:2179,216a:217a,216b:217b,216c:217c,216d:217d,216e:217e,216f:217f,2183:2184,24b6:24d0,24b7:24d1,24b8:24d2,24b9:24d3,24ba:24d4,24bb:24d5,24bc:24d6,24bd:24d7,24be:24d8,24bf:24d9,24c0:24da,24c1:24db,24c2:24dc,24c3:24dd,24c4:24de,24c5:24df,24c6:24e0,24c7:24e1,24c8:24e2,24c9:24e3,24ca:24e4,24cb:24e5,24cc:24e6,24cd:24e7,24ce:24e8,24cf:24e9,2c00:2c30,2c01:2c31,2c02:2c32,2c03:2c33,2c04:2c34,2c05:2c35,2c06:2c36,2c07:2c37,2c08:2c38,2c09:2c39,2c0a:2c3a,2c0b:2c3b,2c0c:2c3c,2c0d:2c3d,2c0e:2c3e,2c0f:2c3f,2c10:2c40,2c11:2c41,2c12:2c42,2c13:2c43,2c14:2c44,2c15:2c45,2c16:2c46,2c17:2c47,2c18:2c48,2c19:2c49,2c1a:2c4a,2c1b:2c4b,2c1c:2c4c,2c1d:2c4d,2c1e:2c4e,2c1f:2c4f,2c20:2c50,2c21:2c51,2c22:2c52,2c23:2c53,2c24:2c54,2c25:2c55,2c26:2c56,2c27:2c57,2c28:2c58,2c29:2c59,2c2a:2c5a,2c2b:2c5b,2c2c:2c5c,2c2d:2c5d,2c2e:2c5e,2c2f:2c5f,2c60:2c61,2c62:26b,2c63:1d7d,2c64:27d,2c67:2c68,2c69:2c6a,2c6b:2c6c,2c6d:251,2c6e:271,2c6f:250,2c70:252,2c72:2c73,2c75:2c76,2c7e:23f,2c7f:240,2c80:2c81,2c82:2c83,2c84:2c85,2c86:2c87,2c88:2c89,2c8a:2c8b,2c8c:2c8d,2c8e:2c8f,2c90:2c91,2c92:2c93,2c94:2c95,2c96:2c97,2c98:2c99,2c9a:2c9b,2c9c:2c9d,2c9e:2c9f,2ca0:2ca1,2ca2:2ca3,2ca4:2ca5,2ca6:2ca7,2ca8:2ca9,2caa:2cab,2cac:2cad,2cae:2caf,2cb0:2cb1,2cb2:2cb3,2cb4:2cb5,2cb6:2cb7,2cb8:2cb9,2cba:2cbb,2cbc:2cbd,2cbe:2cbf,2cc0:2cc1,2cc2:2cc3,2cc4:2cc5,2cc6:2cc7,2cc8:2cc9,2cca:2ccb,2ccc:2ccd,2cce:2ccf,2cd0:2cd1,2cd2:2cd3,2cd4:2cd5,2cd6:2cd7,2cd8:2cd9,2cda:2cdb,2cdc:2cdd,2cde:2cdf,2ce0:2ce1,2ce2:2ce3,2ceb:2cec,2ced:2cee,2cf2:2cf3,a640:a641,a642:a643,a644:a645,a646:a647,a648:a649,a64a:a64b,a64c:a64d,a64e:a64f,a650:a651,a652:a653,a654:a655,a656:a657,a658:a659,a65a:a65b,a65c:a65d,a65e:a65f,a660:a661,a662:a663,a664:a665,a666:a667,a668:a669,a66a:a66b,a66c:a66d,a680:a681,a682:a683,a684:a685,a686:a687,a688:a689,a68a:a68b,a68c:a68d,a68e:a68f,a690:a691,a692:a693,a694:a695,a696:a697,a698:a699,a69a:a69b,a722:a723,a724:a725,a726:a727,a728:a729,a72a:a72b,a72c:a72d,a72e:a72f,a732:a733,a734:a735,a736:a737,a738:a739,a73a:a73b,a73c:a73d,a73e:a73f,a740:a741,a742:a743,a744:a745,a746:a747,a748:a749,a74a:a74b,a74c:a74d,a74e:a74f,a750:a751,a752:a753,a754:a755,a756:a757,a758:a759,a75a:a75b,a75c:a75d,a75e:a75f,a760:a761,a762:a763,a764:a765,a766:a767,a768:a769,a76a:a76b,a76c:a76d,a76e:a76f,a779:a77a,a77b:a77c,a77d:1d79,a77e:a77f,a780:a781,a782:a783,a784:a785,a786:a787,a78b:a78c,a78d:265,a790:a791,a792:a793,a796:a797,a798:a799,a79a:a79b,a79c:a79d,a79e:a79f,a7a0:a7a1,a7a2:a7a3,a7a4:a7a5,a7a6:a7a7,a7a8:a7a9,a7aa:266,a7ab:25c,a7ac:261,a7ad:26c,a7ae:26a,a7b0:29e,a7b1:287,a7b2:29d,a7b3:ab53,a7b4:a7b5,a7b6:a7b7,a7b8:a7b9,a7ba:a7bb,a7bc:a7bd,a7be:a7bf,a7c0:a7c1,a7c2:a7c3,a7c4:a794,a7c5:282,a7c6:1d8e,a7c7:a7c8,a7c9:a7ca,a7cb:264,a7cc:a7cd,a7ce:a7cf,a7d0:a7d1,a7d2:a7d3,a7d4:a7d5,a7d6:a7d7,a7d8:a7d9,a7da:a7db,a7dc:19b,a7f5:a7f6,ff21:ff41,ff22:ff42,ff23:ff43,ff24:ff44,ff25:ff45,ff26:ff46,ff27:ff47,ff28:ff48,ff29:ff49,ff2a:ff4a,ff2b:ff4b,ff2c:ff4c,ff2d:ff4d,ff2e:ff4e,ff2f:ff4f,ff30:ff50,ff31:ff51,ff32:ff52,ff33:ff53,ff34:ff54,ff35:ff55,ff36:ff56,ff37:ff57,ff38:ff58,ff39:ff59,ff3a:ff5a,10400:10428,10401:10429,10402:1042a,10403:1042b,10404:1042c,10405:1042d,10406:1042e,10407:1042f,10408:10430,10409:10431,1040a:10432,1040b:10433,1040c:10434,1040d:10435,1040e:10436,1040f:10437,10410:10438,10411:10439,10412:1043a,10413:1043b,10414:1043c,10415:1043d,10416:1043e,10417:1043f,10418:10440,10419:10441,1041a:10442,1041b:10443,1041c:10444,1041d:10445,1041e:10446,1041f:10447,10420:10448,10421:10449,10422:1044a,10423:1044b,10424:1044c,10425:1044d,10426:1044e,10427:1044f,104b0:104d8,104b1:104d9,104b2:104da,104b3:104db,104b4:104dc,104b5:104dd,104b6:104de,104b7:104df,104b8:104e0,104b9:104e1,104ba:104e2,104bb:104e3,104bc:104e4,104bd:104e5,104be:104e6,104bf:104e7,104c0:104e8,104c1:104e9,104c2:104ea,104c3:104eb,104c4:104ec,104c5:104ed,104c6:104ee,104c7:104ef,104c8:104f0,104c9:104f1,104ca:104f2,104cb:104f3,104cc:104f4,104cd:104f5,104ce:104f6,104cf:104f7,104d0:104f8,104d1:104f9,104d2:104fa,104d3:104fb,10570:10597,10571:10598,10572:10599,10573:1059a,10574:1059b,10575:1059c,10576:1059d,10577:1059e,10578:1059f,10579:105a0,1057a:105a1,1057c:105a3,1057d:105a4,1057e:105a5,1057f:105a6,10580:105a7,10581:105a8,10582:105a9,10583:105aa,10584:105ab,10585:105ac,10586:105ad,10587:105ae,10588:105af,10589:105b0,1058a:105b1,1058c:105b3,1058d:105b4,1058e:105b5,1058f:105b6,10590:105b7,10591:105b8,10592:105b9,10594:105bb,10595:105bc,10c80:10cc0,10c81:10cc1,10c82:10cc2,10c83:10cc3,10c84:10cc4,10c85:10cc5,10c86:10cc6,10c87:10cc7,10c88:10cc8,10c89:10cc9,10c8a:10cca,10c8b:10ccb,10c8c:10ccc,10c8d:10ccd,10c8e:10cce,10c8f:10ccf,10c90:10cd0,10c91:10cd1,10c92:10cd2,10c93:10cd3,10c94:10cd4,10c95:10cd5,10c96:10cd6,10c97:10cd7,10c98:10cd8,10c99:10cd9,10c9a:10cda,10c9b:10cdb,10c9c:10cdc,10c9d:10cdd,10c9e:10cde,10c9f:10cdf,10ca0:10ce0,10ca1:10ce1,10ca2:10ce2,10ca3:10ce3,10ca4:10ce4,10ca5:10ce5,10ca6:10ce6,10ca7:10ce7,10ca8:10ce8,10ca9:10ce9,10caa:10cea,10cab:10ceb,10cac:10cec,10cad:10ced,10cae:10cee,10caf:10cef,10cb0:10cf0,10cb1:10cf1,10cb2:10cf2,10d50:10d70,10d51:10d71,10d52:10d72,10d53:10d73,10d54:10d74,10d55:10d75,10d56:10d76,10d57:10d77,10d58:10d78,10d59:10d79,10d5a:10d7a,10d5b:10d7b,10d5c:10d7c,10d5d:10d7d,10d5e:10d7e,10d5f:10d7f,10d60:10d80,10d61:10d81,10d62:10d82,10d63:10d83,10d64:10d84,10d65:10d85,118a0:118c0,118a1:118c1,118a2:118c2,118a3:118c3,118a4:118c4,118a5:118c5,118a6:118c6,118a7:118c7,118a8:118c8,118a9:118c9,118aa:118ca,118ab:118cb,118ac:118cc,118ad:118cd,118ae:118ce,118af:118cf,118b0:118d0,118b1:118d1,118b2:118d2,118b3:118d3,118b4:118d4,118b5:118d5,118b6:118d6,118b7:118d7,118b8:118d8,118b9:118d9,118ba:118da,118bb:118db,118bc:118dc,118bd:118dd,118be:118de,118bf:118df,16e40:16e60,16e41:16e61,16e42:16e62,16e43:16e63,16e44:16e64,16e45:16e65,16e46:16e66,16e47:16e67,16e48:16e68,16e49:16e69,16e4a:16e6a,16e4b:16e6b,16e4c:16e6c,16e4d:16e6d,16e4e:16e6e,16e4f:16e6f,16e50:16e70,16e51:16e71,16e52:16e72,16e53:16e73,16e54:16e74,16e55:16e75,16e56:16e76,16e57:16e77,16e58:16e78,16e59:16e79,16e5a:16e7a,16e5b:16e7b,16e5c:16e7c,16e5d:16e7d,16e5e:16e7e,16e5f:16e7f,16ea0:16ebb,16ea1:16ebc,16ea2:16ebd,16ea3:16ebe,16ea4:16ebf,16ea5:16ec0,16ea6:16ec1,16ea7:16ec2,16ea8:16ec3,16ea9:16ec4,16eaa:16ec5,16eab:16ec6,16eac:16ec7,16ead:16ec8,16eae:16ec9,16eaf:16eca,16eb0:16ecb,16eb1:16ecc,16eb2:16ecd,16eb3:16ece,16eb4:16ecf,16eb5:16ed0,16eb6:16ed1,16eb7:16ed2,16eb8:16ed3,1e900:1e922,1e901:1e923,1e902:1e924,1e903:1e925,1e904:1e926,1e905:1e927,1e906:1e928,1e907:1e929,1e908:1e92a,1e909:1e92b,1e90a:1e92c,1e90b:1e92d,1e90c:1e92e,1e90d:1e92f,1e90e:1e930,1e90f:1e931,1e910:1e932,1e911:1e933,1e912:1e934,1e913:1e935,1e914:1e936,1e915:1e937,1e916:1e938,1e917:1e939,1e918:1e93a,1e919:1e93b,1e91a:1e93c,1e91b:1e93d,1e91c:1e93e,1e91d:1e93f,1e91e:1e940,1e91f:1e941,1e920:1e942,1e921:1e943",
}
-- END unicode tables

local uni_ranges, uni_maps = {}, {}

--- Whether code point `c` is in the table `name` of ranges (found the
--- first time one is asked for).
local function in_ranges(name, c)
  local t = uni_ranges[name]
  if not t then
    t = {}
    for first, last in string.gmatch(UNICODE[name], "(%x+)%-?(%x*)") do
      local lo = tonumber(first, 16)
      t[#t + 1] = lo
      t[#t + 1] = last == "" and lo or tonumber(last, 16)
    end
    uni_ranges[name] = t
  end
  local a, b = 1, #t // 2
  while a <= b do
    local m = (a + b) // 2
    if c < t[2 * m - 1] then
      b = m - 1
    elseif c > t[2 * m] then
      a = m + 1
    else
      return true
    end
  end
  return false
end

--- The code points `c` becomes in the mapping `name`, or nil.
local function mapped(name, c)
  local t = uni_maps[name]
  if not t then
    t = {}
    for from, to in string.gmatch(UNICODE[name], "(%x+):([%x%.]+)") do
      local cps = {}
      for cp in string.gmatch(to, "%x+") do
        cps[#cps + 1] = tonumber(cp, 16)
      end
      t[tonumber(from, 16)] = cps
    end
    uni_maps[name] = t
  end
  return t[c]
end

--- Unicode White_Space.
local function is_space(c)
  if c < 128 then return c == 32 or between(c, 9, 13) end
  return c == 0x85 or c == 0xA0 or c == 0x1680 or between(c, 0x2000, 0x200A) or c == 0x2028 or c == 0x2029
    or c == 0x202F or c == 0x205F or c == 0x3000
end

-- The bytes of the UTF-8 encoding of code point `c`.
local function utf8_width(c)
  if c < 0x80 then return 1 end
  if c < 0x800 then return 2 end
  if c < 0x10000 then return 3 end
  return 4
end

local function ascii_only(s)
  return not string.find(s, "[\128-\255]")
end

--- `s` without the whitespace at its start, its end, or both.
local function trim_by(s, front, back)
  local first, last = 1, #s
  if front then
    while first <= last do
      local c = utf8.codepoint(s, first)
      if not is_space(c) then break end
      first = first + utf8_width(c)
    end
  end
  if back then
    while last >= first do
      local at = utf8.offset(s, -1, last + 1)
      local c = utf8.codepoint(s, at)
      if not is_space(c) then break end
      last = at - 1
    end
  end
  return s:sub(first, last)
end

--- `s` without the ASCII white space at its start, its end, or both, in one
--- pass from each end.
local function trim_ascii(s, front, back)
  local first, last = 1, #s
  if front then
    first = string.find(s, "%S") or last + 1
  end
  if back then
    while last >= first and string.find(s, "^%s", last) do
      last = last - 1
    end
  end
  return s:sub(first, last)
end

--- Text that isn't valid UTF-8 is trimmed as bytes.
ext["std.string.Str.trim"] = function(s)
  if ascii_only(s) or not utf8.len(s) then return trim_ascii(s, true, true) end
  return trim_by(s, true, true)
end

ext["std.string.Str.trim_start"] = function(s)
  if ascii_only(s) or not utf8.len(s) then return trim_ascii(s, true, false) end
  return trim_by(s, true, false)
end

ext["std.string.Str.trim_end"] = function(s)
  if ascii_only(s) or not utf8.len(s) then return trim_ascii(s, false, true) end
  return trim_by(s, false, true)
end

ext["std.string.Str.trim_ascii"] = function(s)
  return trim_ascii(s, true, true)
end

ext["std.string.Str.trim_ascii_start"] = function(s)
  return trim_ascii(s, true, false)
end

ext["std.string.Str.trim_ascii_end"] = function(s)
  return trim_ascii(s, false, true)
end

local function change_case(s, name, ascii)
  if ascii_only(s) or not utf8.len(s) then
    return (s:gsub(ascii, name == "to_upper" and string.upper or string.lower))
  end
  local out = {}
  for _, c in utf8.codes(s) do
    local to = c >= 128 and mapped(name, c) or nil
    if to then
      for _, cp in ipairs(to) do
        out[#out + 1] = utf8.char(cp)
      end
    elseif c < 128 then
      local b = c
      if name == "to_upper" and is_lower(c) then b = c - 32 end
      if name == "to_lower" and is_upper(c) then b = c + 32 end
      out[#out + 1] = string.char(b)
    else
      out[#out + 1] = utf8.char(c)
    end
  end
  return table.concat(out)
end

ext["std.string.Str.to_upper"] = function(s)
  return change_case(s, "to_upper", "%l")
end

ext["std.string.Str.to_lower"] = function(s)
  return change_case(s, "to_lower", "%u")
end

ext["std.string.Str.to_ascii_upper"] = function(s)
  return (s:gsub("%l", string.upper))
end

ext["std.string.Str.to_ascii_lower"] = function(s)
  return (s:gsub("%u", string.lower))
end

ext["std.string.Str.replace"] = function(s, from, to)
  if from == "" then
    -- `to` before every character, and at the end, as Rust's does.
    local out = {}
    for c in string.gmatch(s, utf8.charpattern) do
      out[#out + 1] = to
      out[#out + 1] = c
    end
    out[#out + 1] = to
    return concat(out)
  end
  local pattern = from:gsub("%p", "%%%0")
  return (s:gsub(pattern, (to:gsub("%%", "%%%%"))))
end

ext["std.string.Str.repeat"] = function(s, n)
  -- Past the host's memory limit (8 GiB) it could never be made.
  if #s > 0 and n > (8 << 30) // #s then
    error(sformat("%sout of memory: the string would be %d times %d bytes", MARK, n, #s), 2)
  end
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
  local byte, codepoint = string.byte, utf8.codepoint
  return function()
    if at > n then return nil end
    local c = byte(s, at)
    if c < 0x80 then
      at = at + 1
      return true, c
    end
    c = codepoint(s, at)
    at = at + utf8_width(c)
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
  local at, n = 1, #s
  local ascii = ascii_only(s) or not utf8.len(s)
  return function()
    if ascii then
      local i, j = string.find(s, "%S+", at)
      if i == nil then return nil end
      at = j + 1
      return true, s:sub(i, j)
    end
    while at <= n do
      local c = utf8.codepoint(s, at)
      if not is_space(c) then break end
      at = at + utf8_width(c)
    end
    if at > n then return nil end
    local start = at
    while at <= n do
      local c = utf8.codepoint(s, at)
      if is_space(c) then break end
      at = at + utf8_width(c)
    end
    return true, s:sub(start, at - 1)
  end
end

local function same(s)
  return s
end
ext["std.string.Str.to_string"] = same

-- The `char` that ends at byte `e` (0-based), and where it starts.
ext["std.string.Str.char_before"] = function(s, e)
  if e == 0 then return { 0xFFFD, 0 } end
  local at = utf8.offset(s, -1, e + 1)
  return { utf8.codepoint(s, at), at - 1 }
end
-- `copy` takes the spool it is given after the text; Lua has a collector, so
-- the descriptor of its type is all that comes first and it is not used.
ext["std.string.Str.copy"] = function(_, s)
  return s
end

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

ext["std.string.Char.is_digit"] = is_digit
ext["std.string.Char.is_ascii_upper"] = is_upper
ext["std.string.Char.is_ascii_lower"] = is_lower

ext["std.string.Char.is_upper"] = function(c)
  if c < 128 then return is_upper(c) end
  return in_ranges("upper", c)
end

ext["std.string.Char.is_lower"] = function(c)
  if c < 128 then return is_lower(c) end
  return in_ranges("lower", c)
end

ext["std.string.Char.is_numeric"] = function(c)
  if c < 128 then return is_digit(c) end
  return in_ranges("numeric", c)
end

ext["std.string.Char.is_alphabetic"] = function(c)
  if c < 128 then return is_upper(c) or is_lower(c) end
  return in_ranges("alphabetic", c)
end

ext["std.string.Char.is_alphanumeric"] = function(c)
  if c < 128 then return is_upper(c) or is_lower(c) or is_digit(c) end
  return in_ranges("alphabetic", c) or in_ranges("numeric", c)
end

ext["std.string.Char.is_ascii_alphabetic"] = function(c)
  return is_upper(c) or is_lower(c)
end

ext["std.string.Char.is_ascii_alphanumeric"] = function(c)
  return is_upper(c) or is_lower(c) or is_digit(c)
end

ext["std.string.Char.is_whitespace"] = is_space

ext["std.string.Char.is_ascii_whitespace"] = function(c)
  return c == 32 or between(c, 9, 13)
end

ext["std.string.Char.to_ascii_upper"] = function(c)
  if is_lower(c) then return c - 32 end
  return c
end

ext["std.string.Char.to_ascii_lower"] = function(c)
  if is_upper(c) then return c + 32 end
  return c
end

-- A character with a one-character mapping becomes it; one with none, or
-- a longer one (`ß` to `SS`), stays.
ext["std.string.Char.to_upper"] = function(c)
  if c < 128 then
    if is_lower(c) then return c - 32 end
    return c
  end
  local to = mapped("to_upper", c)
  if to and #to == 1 then return to[1] end
  return c
end

ext["std.string.Char.to_lower"] = function(c)
  if c < 128 then
    if is_upper(c) then return c + 32 end
    return c
  end
  local to = mapped("to_lower", c)
  if to and #to == 1 then return to[1] end
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

ext["std.string.Char.digit"] = function(c)
  if c < 48 or c > 57 then error(MARK .. "not a decimal digit: " .. show(rt.char, c, true), 2) end
  return c - 48
end

ext["std.string.Char.len_utf8"] = function(c)
  return utf8_width(c)
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

-- `?T`'s methods. A scalar `mut self` comes back after the result.
ext["std.option.Optional.take"] = function(_, v)
  return v, nil
end

ext["std.num.Int.is_power_of_two"] = function(d, v)
  if d.signed and v < 0 then return false end
  return v ~= 0 and v & (v - 1) == 0
end

ext["std.num.Int.log2"] = function(d, v)
  if v == 0 or (d.signed and v < 0) then
    error(MARK .. "log2 of " .. rt.fmt(d, v, nil) .. ": it needs a number above zero", 2)
  end
  local n, bits = -1, v
  while bits ~= 0 do
    bits = bits >> 1
    n = n + 1
  end
  return n
end

-- Number methods that need the exact result, or overflow: the work is done in
-- Lua integers, with unsigned 64-bit values going through `udiv`.

local function ucmp_lt(d, a, b)
  if d.u64 then return ult(a, b) end
  return a < b
end

local function divmod(d, a, b, name)
  if b == 0 then error(MARK .. name .. ": division by zero", 3) end
  return rt.div(d, a, b), rt.rem(d, a, b)
end

local function fits(d, r, name)
  if r == nil then error(MARK .. name .. ": the result doesn't fit", 3) end
  return r
end

ext["std.num.Int.signum"] = function(d, v)
  if d.signed then return v < 0 and -1 or (v > 0 and 1 or 0) end
  return v ~= 0 and 1 or 0
end

ext["std.num.Int.rem_euclid"] = function(d, a, b)
  if b == 0 then error(MARK .. "rem_euclid: division by zero", 2) end
  local m = rt.rem(d, a, b)
  if d.signed and m < 0 then m = m + (b < 0 and -b or b) end
  return m
end

ext["std.num.Int.div_euclid"] = function(d, a, b)
  local q, m = divmod(d, a, b, "div_euclid")
  if d.signed and m < 0 then q = b > 0 and q - 1 or q + 1 end
  return q
end

ext["std.num.Int.div_floor"] = function(d, a, b)
  local q, m = divmod(d, a, b, "div_floor")
  if d.signed and m ~= 0 and ((m < 0) ~= (b < 0)) then q = q - 1 end
  return q
end

ext["std.num.Int.div_ceil"] = function(d, a, b)
  local q, m = divmod(d, a, b, "div_ceil")
  if m ~= 0 and (not d.signed or ((m < 0) == (b < 0))) then q = q + 1 end
  return q
end

local function gcd(d, a, b)
  while b ~= 0 do
    a, b = b, rt.rem(d, a, b)
  end
  if d.signed and a < 0 then
    if a == d.lo then error(MARK .. "gcd: the result doesn't fit", 3) end
    a = -a
  end
  return a
end

ext["std.num.Int.gcd"] = function(d, a, b)
  return gcd(d, a, b)
end

ext["std.num.Int.lcm"] = function(d, a, b)
  if a == 0 or b == 0 then return 0 end
  local g = gcd(d, a, b)
  local q = rt.div(d, a, g)
  if d.signed and (q == d.lo or b == d.lo) then error(MARK .. "lcm: the result doesn't fit", 2) end
  if d.signed and q < 0 then q = -q end
  if d.signed and b < 0 then b = -b end
  local fine, r = pcall(rt.mul, d, q, b)
  if not fine then error(MARK .. "lcm: the result doesn't fit", 2) end
  return r
end

ext["std.num.Int.isqrt"] = function(d, v)
  if d.signed and v < 0 then error(MARK .. "isqrt: the number is below zero", 2) end
  local f = v >= 0 and v + 0.0 or ((v >> 1) + 0.0) * 2.0
  local r = floor(math.sqrt(f))
  if r > 4294967295 then r = 4294967295 end
  r = math.tointeger(r)
  while r > 0 and ult(v, r * r) do r = r - 1 end
  while r < 4294967295 and not ult(v, (r + 1) * (r + 1)) do r = r + 1 end
  return r
end

ext["std.num.Int.ilog10"] = function(d, v)
  if v == 0 or (d.signed and v < 0) then
    error(MARK .. "ilog10: the number must be above zero", 2)
  end
  local n = 0
  while not ult(v, 10) do
    v = udiv(v, 10)
    n = n + 1
  end
  return n
end

ext["std.num.Int.next_power_of_two"] = function(d, v)
  local p = 1
  while ucmp_lt(d, p, v) do
    p = p << 1
    if p == 0 then error(MARK .. "next_power_of_two: the result doesn't fit", 2) end
  end
  if not d.u64 and (p < 0 or p > d.hi) then
    error(MARK .. "next_power_of_two: the result doesn't fit", 2)
  end
  return p
end

local function pow_by(mul, d, v, e)
  local r, b = 1, v
  while e > 0 do
    if e & 1 == 1 then r = mul(d, r, b) end
    e = e >> 1
    if e > 0 then b = mul(d, b, b) end
  end
  return r
end

ext["std.num.Int.pow_wrap"] = function(d, v, e)
  return pow_by(rt.mul_wrap, d, v, e)
end

ext["std.num.Int.pow_sat"] = function(d, v, e)
  local fine, r = pcall(pow_by, rt.mul, d, v, e)
  if fine then return r end
  if d.signed and v < 0 and e & 1 == 1 then return d.lo end
  return d.hi
end

ext["std.num.Int.count_zeros"] = function(d, v)
  local x, n = v & d.mask, 0
  while x ~= 0 do
    n = n + (x & 1)
    x = x >> 1
  end
  return d.bits - n
end

-- Rotates the type's bits `n` places left: Lua's `<<` and `>>` are logical
-- on all 64 bits, and `wrapv` keeps the type's bits (sign-extending).
local function rotl(d, v, n)
  local bits = d.bits
  n = n % bits
  if n == 0 then return v end
  local x = v & d.mask
  return wrapv(d, (x << n) | (x >> (bits - n)))
end

ext["std.num.Int.rotate_left"] = function(d, v, n)
  return rotl(d, v, n)
end

ext["std.num.Int.rotate_right"] = function(d, v, n)
  return rotl(d, v, d.bits - n % d.bits)
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

ext["std.num.Float.to_bits"] = function(d, v)
  if d.name == "f32" then return (string.unpack("<I4", string.pack("<f", v))) end
  return (string.unpack("<i8", string.pack("<d", v)))
end

ext["std.num.Float.from_bits"] = function(d, bits)
  if d.name == "f32" then return (string.unpack("<f", string.pack("<I4", bits))) end
  return (string.unpack("<d", string.pack("<i8", bits)))
end

ext["std.num.Float.is_nan"] = function(_, v)
  return v ~= v
end

local function float_fn(name, f)
  ext["std.num.Float." .. name] = function(d, v, b)
    return float(d, f(v, b))
  end
end

float_fn("sin", math.sin)
float_fn("cos", math.cos)
float_fn("tan", math.tan)
float_fn("asin", math.asin)
float_fn("acos", math.acos)
float_fn("atan", math.atan)
float_fn("atan2", function(a, b) return math.atan(a, b) end)
float_fn("sinh", function(a) return (math.exp(a) - math.exp(-a)) / 2.0 end)
float_fn("cosh", function(a) return (math.exp(a) + math.exp(-a)) / 2.0 end)
float_fn("tanh", function(a)
  local e = math.exp(2.0 * a)
  if e == huge then return 1.0 end
  return (e - 1.0) / (e + 1.0)
end)
float_fn("exp", math.exp)
float_fn("exp2", function(a) return 2.0 ^ a end)
float_fn("ln", function(a) return math.log(a) end)
float_fn("log2", function(a) return math.log(a, 2) end)
float_fn("log10", function(a) return math.log(a, 10) end)
float_fn("log", function(a, b) return math.log(a) / math.log(b) end)
float_fn("hypot", function(a, b)
  a, b = math.abs(a), math.abs(b)
  if a == huge or b == huge then return huge end
  local m = a > b and a or b
  if m == 0 or m ~= m then return m end
  a, b = a / m, b / m
  return m * math.sqrt(a * a + b * b)
end)
float_fn("fract", function(a)
  local t = a >= 0 and floor(a) or ceil(a)
  return a - t
end)
float_fn("signum", function(a)
  if a ~= a then return a end
  if a < 0 or (a == 0 and 1 / a < 0) then return -1.0 end
  return 1.0
end)
float_fn("copysign", function(a, b)
  local neg = b < 0 or (b == 0 and 1 / b < 0)
  a = math.abs(a)
  return neg and -a or a
end)
float_fn("to_degrees", function(a) return a * (180.0 / math.pi) end)
float_fn("to_radians", function(a) return a * (math.pi / 180.0) end)

ext["std.num.Float.is_finite"] = function(_, v)
  return v == v and v ~= huge and v ~= -huge
end

ext["std.num.Float.is_infinite"] = function(_, v)
  return v == huge or v == -huge
end

ext["std.num.Float.is_sign_negative"] = function(_, v)
  return v < 0 or (v == 0 and 1 / v < 0)
end

-- ================================================================ constants

--- Makes `C` compute each constant the first time it's read, from its
--- initializer in `init`. Compile-time code has run in the compiler by now
--- (on Treadle, see design/vm.md §17), and a constant that needs its own
--- value was reported there (K0107): reading one while it's being computed
--- is a bug in Loom.
function rt.lazy(C, init)
  -- `nils`: the constants computed to nil (`none`), which `C` can't hold.
  local busy, nils = {}, {}
  setmetatable(C, {
    __index = function(t, k)
      if nils[k] then return nil end
      if busy[k] then error("loom internal: constant " .. k .. " needs its own value", 2) end
      local f = init[k]
      if f == nil then return nil end
      busy[k] = true
      local v = f()
      busy[k] = nil
      if v == nil then nils[k] = true else rawset(t, k, v) end
      return v
    end,
  })
end

return rt
