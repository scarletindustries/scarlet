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
