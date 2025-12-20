fn identity(x a) a {
    x
}

fn first(arr []a) a {
    arr[0]
}

{[
    '${identity(5)}',
    identity('hello'),
    '${first([1, 2, 3])}',
    first(['a', 'b']),
]}
