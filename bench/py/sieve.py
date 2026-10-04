# bench/sieve.loom in idiomatic Python, for comparison (bench/README.md).
size = 10_000_000
composite = bytearray(size + 1)
count = 0
for i in range(2, size + 1):
    if not composite[i]:
        count += 1
        for j in range(i * i, size + 1, i):
            composite[j] = 1
print(count)
