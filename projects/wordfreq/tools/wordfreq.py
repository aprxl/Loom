"""The same report as projects/wordfreq/main.loom, for comparing with it.

Usage: python -I wordfreq.py [--top N] [--ngram N] [FILE...]
With no FILE, reads standard input.
"""

import re
import sys
from collections import Counter

TOKEN = re.compile(r"[A-Za-z0-9']+")


def fail(message):
    sys.stderr.write(f"wordfreq: {message}\n")
    sys.exit(1)


def flag_number(args, i):
    flag = args[i]
    if i + 1 == len(args):
        fail(f"{flag} needs a number of at least 1")
    text = args[i + 1]
    if not text.isascii() or not text.isdigit() or int(text) == 0:
        fail(f"{flag} needs a number of at least 1")
    return int(text)


def tokens(text):
    words = []
    for match in TOKEN.findall(text):
        word = match.lower().strip("'")
        if word:
            words.append(word)
    return words


def top(counts, n):
    ranked = sorted(counts.items(), key=lambda entry: (-entry[1], entry[0]))
    return ranked[:n]


def section(heading, entries):
    print(heading)
    if not entries:
        return
    width = len(str(entries[0][1]))
    for word, n in entries:
        print(f"  {str(n).rjust(width)} {word}")


def main():
    sys.stdout.reconfigure(newline="\n")
    args = sys.argv[1:]
    top_n = 10
    gram = 2
    files = []
    i = 0
    while i < len(args):
        if args[i] == "--top":
            top_n = flag_number(args, i)
            i += 1
        elif args[i] == "--ngram":
            gram = flag_number(args, i)
            i += 1
        elif args[i].startswith("--"):
            fail(f"unknown flag {args[i]}")
        else:
            files.append(args[i])
        i += 1

    all_words = []
    line_count = 0
    byte_count = 0
    if not files:
        data = sys.stdin.buffer.read()
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            fail("cannot read standard input: invalid UTF-8")
        pieces = text.split("\n")
        if pieces and pieces[-1] == "":
            pieces.pop()
        for line in pieces:
            line_count += 1
            byte_count += len(line.encode("utf-8")) + 1
            all_words.extend(tokens(line))
    else:
        for path in files:
            try:
                with open(path, "rb") as handle:
                    data = handle.read()
                text = data.decode("utf-8")
            except (OSError, UnicodeDecodeError):
                fail(f"cannot read {path}")
            line_count += text.count("\n") + (1 if text and not text.endswith("\n") else 0)
            byte_count += len(data)
            all_words.extend(tokens(text))

    counts = Counter(all_words)
    longest = ""
    for word in all_words:
        if len(word) > len(longest):
            longest = word

    print(f"words: {len(all_words)}")
    print(f"unique: {len(counts)}")
    print(f"lines: {line_count}")
    print(f"bytes: {byte_count}")
    print(f"longest: {longest} ({len(longest)})")

    section(f"top {top_n} words:", top(counts, top_n))

    grams = Counter(
        " ".join(all_words[i : i + gram]) for i in range(len(all_words) - gram + 1)
    ) if gram <= len(all_words) else Counter()
    section(f"top {top_n} {gram}-grams:", top(grams, top_n))


if __name__ == "__main__":
    main()
