// Run with --experimental-shitty-io

server = tcp_listen(8080)
println('Listening on http://localhost:8080')

loop = fn() {
	conn = tcp_accept(server)
	// request = tcp_read(conn)

	body = 'Hello from AL!'
	response = 'HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\n${body}'

	tcp_write(conn, response)
	tcp_close(conn)

	loop()
}

loop()
