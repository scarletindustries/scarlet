# binary-trees: builds complete binary trees and walks them to count their
# nodes. It stresses allocating and freeing many small objects.
# Ported from the Computer Language Benchmarks Game (BSD-3-Clause licence).

import sys

sys.setrecursionlimit(10000)


def bottom_up_tree(depth):
    if depth > 0:
        depth -= 1
        return (bottom_up_tree(depth), bottom_up_tree(depth))
    return (None, None)


def item_check(tree):
    left, right = tree
    if left is None:
        return 1
    return 1 + item_check(left) + item_check(right)


def main():
    n = 16
    min_depth = 4
    max_depth = max(min_depth + 2, n)

    stretch_depth = max_depth + 1
    print(f"stretch tree of depth {stretch_depth}\t check: {item_check(bottom_up_tree(stretch_depth))}")

    long_lived_tree = bottom_up_tree(max_depth)

    for depth in range(min_depth, max_depth + 1, 2):
        iterations = 2 ** (max_depth - depth + min_depth)
        check = 0
        for _ in range(iterations):
            check += item_check(bottom_up_tree(depth))
        print(f"{iterations}\t trees of depth {depth}\t check: {check}")

    print(f"long lived tree of depth {max_depth}\t check: {item_check(long_lived_tree)}")


main()
