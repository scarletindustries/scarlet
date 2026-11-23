# Queens from the Are We Fast Yet suite: place eight queens by backtracking over
# boolean arrays of free rows and diagonals. Stresses array reads and writes.
# Ported from https://github.com/smarr/are-we-fast-yet (MIT licence).


class Queens:
    def __init__(self):
        self._free_maxs = None
        self._free_rows = None
        self._free_mins = None
        self._queen_rows = None

    def queens(self):
        self._free_rows = [True] * 8
        self._free_maxs = [True] * 16
        self._free_mins = [True] * 16
        self._queen_rows = [-1] * 8

        return self.place_queen(0)

    def place_queen(self, c):
        for r in range(8):
            if self.get_row_column(r, c):
                self._queen_rows[r] = c
                self.set_row_column(r, c, False)

                if c == 7:
                    return True
                if self.place_queen(c + 1):
                    return True

                self.set_row_column(r, c, True)
        return False

    def get_row_column(self, r, c):
        return (
            self._free_rows[r] and self._free_maxs[c + r] and self._free_mins[c - r + 7]
        )

    def set_row_column(self, r, c, v):
        self._free_rows[r] = v
        self._free_maxs[c + r] = v
        self._free_mins[c - r + 7] = v

    # The column of each row's queen, read as one decimal number.
    def digits(self):
        acc = 0
        for c in self._queen_rows:
            acc = acc * 10 + c
        return acc


def main():
    board = Queens()
    solved = 0
    total = 0
    for _ in range(3500):
        if board.queens():
            solved += 1
            total += board.digits()
    print(solved)
    print(total)


main()
