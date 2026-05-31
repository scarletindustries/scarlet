import al/http.{Request, Response}

fn handler(_req Request) Response {
	http.text('Hello from al/http!')
}

http.serve('0.0.0.0', 8080, handler) or e -> println('serve failed: ${e}')
