(() => {
const println = console.log;

(() => {
let users = ['bob', 'alice', 'foo'];
if ((users.length > 2)) {
println('There are more than 2 users');

} else {
println('There are less than 2 users');

}for (const i of Array.from({length: users.length - 0}, (_, i) => 0 + i)) {
println(users[i]);

}


})()
})()
