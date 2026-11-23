# Counts how often each word turns up in a stream of short words, in a dict
# keyed by string, then looks a second stream up in it. It stresses building
# short strings, hashing them and maps keyed by strings. Written for this suite.

LETTERS = ["a", "e", "i", "n", "r", "t"]


def next_seed(seed):
    return seed * 16807 % 2147483647


def word(r, lengths):
    """A word of 2 to `lengths + 1` letters, all of it decided by `r`."""
    n = 2 + r % lengths
    r //= lengths
    w = ""
    for _ in range(n):
        w += LETTERS[r % 6]
        r //= 6
    return w


def main():
    counts = {}
    seed = 42
    for _ in range(1000000):
        seed = next_seed(seed)
        w = word(seed, 5)
        counts[w] = counts.get(w, 0) + 1

    squares = 0
    for n in counts.values():
        squares += n * n

    # The second stream has longer words in it too, so some lookups miss.
    check = 0
    seed = 2024
    for _ in range(1000000):
        seed = next_seed(seed)
        n = counts.get(word(seed, 6), 0)
        check = (check * 31 + n) % 1000000007

    print(len(counts))
    print(squares)
    print(check)


main()
