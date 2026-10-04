// bench/nbody.loom in idiomatic JavaScript, for comparison (bench/README.md).
const PI = 3.141592653589793;
const SOLAR_MASS = 4 * PI * PI;
const DAYS = 365.24;

class Body {
  constructor(x, y, z, vx, vy, vz, mass) {
    Object.assign(this, { x, y, z, vx, vy, vz, mass });
  }
}

const planets = () => [
  new Body(0, 0, 0, 0, 0, 0, SOLAR_MASS),
  new Body(4.84143144246472090e+00, -1.16032004402742839e+00, -1.03622044471123109e-01,
    1.66007664274403694e-03 * DAYS, 7.69901118419740425e-03 * DAYS, -6.90460016972063023e-05 * DAYS,
    9.54791938424326609e-04 * SOLAR_MASS),
  new Body(8.34336671824457987e+00, 4.12479856412430479e+00, -4.03523417114321381e-01,
    -2.76742510726862411e-03 * DAYS, 4.99852801234917238e-03 * DAYS, 2.30417297573763929e-05 * DAYS,
    2.85885980666130812e-04 * SOLAR_MASS),
  new Body(1.28943695621391310e+01, -1.51111514016986312e+01, -2.23307578892655734e-01,
    2.96460137564761618e-03 * DAYS, 2.37847173959480950e-03 * DAYS, -2.96589568540237556e-05 * DAYS,
    4.36624404335156298e-05 * SOLAR_MASS),
  new Body(1.53796971148509165e+01, -2.59193146099879641e+01, 1.79258772950371181e-01,
    2.68067772490389322e-03 * DAYS, 1.62824170038242295e-03 * DAYS, -9.51592254519715870e-05 * DAYS,
    5.15138902046611451e-05 * SOLAR_MASS),
];

function offsetMomentum(bodies) {
  let px = 0, py = 0, pz = 0;
  for (const b of bodies) {
    px += b.vx * b.mass;
    py += b.vy * b.mass;
    pz += b.vz * b.mass;
  }
  bodies[0].vx = -px / SOLAR_MASS;
  bodies[0].vy = -py / SOLAR_MASS;
  bodies[0].vz = -pz / SOLAR_MASS;
}

function energy(bodies) {
  let e = 0;
  for (let i = 0; i < bodies.length; i++) {
    const b = bodies[i];
    e += 0.5 * b.mass * (b.vx * b.vx + b.vy * b.vy + b.vz * b.vz);
    for (let j = i + 1; j < bodies.length; j++) {
      const c = bodies[j];
      const dx = b.x - c.x, dy = b.y - c.y, dz = b.z - c.z;
      e -= (b.mass * c.mass) / Math.sqrt(dx * dx + dy * dy + dz * dz);
    }
  }
  return e;
}

function advance(bodies, dt) {
  const n = bodies.length;
  for (let i = 0; i < n; i++) {
    const bi = bodies[i];
    for (let j = i + 1; j < n; j++) {
      const bj = bodies[j];
      const dx = bi.x - bj.x, dy = bi.y - bj.y, dz = bi.z - bj.z;
      const d2 = dx * dx + dy * dy + dz * dz;
      const mag = dt / (d2 * Math.sqrt(d2));
      const mi = bi.mass * mag, mj = bj.mass * mag;
      bi.vx -= dx * mj; bi.vy -= dy * mj; bi.vz -= dz * mj;
      bj.vx += dx * mi; bj.vy += dy * mi; bj.vz += dz * mi;
    }
  }
  for (const b of bodies) {
    b.x += dt * b.vx; b.y += dt * b.vy; b.z += dt * b.vz;
  }
}

const bodies = planets();
offsetMomentum(bodies);
console.log(energy(bodies).toFixed(9));
for (let i = 0; i < 500_000; i++) advance(bodies, 0.01);
console.log(energy(bodies).toFixed(9));
