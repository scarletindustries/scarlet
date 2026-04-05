module bytecode

import type_def { Type }
import types { IFun, mono }

// Builtins are VM intrinsics that provide low-level functionality beyond
// the prelude. These will eventually move behind standard library imports
// once the module system is fully implemented.

fn (mut c Compiler) register_builtins() {
	c.env.register_struct('Socket', [], map[string]Type{})

	socket := c.engine.icon('Socket', [])
	i := c.engine.icon_int()
	s := c.engine.icon_string()
	n := c.engine.icon_none()

	c.env.define('__stack_depth__', mono(IFun{ params: [], ret: i, err: none }))

	c.env.define('read_file', mono(IFun{ params: [s], ret: s, err: s }))
	c.env.define('write_file', mono(IFun{ params: [s, s], ret: n, err: s }))

	c.env.define('str_split', mono(IFun{ params: [s, s], ret: c.engine.icon_array(s), err: none }))

	c.env.define('tcp_listen', mono(IFun{ params: [i], ret: socket, err: s }))
	c.env.define('tcp_accept', mono(IFun{ params: [socket], ret: socket, err: s }))
	c.env.define('tcp_read', mono(IFun{ params: [socket], ret: s, err: s }))
	c.env.define('tcp_write', mono(IFun{ params: [socket, s], ret: n, err: s }))
	c.env.define('tcp_close', mono(IFun{ params: [socket], ret: n, err: s }))
}
