-- bench/fib.loom in idiomatic Lua 5.4, for comparison (bench/README.md).
local function fib(n)
  if n < 2 then return n end
  return fib(n - 1) + fib(n - 2)
end

return function()
  io.write(fib(32), "\n")
end
