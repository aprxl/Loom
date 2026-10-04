# bench/maps.loom in idiomatic Python, for comparison (bench/README.md).
size = 200_000
m = {}
for i in range(size):
    m[i << 12] = i
total = sum(m[i << 12] for i in range(size))
s = {}
for i in range(size):
    s[f"k{i}"] = i
hits = sum(1 for i in range(size) if f"k{i * 2}" in s)
for i in range(size):
    del m[i << 12]
print(len(m), total, len(s), hits)
