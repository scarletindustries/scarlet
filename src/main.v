module main

import os
import cli

fn main() {
	v_version := '0.0.1'

	mut app := cli.Command{
		name: 'alc'
		description: 'al compiler and toolchain'
		execute: fn (cmd cli.Command) ! {
			println('hello app')
			return
		}
		commands: [
			cli.Command{
				name: 'doctor'
				execute: fn [v_version] (cmd cli.Command) ! {
					println('alc version 0.0.1 is running vlang ${v_version}')
					return
				}
			},
		]
	}

	app.setup()
	app.parse(os.args)
}
