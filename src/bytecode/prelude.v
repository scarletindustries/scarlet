module bytecode

import types { IFun, IVar, Scheme }

// The prelude defines types and functions that are automatically available
// in every AL program without requiring an import.
//
// Primitive types (Int, Float, String, Bool, None) are built into the
// type system and are always available.
//
// Functions:
//   println(a) None     print any value to stdout
//   inspect(a) String   convert any value to its string representation

fn (mut c Compiler) register_prelude() {
	a := c.engine.fresh_var()
	n := c.engine.icon_none()
	s := c.engine.icon_string()
	a_id := (a as IVar).id

	c.env.define('println', Scheme{
		quantified: [a_id]
		ty:         IFun{
			params: [a]
			ret:    n
			err:    none
		}
	})

	c.env.define('inspect', Scheme{
		quantified: [a_id]
		ty:         IFun{
			params: [a]
			ret:    s
			err:    none
		}
	})
}
