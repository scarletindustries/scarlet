# Bounce: 100 balls bounce around a box for 50 steps. We count the bounces and
# add up where the balls end.
# It stresses record allocation, field reads and small-integer arithmetic.
# Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet, MIT licence).


# The suite's own generator.
class Random:
    def __init__(self):
        self._seed = 74755

    def next(self):
        self._seed = ((self._seed * 1309) + 13849) & 65535
        return self._seed


class Ball:
    def __init__(self, random):
        self._x = random.next() % 500
        self._y = random.next() % 500
        self._x_vel = (random.next() % 300) - 150
        self._y_vel = (random.next() % 300) - 150

    def bounce(self):
        x_limit = 500
        y_limit = 500
        bounced = False

        self._x += self._x_vel
        self._y += self._y_vel

        if self._x > x_limit:
            self._x = x_limit
            self._x_vel = -abs(self._x_vel)
            bounced = True

        if self._x < 0:
            self._x = 0
            self._x_vel = abs(self._x_vel)
            bounced = True

        if self._y > y_limit:
            self._y = y_limit
            self._y_vel = -abs(self._y_vel)
            bounced = True

        if self._y < 0:
            self._y = 0
            self._y_vel = abs(self._y_vel)
            bounced = True

        return bounced


def benchmark():
    random = Random()

    ball_count = 100
    bounces = 0
    balls = [None] * ball_count

    for i in range(ball_count):
        balls[i] = Ball(random)

    for i in range(50):
        for ball in balls:
            if ball.bounce():
                bounces += 1

    positions = 0
    for ball in balls:
        positions += ball._x + ball._y

    return bounces, positions


# One run gives the bounce count and the sum of every ball's final x and y.
def main():
    bounces, positions = benchmark()
    print(bounces, positions)
    bounces = 0
    positions = 0
    for _ in range(1000):
        b, p = benchmark()
        bounces += b
        positions += p
    print(bounces, positions)


main()
