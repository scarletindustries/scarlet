import al/http

http.serve('0.0.0.0', 8080, fn(_req) http.text('Hello from al/http!')) or e -> println(
	'serve failed: ${e}',
)
