import al/http

match http.serve('0.0.0.0', 8080, fn(_req) http.text('Hello from al/http!')) {
	Ok(_) -> println('Listening on http://localhost:8080')
	Err(e) -> println('serve failed: ${e}')
}
