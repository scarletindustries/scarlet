<img width="128" src="https://github.com/scarletindustries.png" />

### Scarlet programming language

A statically typed, expression-oriented programming language. It is not finished, and the syntax and standard library still change between releases.

[Documentation](https://scarlet.industries/docs/language)

---

```scarlet
import scarlet/array

type Shape {
	Circle(r Float)
	Rect(w Float, h Float)
}

fn area(s Shape) Float {
	match s {
		Circle(r) -> 3.14159 * r * r
		Rect(w, h) -> w * h
	}
}

pub fn main() {
	shapes = [Circle(r: 2.0), Rect(w: 3.0, h: 4.0), Circle(r: 1.0)]
	println('total area: ${array::fold(shapes, 0.0, fn(acc, s) acc + area(s))}')
}
```

```sh
curl -fsSL scarlet.industries/install.sh | bash
```

```
scarlet run <file.sca>         Run a program
scarlet repl                    Start an interactive REPL
scarlet check <file.sca>       Type-check without running
scarlet fmt [path]              Format source files
```
