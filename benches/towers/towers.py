# Towers of Hanoi with explicit disk stacks: 13 disks moved between three piles,
# many times over. It stresses small record updates and linked-list pushes and pops.
# Ported from the Are We Fast Yet suite (github.com/smarr/are-we-fast-yet, MIT licence).


class TowersDisk:
    def __init__(self, size):
        self.size = size
        self.next = None


class Towers:
    def __init__(self):
        self._piles = None
        self._moves_done = 0

    def _push_disk(self, disk, pile):
        top = self._piles[pile]

        if top is not None and disk.size >= top.size:
            raise Exception("Cannot put a big disk on a smaller one")

        disk.next = top
        self._piles[pile] = disk

    def _pop_disk_from(self, pile):
        top = self._piles[pile]
        if top is None:
            raise Exception("Attempting to remove a disk from an empty pile")

        self._piles[pile] = top.next
        top.next = None
        return top

    def _move_top_disk(self, from_pile, to_pile):
        self._push_disk(self._pop_disk_from(from_pile), to_pile)
        self._moves_done += 1

    def _build_tower_at(self, pile, disks):
        for i in range(disks, -1, -1):
            self._push_disk(TowersDisk(i), pile)

    def _move_disks(self, disks, from_pile, to_pile):
        if disks == 1:
            self._move_top_disk(from_pile, to_pile)
        else:
            other_pile = (3 - from_pile) - to_pile
            self._move_disks(disks - 1, from_pile, other_pile)
            self._move_top_disk(from_pile, to_pile)
            self._move_disks(disks - 1, other_pile, to_pile)

    def weigh(self, pile):
        disk = self._piles[pile]
        depth = 1
        acc = 0
        while disk is not None:
            acc += disk.size * depth
            depth += 1
            disk = disk.next
        return acc

    def benchmark(self):
        self._piles = [None, None, None]
        self._build_tower_at(0, 13)
        self._moves_done = 0
        self._move_disks(13, 0, 1)
        return self._moves_done


def main():
    moves = 0
    weight = 0
    for _ in range(600):
        towers = Towers()
        moves += towers.benchmark()
        weight += towers.weigh(1) + towers.weigh(0)
    print(moves)
    print(weight)


main()
