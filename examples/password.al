// Password strength checker. Each password is tested against four rules and
// the verdict comes from how many rules pass.

import al/array
import al/string

type Rule {
	name String
	passed Bool
}

type Report {
	password String
	rules Array(Rule)
	score Int
	strength String
}

const digits = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9']
const symbols = ['!', '@', '#', '\$', '%', '&', '*', '-', '_', '?']

// True when the password contains at least one of the given characters.
fn contains_any(password String, chars Array(String)) Bool {
	array.any(chars, fn(c) string.contains(password, c))
}

fn check_rules(password String) Array(Rule) {
	[
		Rule(name: 'at least 8 characters', passed: string.length(password) >= 8),
		Rule(name: 'at least 12 characters', passed: string.length(password) >= 12),
		Rule(name: 'contains a digit', passed: contains_any(password, digits)),
		Rule(name: 'contains a symbol', passed: contains_any(password, symbols)),
	]
}

fn count_passed(rules Array(Rule)) Int {
	array.length(array.filter(rules, fn(rule) rule.passed))
}

fn strength(score Int) String {
	match score {
		0 | 1 -> 'weak'
		2 -> 'fair'
		3 -> 'good'
		else -> 'strong'
	}
}

fn analyze(password String) Report {
	rules = check_rules(password)
	score = count_passed(rules)
	Report(password: password, rules: rules, score: score, strength: strength(score))
}

fn print_rules(rules Array(Rule)) Nil {
	match rules {
		[] -> Nil
		[rule, ..rest] -> {
			mark = if rule.passed { 'pass' } else { 'fail' }
			println('  ${mark}  ${rule.name}')
			print_rules(rest)
		}
	}
}

// One verdict block per password.
fn print_report(password String) Nil {
	report = analyze(password)
	println('password: ${report.password}')
	print_rules(report.rules)
	println('  verdict: ${report.strength} (${report.score}/${array.length(report.rules)} rules)')
	println('')
}

print_report('cat')
print_report('hunter2')
print_report('blue-whale')
print_report('tomato-42')
print_report('starlight-2024')
