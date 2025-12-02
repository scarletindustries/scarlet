enum MySubEnum {
    D,
    E,
}

enum MyEnum {
    A,
    B,
    C(MySubEnum),
}

fn test(arg MyEnum) String {
    match arg {
        MyEnum.A => 'a is the best!',
        MyEnum.B => 'b is the best!',
        MyEnum.C(sub) => match sub {
            MySubEnum.D => 'd is the best!',
            MySubEnum.E => 'e is the worst!',
        },
    }
}

println(test(MyEnum.C(MySubEnum.D)))
println(test(MyEnum.A))
println(test(MyEnum.C(MySubEnum.E)))
