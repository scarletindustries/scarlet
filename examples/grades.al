// Grade calculator with letter grades

fn score_to_grade(score Int) String {
	match true {
		score >= 90 -> 'A',
		score >= 80 -> 'B',
		score >= 70 -> 'C',
		score >= 60 -> 'D',
		else -> 'F',
	}
}

fn grade_points(score Int) Int {
	match true {
		score >= 90 -> 4,
		score >= 80 -> 3,
		score >= 70 -> 2,
		score >= 60 -> 1,
		else -> 0,
	}
}

fn is_passing(score Int) Bool { score >= 60 }

struct StudentReport {
	name String
	score Int
	grade String
	passing Bool
	gpa Int
}

fn generate_report(name String, score Int) StudentReport {
	StudentReport{
		name: name,
		score: score,
		grade: score_to_grade(score),
		passing: is_passing(score),
		gpa: grade_points(score),
	}
}

[generate_report('Alice', 95), generate_report('Bob', 72), generate_report('Charlie', 55)]
