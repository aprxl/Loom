# bench/richards.loom in idiomatic Python, for comparison (bench/README.md).
WORK, DEVICE, IDLE = 1, 2, 3


class Packet:
    __slots__ = ("kind", "n", "id", "hops")

    def __init__(self, kind, hops, n=0, id=0):
        self.kind, self.hops, self.n, self.id = kind, hops, n, id


class Worker:
    def __init__(self):
        self.count, self.seed = 0, 7

    def run(self, p):
        self.count += 1
        self.seed = (self.seed * 1103515245 + 12345) & 0xFFFFFFFF
        if p.kind == WORK:
            if p.n > 0:
                return Packet(WORK, p.hops + 1, n=p.n - 1)
            return Packet(DEVICE, p.hops + 1, id=self.seed % 4)
        return None

    def done(self):
        return self.count


class Device:
    def __init__(self, id):
        self.id, self.handled = id, 0

    def run(self, p):
        self.handled += 1
        if p.hops % 7 == 0:
            return Packet(IDLE, 0)
        return Packet(WORK, p.hops + 1, n=3)

    def done(self):
        return self.handled * 10 + self.id


class Idler:
    def __init__(self):
        self.ticks = 0

    def run(self, p):
        self.ticks += 1
        return Packet(WORK, p.hops, n=self.ticks % 5)

    def done(self):
        return self.ticks


tasks = [Worker(), Idler()] + [Device(i) for i in range(4)]
queue = [Packet(WORK, 0, n=i) for i in range(8)]
head, n = 0, len(queue)
for _ in range(1_000_000):
    p = queue[head]
    head = (head + 1) % n
    target = 0 if p.kind == WORK else 1 if p.kind == IDLE else 2 + p.id
    nxt = tasks[target].run(p)
    if nxt is not None:
        queue[(head - 1) % n] = nxt
total = 0
for t in tasks:
    total = (total + t.done()) & 0xFFFFFFFF
print(total)
