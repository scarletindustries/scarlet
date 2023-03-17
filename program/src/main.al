from './math.al' import subtract, add
from 'al:date' import date

fn get_approximate_age(year_born, year_now) {
  return subtract(year_born, year_now)
}

fn print_user(name, age) {
  println('User is $name and $age years old')
}

fn print_user_strict(name: string, age: i16): void {
  print_user(name, age)
}

fn main() {
  year_born := 2004
  age := get_approximate_age(year_born, date().get_year())
  print_user_strict('alistair', age)

  if age > 18 {
    println('User is an adult')
  } else {
    println('User is a minor')
  }

  // All variables are immutable by default
  // use `mut` keyword to mark a variable as mutable
  mut name := 'alistair'
  name = 'alistair 2'
  println('User is $name and $age years old')
}
