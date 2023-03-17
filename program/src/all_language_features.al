// Import
from './file.al' import a, b, c

// Comment

// const
const name = 'alistair'

// comptime
const ip = comptime {
    resp := http.get('https://ifconfig.co')
    return resp.body
}

// Struct
export struct Person {
    name: string = 'alistair',
    age:  int,
}

// Function
fn add(a, b) {
    return a + b
}

// Typed function
fn add_typed(a: int, b: int) int {
    return a + b
}

// Exported function
export fn main() {
    a().b()

    person := Person{
        name: 'not alistair',
        age:  18,
    }
}

fn result() !int {
    if random() > 0.5 {
        return error 'Something went wrong'
    }

    return 1
}

fn option() ?int {
    if random() > 0.5 {
        return none
    }

    return 1
}

fn option_result() ?!int {
    if random() > 0.5 {
        return error 'Something went wrong'
    }

    if random() > 0.5 {
        return none
    }

    return 1
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

    for {
        break
    }

    counter := 0
    for counter < 10 {
        counter = counter + 1
    }

    assert true, 'This is an error message'
}
