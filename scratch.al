import al/io.{NotFound}

io.read_text('./does-not-exist.txt') or err -> {
	match err {
		NotFound(path) -> println('couldnt find ${path}')
		else -> println(err)
	}

	'<empty>'
}
