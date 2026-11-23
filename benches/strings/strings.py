# Strings: builds records by interpolation and concatenation, trims, splits,
# joins and replaces them, searches for substrings and turns ints into strings
# and back. It stresses the string built-ins. Written for this suite.

ROWS = 120000
BATCH_SIZE = 50
WORDS = [
    "amber", "birch", "cedar", "delta", "ember", "flint", "grove", "heath",
    "ivory", "jade", "kelp", "lotus", "maple", "north", "olive", "pearl",
]


def next_seed(seed):
    return (seed * 1309 + 13849) % 65536


def mix(h, v):
    return (h * 31 + v) % 1000000007


def count(flag):
    return 1 if flag else 0


def field_value(field):
    parts = field.split("=")
    if len(parts) != 2:
        return 0
    key, value = parts
    if key == "id" or key == "score":
        return int(value)
    return len(value)


def field_text(field):
    parts = field.split("=")
    if len(parts) != 2:
        return ""
    return parts[1]


def row_checksum(h, i, a, b, c, row):
    padded = "  " + row + " "
    fields = padded.strip().split(",")
    for field in fields:
        h = mix(h, field_value(field))
    rebuilt = "|".join([field_text(field) for field in fields])
    h = mix(h, len(rebuilt))
    h = mix(h, count("7,tag=" in row) + 2 * count("ar-ma" in row))
    h = mix(h, len(row.replace("=", ": ")))
    digits = str(a * 65536 + b) + str(c)
    number = int(digits)
    return mix(mix(h, number % 1000003), i)


def batch_checksum(h, batch):
    text = "\n".join(batch)
    lines = text.split("\n")
    pieces = text.split(",")
    hits = 0
    for line in lines:
        hits += count("user=u1" in line)
    h = mix(h, len(text))
    h = mix(h, len(lines) * 1000 + hits)
    h = mix(h, len(pieces))
    return mix(h, len(text.replace("tag=", "#")))


def main():
    seed = 42
    h = 7
    batch = []
    for i in range(ROWS):
        a = next_seed(seed)
        b = next_seed(a)
        c = next_seed(b)
        seed = c
        row = f"id={i},user=u{a % 997},score={b},tag={WORDS[c % 16]}-{WORDS[a % 16]}"
        h = row_checksum(h, i, a, b, c, row)
        batch.append(row)
        if len(batch) == BATCH_SIZE:
            h = batch_checksum(h, batch)
            batch = []
    print(h)


main()
