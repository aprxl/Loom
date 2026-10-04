# bench/binary_trees.loom in idiomatic Python, for comparison (bench/README.md).
def make(depth):
    return () if depth == 0 else (make(depth - 1), make(depth - 1))


def check(t):
    return 1 + sum(check(k) for k in t)


max_depth = 16
print(f"stretch tree of depth {max_depth + 1}\t check: {check(make(max_depth + 1))}")
long_lived = make(max_depth)
for depth in range(4, max_depth + 1, 2):
    iterations = 1 << (max_depth - depth + 4)
    total = sum(check(make(depth)) for _ in range(iterations))
    print(f"{iterations}\t trees of depth {depth}\t check: {total}")
print(f"long lived tree of depth {max_depth}\t check: {check(long_lived)}")
