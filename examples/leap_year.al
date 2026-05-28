// Leap year checker

fn is_leap_year(year Int) Bool {
	if year % 400 == 0 {
		True
	} else if year % 100 == 0 {
		False
	} else if year % 4 == 0 {
		True
	} else {
		False
	}
}

fn days_in_year(year Int) Int {
	if is_leap_year(year) { 366 } else { 365 }
}

fn days_in_february(year Int) Int {
	if is_leap_year(year) { 29 } else { 28 }
}

type YearInfo {
	year Int
	is_leap Bool
	days Int
	feb_days Int
}

fn analyze_year(year Int) YearInfo {
	YearInfo(
		year: year,
		is_leap: is_leap_year(year),
		days: days_in_year(year),
		feb_days: days_in_february(year),
	)
}

[analyze_year(2000), analyze_year(2024), analyze_year(2023), analyze_year(1900), analyze_year(2100)]
