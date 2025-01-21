const println = console.log;
class MySubEnum {
  static D = class {};

  static D = new this.D();

  static E = class {};

  static E = new this.E();
}
class MyEnum {
  static A = class {};

  static A = new this.A();

  static B = class {};

  static B = new this.B();

  static C = class C {
    constructor(value) {
      this.value = value;
    }
  };

  static C(value) {
    return new this.C(value);
  }
}
function test(arg) {
  return (() => {
    if (arg === MyEnum.A) {
      return "a is the best!";
    } else if (arg === MyEnum.B) {
      return "b is the best!";
    } else if (arg instanceof MyEnum.C.C) {
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
