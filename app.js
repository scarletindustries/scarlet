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

  console.log(MyEnum);

  function test(arg) {
    (() => {
      let result = (() => {
        if (arg instanceof MyEnum.A) {
          return "a";
        } else if (arg instanceof MyEnum.B) {
          return "b";
        } else if (arg instanceof MyEnum.C) {
          const sub = arg.value;
          return (() => {
            if (sub instanceof MySubEnum.D) {
              return "a";
            } else if (sub instanceof MySubEnum.E) {
              return "b";
            }
            throw new Error("No match case found");
          })();
        }
        throw new Error("No match case found");
      })();
      println(result);
    })();
  }

  test(MyEnum.A);
})();
