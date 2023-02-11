import os
import cli
import lib.compiler as c

fn main() {
	mut app := cli.Command{
		name: 'alc'
		description: 'al compiler and toolchain'
		execute: fn (cmd cli.Command) ! {
			println('hello app')
			return
		}
		commands: [
			cli.Command{
				name: 'info'
				execute: fn (cmd cli.Command) ! {
					println('alc version 0.0.1')
					println('i currently understand ${c.token_length} tokens')
					return
				}
			},
		]
	}

	app.setup()
	app.parse(os.args)
}
