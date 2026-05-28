// Grade calculator with letter grades

fn score_to_grade(score Int) String {
	if score >= 90 {
		'A'
	} else if score >= 80 {
		'B'
	} else if score >= 70 {
		'C'
	} else if score >= 60 {
		'D'
	} else {
		'F'
	}
}

fn grade_points(score Int) Int {
	if score >= 90 {
		4
	} else if score >= 80 {
		3
	} else if score >= 70 {
		2
	} else if score >= 60 {
		1
	} else {
		0
	}
}

fn is_passing(score Int) Bool {
	score >= 60
}

type StudentReport {
	name String
	score Int
	grade String
	passing Bool
	gpa Int
}

fn generate_report(name String, score Int) StudentReport {
	StudentReport(
		name: name,
		score: score,
		grade: score_to_grade(score),
		passing: is_passing(score),
		gpa: grade_points(score),
	)
}

[generate_report('Alice', 95), generate_report('Bob', 72), generate_report('Charlie', 55)]
