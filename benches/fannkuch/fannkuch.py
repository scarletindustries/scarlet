# fannkuch-redux: flips the front of every permutation of 0..N-1 until 0 leads,
# and keeps a checksum and the longest run of flips. It stresses small-array
# reads, writes and reversal. Ported from Mike Pall's Lua program in the Computer
# Language Benchmarks Game (benchmarksgame-team.pages.debian.net, BSD-3 licence).

N = 9


def fannkuch(n):
    p = list(range(n))
    q = list(range(n))
    s = list(range(n))
    sign = 1
    maxflips = 0
    total = 0
    while True:
        # Copy and flip. The first element lives in q1, not in q.
        q1 = p[0]
        if q1 != 0:
            for i in range(1, n):
                q[i] = p[i]
            flips = 1
            while True:
                qq = q[q1]
                if qq == 0:
                    total += sign * flips
                    if flips > maxflips:
                        maxflips = flips
                    break
                q[q1] = q1
                if q1 >= 3:
                    i = 1
                    j = q1 - 1
                    while True:
                        t = q[i]
                        q[i] = q[j]
                        q[j] = t
                        i += 1
                        j -= 1
                        if i >= j:
                            break
                q1 = qq
                flips += 1
        # Permute.
        if sign == 1:
            t = p[1]
            p[1] = p[0]
            p[0] = t
            sign = -1
        else:
            t = p[1]
            p[1] = p[2]
            p[2] = t
            sign = 1
            for i in range(2, n):
                sx = s[i]
                if sx != 0:
                    s[i] = sx - 1
                    break
                if i == n - 1:
                    return total, maxflips
                s[i] = i
                # Rotate the first i + 2 elements left by one.
                t = p[0]
                for j in range(i + 1):
                    p[j] = p[j + 1]
                p[i + 1] = t


def main():
    total, flips = fannkuch(N)
    print(f"{total}\nPfannkuchen({N}) = {flips}")


main()
