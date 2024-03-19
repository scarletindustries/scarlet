users := ['bob', 'alice', 'foo']

if users.length > 2 {
    println('There are more than 2 users')
} else {
    println('There are less than 2 users')
}

for i in 0..users.length {
    println(users[i])
}
