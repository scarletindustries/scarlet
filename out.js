(() => {
  const println = console.log;

  (() => {
    const source = "println('hello')";
    class Token {
      type = undefined;

      value = undefined;
    }

    /** @returns {boolean} */ function isLetter(c) {
      let isLetterLower = c >= "a" && c <= "z";
      let isLetterUpper = c >= "A" && c <= "Z";
      return isLetterLower || isLetterUpper;
    }

    /** @returns {boolean} */ function isDigit(c) {
      return c >= "0" && c <= "9";
    }

    /** @returns {Token} */ function lex(input) {}

    class Node {}

    /** @returns {Node} */ function parse(tokens) {}

    /** @returns {string} */ function generate(node) {}

    /*exported*/ /** @returns {string} */ function compile(input) {
      let tokens = lex(input);
      let ast = parse(tokens);
      let output = generate(ast);
      return output;
    }

    /** @returns {void} */ function main() {
      let result = compile(source);
    }
  })();
})();
