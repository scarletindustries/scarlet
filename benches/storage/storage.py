# Storage builds a tree of nested arrays, four children to a node and seven
# levels deep, over and over. It stresses allocation and freeing.
# Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet), MIT licence.


class Storage:
    def __init__(self):
        self.seed = 74755
        self.count = 0
        self.slots = 0

    def next(self):
        self.seed = (self.seed * 1309 + 13849) % 65536
        return self.seed

    def build(self, depth):
        self.count += 1
        if depth == 1:
            n = self.next() % 10 + 1
            self.slots += n
            return [None] * n
        arr = [None] * 4
        for i in range(4):
            arr[i] = self.build(depth - 1)
        return arr


def main():
    total_count = 0
    total_slots = 0
    for _ in range(500):
        storage = Storage()
        storage.build(7)
        total_count += storage.count
        total_slots += storage.slots
    print(total_count)
    print(total_slots)


main()
