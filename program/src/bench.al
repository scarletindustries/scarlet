enum MySubEnum {
    D,
    E,
}

enum MyEnum {
    A,
    B,
    C(MySubEnum),
}

fn test(arg MyEnum) {
    result := match arg {
        MyEnum.A => 'a',
        MyEnum.B => 'b',
        MyEnum.C(sub) => match sub {
            MySubEnum.D => 'a',
            MySubEnum.E => 'b',
        },
    }

    println(result)
}

test(MyEnum.A)