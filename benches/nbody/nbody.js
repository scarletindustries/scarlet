// NBody: five bodies (the sun and four planets) pull on each other for many
// small time steps. It stresses float arithmetic and record fields. Ported from
// the Are We Fast Yet suite (MIT licence), which took it from the Computer
// Language Benchmarks Game. The square root is Newton's method written out, the
// same in every language, because Scarlet has none.
'use strict';

const PI = 3.141592653589793;
const SOLAR_MASS = 4.0 * PI * PI;
const DAYS_PER_YEAR = 365.24;
const STEPS = 100000;

class Body {
  constructor(x, y, z, vx, vy, vz, mass) {
    this.x = x;
    this.y = y;
    this.z = z;
    this.vx = vx * DAYS_PER_YEAR;
    this.vy = vy * DAYS_PER_YEAR;
    this.vz = vz * DAYS_PER_YEAR;
    this.mass = mass * SOLAR_MASS;
  }
}

function sqrt(d) {
  let guess = d > 1.0 ? d : 1.0;
  for (;;) {
    const next = 0.5 * (guess + d / guess);
    if (next >= guess) { return guess; }
    guess = next;
  }
}

function createBodies() {
  const bodies = [
    new Body(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0),
    new Body(
      4.84143144246472090,
      -1.16032004402742839,
      -0.103622044471123109,
      0.00166007664274403694,
      0.00769901118419740425,
      -0.0000690460016972063023,
      0.000954791938424326609
    ),
    new Body(
      8.34336671824457987,
      4.12479856412430479,
      -0.403523417114321381,
      -0.00276742510726862411,
      0.00499852801234917238,
      0.0000230417297573763929,
      0.000285885980666130812
    ),
    new Body(
      12.8943695621391310,
      -15.1111514016986312,
      -0.223307578892655734,
      0.00296460137564761618,
      0.00237847173959480950,
      -0.0000296589568540237556,
      0.0000436624404335156298
    ),
    new Body(
      15.3796971148509165,
      -25.9193146099879641,
      0.179258772950371181,
      0.00268067772490389322,
      0.00162824170038242295,
      -0.0000951592254519715870,
      0.0000515138902046611451
    )
  ];
  let px = 0.0;
  let py = 0.0;
  let pz = 0.0;
  for (let i = 0; i < bodies.length; i += 1) {
    const b = bodies[i];
    px += b.vx * b.mass;
    py += b.vy * b.mass;
    pz += b.vz * b.mass;
  }
  const sun = bodies[0];
  sun.vx = 0.0 - px / SOLAR_MASS;
  sun.vy = 0.0 - py / SOLAR_MASS;
  sun.vz = 0.0 - pz / SOLAR_MASS;
  return bodies;
}

function advance(bodies, dt) {
  for (let i = 0; i < bodies.length; i += 1) {
    const iBody = bodies[i];

    for (let j = i + 1; j < bodies.length; j += 1) {
      const jBody = bodies[j];
      const dx = iBody.x - jBody.x;
      const dy = iBody.y - jBody.y;
      const dz = iBody.z - jBody.z;

      const dSquared = dx * dx + dy * dy + dz * dz;
      const distance = sqrt(dSquared);
      const mag = dt / (dSquared * distance);

      iBody.vx = iBody.vx - dx * jBody.mass * mag;
      iBody.vy = iBody.vy - dy * jBody.mass * mag;
      iBody.vz = iBody.vz - dz * jBody.mass * mag;

      jBody.vx = jBody.vx + dx * iBody.mass * mag;
      jBody.vy = jBody.vy + dy * iBody.mass * mag;
      jBody.vz = jBody.vz + dz * iBody.mass * mag;
    }
  }

  for (let i = 0; i < bodies.length; i += 1) {
    const b = bodies[i];
    b.x = b.x + dt * b.vx;
    b.y = b.y + dt * b.vy;
    b.z = b.z + dt * b.vz;
  }
}

function energy(bodies) {
  let e = 0.0;

  for (let i = 0; i < bodies.length; i += 1) {
    const iBody = bodies[i];

    e += 0.5 * iBody.mass * (iBody.vx * iBody.vx
                           + iBody.vy * iBody.vy
                           + iBody.vz * iBody.vz);

    for (let j = i + 1; j < bodies.length; j += 1) {
      const jBody = bodies[j];
      const dx = iBody.x - jBody.x;
      const dy = iBody.y - jBody.y;
      const dz = iBody.z - jBody.z;

      const distance = sqrt(dx * dx + dy * dy + dz * dz);
      e -= iBody.mass * jBody.mass / distance;
    }
  }
  return e;
}

// The energy as a whole number, so every language prints the same text.
function scaled(e) {
  return Math.trunc(e * 1000000000000000.0);
}

const bodies = createBodies();
console.log(scaled(energy(bodies)));
for (let i = 0; i < STEPS; i += 1) {
  advance(bodies, 0.01);
}
console.log(scaled(energy(bodies)));
