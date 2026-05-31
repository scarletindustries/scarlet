// Grade calculator: turns numeric scores into letter grades and grade points,
// then prints a report line for each student.

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

fn report_line(r StudentReport) String {
	status = if r.passing { 'pass' } else { 'fail' }
	'${r.name}: score ${r.score}, grade ${r.grade}, gpa ${r.gpa}, ${status}'
}

// One report line per student, via recursion over (name, score) pairs.
fn print_reports(students Array((String, Int))) Nil {
	match students {
		[] -> Nil
		[(name, score), ..rest] -> {
			println(report_line(generate_report(name, score)))
			print_reports(rest)
		}
	}
}

print_reports([('Alice', 95), ('Dana', 83), ('Bob', 72), ('Eve', 61), ('Charlie', 55)])
