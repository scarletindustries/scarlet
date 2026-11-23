# NBody: five bodies (the sun and four planets) pull on each other for many
# small time steps. It stresses float arithmetic and record fields. Ported from
# the Are We Fast Yet suite (MIT licence), which took it from the Computer
# Language Benchmarks Game. The square root is Newton's method written out, the
# same in every language, because Scarlet has none.

PI = 3.141592653589793
SOLAR_MASS = 4.0 * PI * PI
DAYS_PER_YEAR = 365.24
STEPS = 100000


class Body:
    def __init__(self, x, y, z, vx, vy, vz, mass):
        self.x = x
        self.y = y
        self.z = z
        self.vx = vx * DAYS_PER_YEAR
        self.vy = vy * DAYS_PER_YEAR
        self.vz = vz * DAYS_PER_YEAR
        self.mass = mass * SOLAR_MASS


def sqrt(d):
    guess = d if d > 1.0 else 1.0
    while True:
        nxt = 0.5 * (guess + d / guess)
        if nxt >= guess:
            return guess
        guess = nxt


def create_bodies():
    bodies = [
        Body(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0),
        Body(
            4.84143144246472090,
            -1.16032004402742839,
            -0.103622044471123109,
            0.00166007664274403694,
            0.00769901118419740425,
            -0.0000690460016972063023,
            0.000954791938424326609,
        ),
        Body(
            8.34336671824457987,
            4.12479856412430479,
            -0.403523417114321381,
            -0.00276742510726862411,
            0.00499852801234917238,
            0.0000230417297573763929,
            0.000285885980666130812,
        ),
        Body(
            12.8943695621391310,
            -15.1111514016986312,
            -0.223307578892655734,
            0.00296460137564761618,
            0.00237847173959480950,
            -0.0000296589568540237556,
            0.0000436624404335156298,
        ),
        Body(
            15.3796971148509165,
            -25.9193146099879641,
            0.179258772950371181,
            0.00268067772490389322,
            0.00162824170038242295,
            -0.0000951592254519715870,
            0.0000515138902046611451,
        ),
    ]
    px = py = pz = 0.0
    for b in bodies:
        px += b.vx * b.mass
        py += b.vy * b.mass
        pz += b.vz * b.mass
    sun = bodies[0]
    sun.vx = 0.0 - px / SOLAR_MASS
    sun.vy = 0.0 - py / SOLAR_MASS
    sun.vz = 0.0 - pz / SOLAR_MASS
    return bodies


def advance(bodies, dt):
    count = len(bodies)
    for i in range(count):
        i_body = bodies[i]

        for j in range(i + 1, count):
            j_body = bodies[j]
            dx = i_body.x - j_body.x
            dy = i_body.y - j_body.y
            dz = i_body.z - j_body.z

            d_squared = dx * dx + dy * dy + dz * dz
            distance = sqrt(d_squared)
            mag = dt / (d_squared * distance)

            i_body.vx = i_body.vx - dx * j_body.mass * mag
            i_body.vy = i_body.vy - dy * j_body.mass * mag
            i_body.vz = i_body.vz - dz * j_body.mass * mag

            j_body.vx = j_body.vx + dx * i_body.mass * mag
            j_body.vy = j_body.vy + dy * i_body.mass * mag
            j_body.vz = j_body.vz + dz * i_body.mass * mag

    for b in bodies:
        b.x = b.x + dt * b.vx
        b.y = b.y + dt * b.vy
        b.z = b.z + dt * b.vz


def energy(bodies):
    e = 0.0
    count = len(bodies)
    for i in range(count):
        i_body = bodies[i]

        e += 0.5 * i_body.mass * (
            i_body.vx * i_body.vx + i_body.vy * i_body.vy + i_body.vz * i_body.vz
        )

        for j in range(i + 1, count):
            j_body = bodies[j]
            dx = i_body.x - j_body.x
            dy = i_body.y - j_body.y
            dz = i_body.z - j_body.z

            distance = sqrt(dx * dx + dy * dy + dz * dz)
            e -= i_body.mass * j_body.mass / distance
    return e


def scaled(e):
    """The energy as a whole number, so every language prints the same text."""
    return int(e * 1000000000000000.0)


def main():
    bodies = create_bodies()
    print(scaled(energy(bodies)))
    for _ in range(STEPS):
        advance(bodies, 0.01)
    print(scaled(energy(bodies)))


main()
