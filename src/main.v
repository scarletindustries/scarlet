module main

import os
import cli
import lib.compiler as c

fn build(cmd cli.Command) ! {
	println('hello app')
	return
}

fn main() {
	mut app := cli.Command{
		name: 'alc'
		description: 'al compiler and toolchain'
		execute: build
		commands: [
			cli.Command{
				name: 'info'
				execute: fn (cmd cli.Command) ! {
					println('alc version 0.0.1')
					println('${c.token_length} tokens')
					return
				}
			},
			cli.Command{
				name: 'build'
				execute: build
			},
		]
	}

	app.setup()
	app.parse(os.args)
}
