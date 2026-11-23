# sieve: the sieve of Eratosthenes over an array of 5000 flags, run many times.
# It stresses array writes in a tight loop. Ported from the Are We Fast Yet
# suite (https://github.com/smarr/are-we-fast-yet, MIT licence).


def sieve(flags, size):
    prime_count = 0
    for i in range(2, size + 1):
        if flags[i - 1]:
            prime_count += 1
            k = i + i
            while k <= size:
                flags[k - 1] = False
                k += i
    return prime_count


def main():
    total = 0
    for _ in range(45):
        flags = []
        for _ in range(5000):
            flags.append(True)
        total += sieve(flags, 5000)
    print(total)


main()
