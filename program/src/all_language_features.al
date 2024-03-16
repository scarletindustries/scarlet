// Import
from './file.al' import a, b, c

// Comment

// const
const name = 'alistair'

// Struct
export struct Person {
    name: string = 'alistair',
    age: int = 19,
}

// Function
fn add(a, b) {
    return a + b
}

// Typed function
fn add_typed(a int, b int) int {
    return a + b
}

// Exported function
export fn main() {
    a().b()

    person := Person{
        name: 'not alistair',
        age: 18,
    }
}

export struct MyErrorType {
    message: string = 'something went wrong man!!!',
    lol: int,
}

fn result() int, MyErrorType {
    if random() > 0.5 {
        throw MyErrorType{
            lol: 1,
        }
    }

    return 1
}

fn handling_result_1() {
    // data would be an int here, and err is typed correctly
    data := result() or err {
        return err.message
    }
}

fn handling_result_2() void, MyErrorType {
    // Append ! to "throw" the error further up the call stack
    data := result()!
}

fn option() ?int {
    if random() > 0.5 {
        return none
    }

    return 1
}

fn option() ?int {
    if random() > 0.5 {
        return none
    }

    return 1
}

fn option_result() ?int, Error  {
    if random() > 0.5 {
        return none
    }

    if random() > 0.5 {
        throw Error{msg: 'Something went wrong'}
    }

    return 1
}

fn asdf() {
    result := option() or 10
}

fn asdf2() {
    result := option() or e -> {
        return 10
    }
}

fn asdf3() {
    result := result() or e -> {
        return 10
    }

    result := result() or {
        return 10
    }
}

fn keywords_and_punctuation() {
    if !true {
        return
    } else if false {
        return
    }

    for i in 0..10 {
        continue
    }

    // inline block expression
    example := {
        test := 20
        return test * 10
    }

    users := ['bob', 'alice', 'foo']
    for user in users {
        println(user.to_upper())
    }

    for user in ['bob', 'alice', 'foo'] {
        println(user.to_upper())
    }

    for {
        break
    }

    counter := 0
    for counter < 10 {
        counter = counter + 1
    }

    assert true, 'This is an error message'
}
