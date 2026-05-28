// Age calculator and life stage classifier

fn classify_age(age Int) String {
	if age < 2 {
		'Infant'
	} else if age < 13 {
		'Child'
	} else if age < 20 {
		'Teenager'
	} else if age < 30 {
		'YoungAdult'
	} else if age < 65 {
		'Adult'
	} else {
		'Senior'
	}
}

fn can_vote(age Int) Bool {
	age >= 18
}

fn can_drive(age Int) Bool {
	age >= 16
}

fn years_until(current Int, target Int) Int {
	if current >= target {
		0
	} else {
		target - current
	}
}

type Person {
	name String
	age Int
}

type AgeReport {
	name String
	age Int
	stage String
	can_vote Bool
	can_drive Bool
	years_to_retire Int
}

fn analyze_person(person Person) AgeReport {
	AgeReport(
		name: person.name,
		age: person.age,
		stage: classify_age(person.age),
		can_vote: can_vote(person.age),
		can_drive: can_drive(person.age),
		years_to_retire: years_until(person.age, 65),
	)
}

println(analyze_person(Person(name: 'Timmy', age: 1)))
println(analyze_person(Person(name: 'Charlie', age: 10)))
println(analyze_person(Person(name: 'Thomas', age: 16)))
println(analyze_person(Person(name: 'Martin', age: 35)))
println(analyze_person(Person(name: 'Edward', age: 70)))
