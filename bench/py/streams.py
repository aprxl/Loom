# bench/streams.loom in idiomatic Python, for comparison (bench/README.md).
from itertools import takewhile

total = 0
for rnd in range(5):
    total += sum(takewhile(lambda x: x < 5_000_000,
                           (x * 2 + rnd for x in range(3_000_000) if x % 3 != 0)))
print(total)
