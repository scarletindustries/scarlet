# Stresses the built-in map (a dict): insert scattered int keys, look them all
# up several times (with a miss probe beside each hit), then delete the oldest
# key and insert a new one in a long churn loop.
# Written for this suite, starting from examples/bench_map.sca.


# A full-period generator, so the keys never repeat. Its products stay far
# below 2^53, which keeps Lua and JavaScript numbers exact.
def next_key(x):
    return (x * 69069 + 1) % 4294967296


def lookups(m, x, n, acc):
    for _ in range(n):
        x = next_key(x)
        v = m.get(x)
        if v is not None:
            acc = (acc * 31 + v) % 1000000007
        v = m.get(x + 1)
        if v is not None:
            acc = acc + v
    return acc


def main():
    n = 200000
    seed = 42

    m = {}
    lead = seed
    for i in range(n):
        lead = next_key(lead)
        m[lead] = i

    acc = 0
    for _ in range(10):
        acc = lookups(m, seed, n, acc)
    print(acc)

    # The map is a sliding window over the key sequence: `trail` names the
    # oldest key, which goes, and `lead` the next new one, which comes in.
    trail = seed
    for i in range(3000000):
        trail = next_key(trail)
        lead = next_key(lead)
        del m[trail]
        m[lead] = i
    # The window's keys must all be there and the first `n`, long deleted,
    # must all be gone: a hit in the second pass would change the sum.
    window = lookups(m, trail, n, 0)
    print(lookups(m, seed, n, window))


main()
