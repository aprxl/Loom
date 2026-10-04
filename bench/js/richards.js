// bench/richards.loom in idiomatic JavaScript, for comparison (bench/README.md).
const WORK = 1, DEVICE = 2, IDLE = 3;

class Worker {
  count = 0;
  seed = 7;
  run(p) {
    this.count++;
    this.seed = (Math.imul(this.seed, 1103515245) + 12345) >>> 0;
    if (p.kind === WORK) {
      if (p.n > 0) return { kind: WORK, n: p.n - 1, id: 0, hops: p.hops + 1 };
      return { kind: DEVICE, n: 0, id: this.seed % 4, hops: p.hops + 1 };
    }
    return null;
  }
  done() { return this.count; }
}

class Device {
  handled = 0;
  constructor(id) { this.id = id; }
  run(p) {
    this.handled++;
    if (p.hops % 7 === 0) return { kind: IDLE, n: 0, id: 0, hops: 0 };
    return { kind: WORK, n: 3, id: 0, hops: p.hops + 1 };
  }
  done() { return this.handled * 10 + this.id; }
}

class Idler {
  ticks = 0;
  run(p) {
    this.ticks++;
    return { kind: WORK, n: this.ticks % 5, id: 0, hops: p.hops };
  }
  done() { return this.ticks; }
}

const tasks = [new Worker(), new Idler(), ...[0, 1, 2, 3].map((i) => new Device(i))];
const queue = Array.from({ length: 8 }, (_, i) => ({ kind: WORK, n: i, id: 0, hops: 0 }));
const n = queue.length;
let head = 0;
for (let step = 0; step < 1_000_000; step++) {
  const p = queue[head];
  head = (head + 1) % n;
  const target = p.kind === WORK ? 0 : p.kind === IDLE ? 1 : 2 + p.id;
  const next = tasks[target].run(p);
  if (next) queue[(head + n - 1) % n] = next;
}
let total = 0;
for (const t of tasks) total = (total + t.done()) >>> 0;
console.log(total);
