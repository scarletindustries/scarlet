(() => {
  const println = console.log;

  class MySubEnum_D {
    constructor() {}
  }

  class MySubEnum_E {
    constructor() {}
  }

  class MySubEnum {
    static D = new MySubEnum_D();
    static E = new MySubEnum_E();
  }
  class MyEnum_A {
    constructor() {}
  }

  class MyEnum_B {
    constructor() {}
  }

  class MyEnum_C {
    constructor(value) {
      this.value = value;
    }
  }

  class MyEnum {
    static A = new MyEnum_A();
    static B = new MyEnum_B();
    static C(value) {
      return new MyEnum_C(value);
    }
  }
  function test(arg) {
    return (() => {
      if (arg === MyEnum.A) {
        return "a is the best!";
      } else if (arg === MyEnum.B) {
        return "b is the best!";
      } else if (arg && arg.__kind === "C") {
        const sub = arg.value;
        return (() => {
          if (sub === MySubEnum.D) {
            return "d is the best!";
          } else if (sub === MySubEnum.E) {
            return "e is the best!";
          }
          throw new Error("No match case found");
        })();
      }
      throw new Error("No match case found");
    })();
  }
  println(test(MyEnum.C(MySubEnum.D)));
})();
