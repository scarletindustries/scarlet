// Password strength checker (simplified)

fn check_strength(length Int) String {
	match true {
		length >= 16 -> 'Strong',
		length >= 12 -> 'Good',
		length >= 8 -> 'Fair',
		else -> 'Weak',
	}
}

fn is_long_enough(length Int) Bool { length >= 8 }

struct PasswordResult {
	length Int,
	is_long_enough Bool,
	strength String,
}

fn analyze(length Int) PasswordResult {
	PasswordResult{
		length: length,
		is_long_enough: is_long_enough(length),
		strength: check_strength(length),
	}
}

// Test with different password lengths
[analyze(3), analyze(8), analyze(12), analyze(20)]
