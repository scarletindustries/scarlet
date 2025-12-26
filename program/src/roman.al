// Roman numeral converter (simplified for small numbers)

fn roman_digit(n Int, one String, five String, ten String) String {
	match n {
		0 -> '',
		1 -> one,
		2 -> one + one,
		3 -> one + one + one,
		4 -> one + five,
		5 -> five,
		6 -> five + one,
		7 -> five + one + one,
		8 -> five + one + one + one,
		9 -> one + ten,
		else -> '',
	}
}

fn to_roman(n Int) String {
	match true {
		n <= 0 -> '',
		n >= 1000 -> 'M' + to_roman(n - 1000),
		n >= 100 -> roman_digit(n / 100, 'C', 'D', 'M') + to_roman(n % 100),
		n >= 10 -> roman_digit(n / 10, 'X', 'L', 'C') + to_roman(n % 10),
		else -> roman_digit(n, 'I', 'V', 'X'),
	}
}

struct RomanNumber {
	arabic Int
	roman String
}

fn convert(n Int) RomanNumber {
	RomanNumber{
		arabic: n,
		roman: to_roman(n),
	}
}

[convert(1), convert(4), convert(9), convert(42), convert(99), convert(2024)]
