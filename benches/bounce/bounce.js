// Bounce: 100 balls bounce around a box for 50 steps. We count the bounces and
// add up where the balls end.
// It stresses record allocation, field reads and small-integer arithmetic.
// Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet, MIT licence).
'use strict';

// The suite's own generator.
class Random {
  constructor() {
    this.seed = 74755;
  }

  next() {
    this.seed = (this.seed * 1309 + 13849) & 65535;
    return this.seed;
  }
}

class Ball {
  constructor(random) {
    this.x = random.next() % 500;
    this.y = random.next() % 500;
    this.xVel = (random.next() % 300) - 150;
    this.yVel = (random.next() % 300) - 150;
  }

  bounce() {
    const xLimit = 500;
    const yLimit = 500;
    let bounced = false;

    this.x += this.xVel;
    this.y += this.yVel;

    if (this.x > xLimit) {
      this.x = xLimit; this.xVel = 0 - Math.abs(this.xVel); bounced = true;
    }

    if (this.x < 0) {
      this.x = 0; this.xVel = Math.abs(this.xVel); bounced = true;
    }

    if (this.y > yLimit) {
      this.y = yLimit; this.yVel = 0 - Math.abs(this.yVel); bounced = true;
    }

    if (this.y < 0) {
      this.y = 0; this.yVel = Math.abs(this.yVel); bounced = true;
    }

    return bounced;
  }
}

function benchmark() {
  const random = new Random();
  const ballCount = 100;
  let bounces = 0;
  const balls = new Array(ballCount);
  let i = 0;

  for (i = 0; i < ballCount; i += 1) {
    balls[i] = new Ball(random);
  }

  for (i = 0; i < 50; i += 1) {
    for (const ball of balls) {
      if (ball.bounce()) {
        bounces += 1;
      }
    }
  }

  let positions = 0;
  for (const ball of balls) {
    positions += ball.x + ball.y;
  }
  return [bounces, positions];
}

// One run gives the bounce count and the sum of every ball's final x and y.
let [bounces, positions] = benchmark();
console.log(`${bounces} ${positions}`);
bounces = 0;
positions = 0;
for (let n = 0; n < 1000; n += 1) {
  const [b, p] = benchmark();
  bounces += b;
  positions += p;
}
console.log(`${bounces} ${positions}`);
