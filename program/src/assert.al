fn check_positive_and_double(x) {
    assert x > 0, 'must be positive'
    x * 2
}

assert_pass = check_positive_and_double(5)
assert_fail = check_positive_and_double(-1) or err -> err
assert_fail_not_unwrapped = check_positive_and_double(-1)

{[assert_pass, assert_fail, assert_fail_not_unwrapped]}
