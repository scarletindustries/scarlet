/*

    This example is currently broken:

    It correctly uses all available CPU cores and schedules across them all, but it does not pre-emptively schedule.
    
    The proof is that the runs do not finish around the same time as each other, which is what should happen
    if the work is interleaved. For example here's logs of a run that proves that the program stays alive and
    the liveliness logger is continuously alive, but the actual work is not scheduled and suspended pre-emptively:

    ```
    ➜  al git:(master) ✗ al run examples/problem.al
    Alive
    1 starting
    2 starting
    3 starting
    4 starting
    6 starting
    8 starting
    9 starting
    5 starting
    10 starting
    11 starting
    13 starting
    14 starting
    15 starting
    16 starting
    17 starting
    18 starting
    20 starting
    21 starting
    22 starting
    23 starting
    12 starting
    7 starting
    24 starting
    19 starting
    28 starting
    29 starting
    25 starting
    30 starting
    26 starting
    31 starting
    27 starting
    32 starting
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    5 done - 102334155
    17 done - 102334155
    33 starting
    34 starting
    Alive
    24 done - 102334155
    29 done - 102334155
    35 starting
    36 starting
    7 done - 102334155
    28 done - 102334155
    37 starting
    38 starting
    4 done - 102334155
    15 done - 102334155
    39 starting
    40 starting
    2 done - 102334155
    12 done - 102334155
    41 starting
    42 starting
    11 done - 102334155
    23 done - 102334155
    43 starting
    44 starting
    1 done - 102334155
    13 done - 102334155
    45 starting
    46 starting
    Alive
    9 done - 102334155
    16 done - 102334155
    47 starting
    48 starting
    14 done - 102334155
    22 done - 102334155
    49 starting
    3 done - 102334155
    18 done - 102334155
    25 done - 102334155
    30 done - 102334155
    27 done - 102334155
    32 done - 102334155
    8 done - 102334155
    21 done - 102334155
    10 done - 102334155
    19 done - 102334155
    26 done - 102334155
    31 done - 102334155
    6 done - 102334155
    20 done - 102334155
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    49 done - 102334155
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    33 done - 102334155
    34 done - 102334155
    35 done - 102334155
    36 done - 102334155
    37 done - 102334155
    38 done - 102334155
    39 done - 102334155
    40 done - 102334155
    41 done - 102334155
    42 done - 102334155
    43 done - 102334155
    44 done - 102334155
    Alive
    45 done - 102334155
    46 done - 102334155
    47 done - 102334155
    48 done - 102334155
    Alive
    Alive
    Alive
    Alive
    Alive
    Alive
    ^C
    ➜  al git:(master) ✗
    ```

*/

import al/experiments/scheduler
import al/list

fn liveliness() {
	println('Alive')
	scheduler.sleep(1000)
	liveliness()
}

fn fib(n) {
	match n {
		0 -> 0
		1 -> 1
		else -> fib(n - 1) + fib(n - 2)
	}
}

fn work(i) {
	println('${i} starting')
	println('${i} done - ${fib(40)}')
}

scheduler.spawn(fn() liveliness())
list.each(1..50, fn(i) scheduler.spawn(fn() work(i)))
