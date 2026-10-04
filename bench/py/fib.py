# bench/fib.loom in idiomatic Python, for comparison (bench/README.md).
def fib(n):
    return n if n < 2 else fib(n - 1) + fib(n - 2)


print(fib(32))
