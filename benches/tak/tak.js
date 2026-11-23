// tak: the Takeuchi function. Deep non-tail recursion on small ints, so it
// stresses function calls and nothing else. Classic benchmark (Takeuchi 1978,
// in the Gabriel suite); written from the definition, not ported.

function tak(x, y, z) {
  if (y < x) {
    return tak(tak(x - 1, y, z), tak(y - 1, z, x), tak(z - 1, x, y));
  }
  return z;
}

console.log(tak(18, 12, 6));
console.log(tak(28, 18, 8));
