# Stresses pattern matching on nested constructors, allocation, and reuse of
# nodes in place: insert into a functional red-black tree (Okasaki balance), then fold.
# Ported from Koka's test/bench/koka/rbtree.kk (https://github.com/koka-lang/koka, Apache-2.0).

RED, BLACK = 0, 1

# A node is the tuple (color, left, key, value, right); a leaf is None.


def is_red(t):
    return t is not None and t[0] == RED


def balance_left(l, k, v, r):
    if l is None:
        return None
    _, ll, ky, vy, lr = l
    if is_red(ll):
        return (RED, (BLACK, ll[1], ll[2], ll[3], ll[4]), ky, vy, (BLACK, lr, k, v, r))
    elif is_red(lr):
        return (RED, (BLACK, ll, ky, vy, lr[1]), lr[2], lr[3], (BLACK, lr[4], k, v, r))
    else:
        return (BLACK, (RED, ll, ky, vy, lr), k, v, r)


def balance_right(l, k, v, r):
    if r is None:
        return None
    _, rl, ky, vy, rr = r
    if is_red(rl):
        return (RED, (BLACK, l, k, v, rl[1]), rl[2], rl[3], (BLACK, rl[4], ky, vy, rr))
    elif is_red(rr):
        return (RED, (BLACK, l, k, v, rl), ky, vy, (BLACK, rr[1], rr[2], rr[3], rr[4]))
    else:
        return (BLACK, l, k, v, (RED, rl, ky, vy, rr))


def ins(t, k, v):
    if t is None:
        return (RED, None, k, v, None)
    c, l, kx, vx, r = t
    if c == RED:
        if k < kx:
            return (RED, ins(l, k, v), kx, vx, r)
        elif k > kx:
            return (RED, l, kx, vx, ins(r, k, v))
        else:
            return (RED, l, k, v, r)
    else:
        if k < kx:
            if is_red(l):
                return balance_left(ins(l, k, v), kx, vx, r)
            else:
                return (BLACK, ins(l, k, v), kx, vx, r)
        elif k > kx:
            if is_red(r):
                return balance_right(l, kx, vx, ins(r, k, v))
            else:
                return (BLACK, l, kx, vx, ins(r, k, v))
        else:
            return (BLACK, l, k, v, r)


def set_black(t):
    if t is None:
        return t
    return (BLACK, t[1], t[2], t[3], t[4])


def insert(t, k, v):
    return set_black(ins(t, k, v))


def fold(t, b, f):
    if t is None:
        return b
    return fold(t[4], f(t[2], t[3], fold(t[1], b, f)), f)


def height(t):
    if t is None:
        return 0
    hl = height(t[1])
    hr = height(t[4])
    if hl > hr:
        return hl + 1
    else:
        return hr + 1


def make_tree(n):
    t = None
    while n > 0:
        n = n - 1
        t = insert(t, n, n % 10 == 0)
    return t


def main():
    t = make_tree(300000)
    count = fold(t, 0, lambda k, v, r: r + 1 if v else r)
    print(count)
    print(fold(t, 0, lambda k, v, r: (r * 31 + k) % 1000000007))
    print(height(t))


main()
