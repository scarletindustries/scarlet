// binary-trees: builds complete binary trees and walks them to count their
// nodes. It stresses allocating and freeing many small objects.
// Ported from the Computer Language Benchmarks Game (BSD-3-Clause licence).

class TreeNode {
  constructor(left, right) {
    this.left = left;
    this.right = right;
  }
}

function bottomUpTree(depth) {
  return depth > 0
    ? new TreeNode(bottomUpTree(depth - 1), bottomUpTree(depth - 1))
    : new TreeNode(null, null);
}

function itemCheck(tree) {
  return tree.left === null ? 1 : 1 + itemCheck(tree.left) + itemCheck(tree.right);
}

function main() {
  const n = 16;
  const minDepth = 4;
  const maxDepth = Math.max(minDepth + 2, n);

  const stretchDepth = maxDepth + 1;
  console.log(`stretch tree of depth ${stretchDepth}\t check: ${itemCheck(bottomUpTree(stretchDepth))}`);

  const longLivedTree = bottomUpTree(maxDepth);

  for (let depth = minDepth; depth <= maxDepth; depth += 2) {
    const iterations = 2 ** (maxDepth - depth + minDepth);
    let check = 0;
    for (let i = 0; i < iterations; i++) {
      check += itemCheck(bottomUpTree(depth));
    }
    console.log(`${iterations}\t trees of depth ${depth}\t check: ${check}`);
  }

  console.log(`long lived tree of depth ${maxDepth}\t check: ${itemCheck(longLivedTree)}`);
}

main();
