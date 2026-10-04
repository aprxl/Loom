# bench/mandelbrot.loom in idiomatic Python, for comparison (bench/README.md).
def escape(cx, cy, limit):
    x = y = 0.0
    i = 0
    while x * x + y * y <= 4.0 and i < limit:
        x, y = x * x - y * y + cx, 2.0 * x * y + cy
        i += 1
    return i


size = 400
total = 0
for row in range(size):
    cy = row / size * 2.4 - 1.2
    for col in range(size):
        cx = col / size * 3.5 - 2.5
        total += escape(cx, cy, 200)
print(total)
