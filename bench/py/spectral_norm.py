# bench/spectral_norm.loom in idiomatic Python, for comparison (bench/README.md).
from math import sqrt


def a(i, j):
    return 1.0 / ((i + j) * (i + j + 1) // 2 + i + 1)


def times(v, out):
    n = len(v)
    for i in range(n):
        out[i] = sum(a(i, j) * v[j] for j in range(n))


def times_transposed(v, out):
    n = len(v)
    for i in range(n):
        out[i] = sum(a(j, i) * v[j] for j in range(n))


def times_both(v, out, tmp):
    times(v, tmp)
    times_transposed(tmp, out)


n = 400
u, v, tmp = [1.0] * n, [0.0] * n, [0.0] * n
for _ in range(10):
    times_both(u, v, tmp)
    times_both(v, u, tmp)
vbv = sum(x * y for x, y in zip(u, v))
vv = sum(y * y for y in v)
print(f"{sqrt(vbv / vv):.9f}")
