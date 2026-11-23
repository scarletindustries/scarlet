# AWFY List: builds linked lists of a user-defined type and runs the recursive
# tail (Takeuchi) comparison over them. It stresses allocation and recursion.
# Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet, MIT licence).


class Element:
    def __init__(self, v):
        self.val = v
        self.next = None

    def length(self):
        if self.next is None:
            return 1
        return 1 + self.next.length()


def make_list(length):
    if length == 0:
        return None
    e = Element(length)
    e.next = make_list(length - 1)
    return e


def is_shorter_than(x, y):
    x_tail = x
    y_tail = y
    while y_tail is not None:
        if x_tail is None:
            return True
        x_tail = x_tail.next
        y_tail = y_tail.next
    return False


def tail(x, y, z):
    if is_shorter_than(y, x):
        return tail(tail(x.next, y, z), tail(y.next, z, x), tail(z.next, x, y))
    return z


def benchmark():
    return tail(make_list(15), make_list(10), make_list(6)).length()


total = 0
for _ in range(2000):
    total += benchmark()
print(total)
