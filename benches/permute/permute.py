# Permute: walks the permutations of a six-element array by swapping in place.
# It stresses array reads and writes and deep recursion.
# Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet), MIT licence.


class Permute:
    def __init__(self):
        self.count = 0
        self.check = 0
        self.v = None

    def benchmark(self):
        self.count = 0
        self.v = [0, 1, 2, 3, 4, 5]
        self.permute(6)
        return self.count

    def permute(self, n):
        self.count += 1
        if n == 0:
            self.check = (self.check * 31 + self.v[0]) % 1000003
        else:
            n1 = n - 1
            self.permute(n1)
            for i in range(n1, -1, -1):
                self.swap(n1, i)
                self.permute(n1)
                self.swap(n1, i)

    def swap(self, i, j):
        tmp = self.v[i]
        self.v[i] = self.v[j]
        self.v[j] = tmp


def main():
    bench = Permute()
    total = 0
    for _ in range(200):
        total += bench.benchmark()
    print(total)
    print(bench.check)


main()
