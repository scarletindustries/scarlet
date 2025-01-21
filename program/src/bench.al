export enum MySubEnum {
    D,
    E,
}

export enum MyEnum {
    A,
    B,
    C(MySubEnum),
}

export fn test(arg MyEnum) {
    return match arg {
        MyEnum.A => 'a is the best!',
        MyEnum.B => 'b is the best!',
        MyEnum.C(sub) => match sub {
            MySubEnum.D => 'd is the best!',
            MySubEnum.E => 'e is the best!',
        },
    }
}

println(test(MyEnum.C(MySubEnum.D)))