-module(fib).
-export([main/0]).

fib(N) when N < 2 -> N;
fib(N) -> fib(N - 1) + fib(N - 2).

main() -> io:format("~p~n", [fib(35)]).
