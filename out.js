
      const println = console.log;
      class MySubEnum {

  static D_class = class D {};

  static D = new MySubEnum.D_class();


  static E_class = class E {};

  static E = new MySubEnum.E_class();

}
class MyEnum {

  static A_class = class A {};

  static A = new MyEnum.A_class();


  static B_class = class B {};

  static B = new MyEnum.B_class();


  static C_class = class {
    constructor(value) {
      this.value = value;
    }
  };

  static C(value) {
    return new MyEnum.C_class(value);
  }

}
function test(arg) {
  return (() => {
  if (arg === MyEnum.A) {
  return 'a is the best!';
} else if (arg === MyEnum.B) {
  return 'b is the best!';
} else if (arg instanceof MyEnum.C_class) {
  const sub = arg.value;
  return (() => {
  if (sub === MySubEnum.D) {
  return 'd is the best!';
} else if (sub === MySubEnum.E) {
  return 'e is the worst!';
}
  throw new Error("No match case found");
})();
}
  throw new Error("No match case found");
})();
}
println(test(MyEnum.C(MySubEnum.E)));
    
