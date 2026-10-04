# bench/word_freq.loom in idiomatic Python, for comparison (bench/README.md).
SYLLABLES = ["ka", "lo", "mi", "ne", "su", "ta", "ro", "vi"]
MASK = (1 << 64) - 1
counts = {}
seed = 42
for _ in range(4_000_000):
    seed = (seed * 6364136223846793005 + 1442695040888963407) & MASK
    a = SYLLABLES[(seed >> 40) % 8]
    b = SYLLABLES[(seed >> 50) % 8]
    word = a + b if (seed >> 60) % 2 == 0 else a + b + a
    counts[word] = counts.get(word, 0) + 1
top = sorted(counts.items())
print(f"{len(counts)} words, first ({top[0][0]}, {top[0][1]}), last ({top[-1][0]}, {top[-1][1]})")
