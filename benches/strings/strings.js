// Strings: builds records by interpolation and concatenation, trims, splits,
// joins and replaces them, searches for substrings and turns ints into strings
// and back. It stresses the string built-ins. Written for this suite.

const ROWS = 120000;
const BATCH_SIZE = 50;
const WORDS = [
  "amber", "birch", "cedar", "delta", "ember", "flint", "grove", "heath",
  "ivory", "jade", "kelp", "lotus", "maple", "north", "olive", "pearl",
];

function next(seed) {
  return (seed * 1309 + 13849) % 65536;
}

function mix(h, v) {
  return (h * 31 + v) % 1000000007;
}

function count(flag) {
  return flag ? 1 : 0;
}

function fieldValue(field) {
  const parts = field.split("=");
  if (parts.length !== 2) return 0;
  const key = parts[0];
  const value = parts[1];
  if (key === "id" || key === "score") return parseInt(value, 10);
  return value.length;
}

function fieldText(field) {
  const parts = field.split("=");
  if (parts.length !== 2) return "";
  return parts[1];
}

function rowChecksum(h, i, a, b, c, row) {
  const padded = "  " + row + " ";
  const fields = padded.trim().split(",");
  for (const field of fields) h = mix(h, fieldValue(field));
  const rebuilt = fields.map(fieldText).join("|");
  h = mix(h, rebuilt.length);
  h = mix(h, count(row.includes("7,tag=")) + 2 * count(row.includes("ar-ma")));
  h = mix(h, row.replaceAll("=", ": ").length);
  const digits = String(a * 65536 + b) + String(c);
  const number = parseInt(digits, 10);
  return mix(mix(h, number % 1000003), i);
}

function batchChecksum(h, batch) {
  const text = batch.join("\n");
  const lines = text.split("\n");
  const pieces = text.split(",");
  let hits = 0;
  for (const line of lines) hits += count(line.includes("user=u1"));
  h = mix(h, text.length);
  h = mix(h, lines.length * 1000 + hits);
  h = mix(h, pieces.length);
  return mix(h, text.replaceAll("tag=", "#").length);
}

function main() {
  let seed = 42;
  let h = 7;
  let batch = [];
  for (let i = 0; i < ROWS; i++) {
    const a = next(seed);
    const b = next(a);
    const c = next(b);
    seed = c;
    const row = `id=${i},user=u${a % 997},score=${b},tag=${WORDS[c % 16]}-${WORDS[a % 16]}`;
    h = rowChecksum(h, i, a, b, c, row);
    batch.push(row);
    if (batch.length === BATCH_SIZE) {
      h = batchChecksum(h, batch);
      batch = [];
    }
  }
  console.log(String(h));
}

main();
