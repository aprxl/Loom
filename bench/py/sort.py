# bench/sort.loom in idiomatic Python, for comparison (bench/README.md).
from dataclasses import dataclass


@dataclass(order=True, frozen=True)
class Person:
    age: int
    id: int


size = 300_000
MASK = (1 << 64) - 1
people = []
seed = 12345
for i in range(size):
    seed = (seed * 6364136223846793005 + 1442695040888963407) & MASK
    people.append(Person((seed >> 33) % 100, i))
people.sort()
show = lambda p: f"Person(age: {p.age}, id: {p.id})"
print(show(people[0]), show(people[size // 2]), show(people[-1]))
