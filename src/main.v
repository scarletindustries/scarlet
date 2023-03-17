module main

import os
import cli
import lib.compiler

fn main() {
	mut app := cli.Command{
		description: 'al compiler and toolchain'
		version: '0.0.1'
		posix_mode: true
		execute: fn (cmd cli.Command) ! {
			println(cmd.help_message())
		}
		commands: [
			cli.Command{
				name: 'build'
				args: ['entrypoint']
				execute: fn (cmd cli.Command) ! {
					entrypoint := '/Users/ali/code/al/program/src/main.al'

					mut scanner := compiler.new_scanner(os.read_file(entrypoint)!)

					for {
						t := scanner.scan_next()
						println(t)

						if t.kind == .eof {
							break
						}
					}
				}
			},
		]
	}

	app.setup()
	app.parse(os.args)
}
