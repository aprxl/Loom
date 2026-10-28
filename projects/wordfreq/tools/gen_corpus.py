"""Writes a deterministic ASCII text of about SIZE_MB megabytes.

Usage: python -I gen_corpus.py SIZE_MB OUT
"""

import random
import sys

SEED = 20261028
VOCAB_SIZE = 5000
WORDS_PER_LINE = 12
CHUNK = 100_000
CONSONANTS = "bcdfghjklmnprstvwz"
VOWELS = "aeiou"


def make_vocab(rng):
    seen = set()
    vocab = []
    while len(vocab) < VOCAB_SIZE:
        syllables = rng.randint(1, 4)
        word = "".join(rng.choice(CONSONANTS) + rng.choice(VOWELS) for _ in range(syllables))
        if rng.random() < 0.1:
            word += rng.choice(CONSONANTS)
        if word not in seen:
            seen.add(word)
            vocab.append(word)
    return vocab


def decorate(rng, word, first_in_sentence):
    if rng.random() < 0.05:
        word = word + "'s"
    elif rng.random() < 0.03:
        word = "don't"
    if first_in_sentence or rng.random() < 0.02:
        word = word.capitalize()
    roll = rng.random()
    if roll < 0.06:
        word += ","
    elif roll < 0.09:
        word += "."
    elif roll < 0.10:
        word += "!"
    elif roll < 0.11:
        word += "?"
    elif roll < 0.12:
        word = "'" + word + "'"
    return word


def main():
    size_mb = float(sys.argv[1])
    out_path = sys.argv[2]
    target = int(size_mb * 1024 * 1024)
    rng = random.Random(SEED)
    vocab = make_vocab(rng)
    weights = [1.0 / (rank + 1) for rank in range(VOCAB_SIZE)]

    written = 0
    words_on_line = 0
    first_in_sentence = True
    with open(out_path, "wb") as out:
        while written < target:
            batch = rng.choices(vocab, weights=weights, k=CHUNK)
            pieces = []
            for word in batch:
                text = decorate(rng, word, first_in_sentence)
                first_in_sentence = text[-1] in ".!?"
                words_on_line += 1
                if words_on_line == WORDS_PER_LINE:
                    text += "\n"
                    words_on_line = 0
                else:
                    text += " "
                pieces.append(text)
            data = "".join(pieces).encode("ascii")
            out.write(data)
            written += len(data)
        if words_on_line:
            out.write(b"\n")


if __name__ == "__main__":
    main()
