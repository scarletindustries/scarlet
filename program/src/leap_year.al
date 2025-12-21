// Leap year checker

fn is_leap_year(year Int) Bool {
	match true {
		year % 400 == 0 -> true,
		year % 100 == 0 -> false,
		year % 4 == 0 -> true,
		else -> false,
	}
}

fn days_in_year(year Int) Int {
	match is_leap_year(year) {
		true -> 366,
		false -> 365,
	}
}

fn days_in_february(year Int) Int {
	match is_leap_year(year) {
		true -> 29,
		false -> 28,
	}
}

struct YearInfo {
	year Int,
	is_leap Bool,
	days Int,
	feb_days Int,
}

fn analyze_year(year Int) YearInfo {
	YearInfo{
		year: year,
		is_leap: is_leap_year(year),
		days: days_in_year(year),
		feb_days: days_in_february(year),
	}
}

[analyze_year(2000), analyze_year(2024), analyze_year(2023), analyze_year(1900), analyze_year(2100)]
