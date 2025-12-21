// Age calculator and life stage classifier

fn classify_age(age Int) String {
	match true {
		age < 2 -> 'Infant',
		age < 13 -> 'Child',
		age < 20 -> 'Teenager',
		age < 30 -> 'YoungAdult',
		age < 65 -> 'Adult',
		else -> 'Senior',
	}
}

fn can_vote(age Int) Bool {
	age >= 18
}

fn can_drive(age Int) Bool {
	age >= 16
}

fn years_until(current Int, target Int) Int {
	match true {
		current >= target -> 0,
		else -> target - current,
	}
}

struct Person {
	name String,
	age Int,
}

struct AgeReport {
	name String,
	age Int,
	stage String,
	can_vote Bool,
	can_drive Bool,
	years_to_retire Int,
}

fn analyze_person(person Person) AgeReport {
	AgeReport{
		name: person.name,
		age: person.age,
		stage: classify_age(person.age),
		can_vote: can_vote(person.age),
		can_drive: can_drive(person.age),
		years_to_retire: years_until(person.age, 65),
	}
}

baby = Person{ name: 'Baby', age: 1 }
kid = Person{ name: 'Kid', age: 10 }
teen = Person{ name: 'Teen', age: 16 }
adult = Person{ name: 'Adult', age: 35 }
retiree = Person{ name: 'Retiree', age: 70 }

[analyze_person(baby), analyze_person(kid), analyze_person(teen), analyze_person(adult), analyze_person(retiree)]
