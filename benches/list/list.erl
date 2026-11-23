%% AWFY List: builds linked lists of a user-defined type and runs the recursive
%% tail (Takeuchi) comparison over them. It stresses allocation and recursion.
%% Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet, MIT licence).
-module(list).
-export([main/0]).

%% The list is our own type, not a built-in cons list: nil, or
%% {element, Val, Next}.

len(nil) -> 0;
len({element, _, Next}) -> 1 + len(Next).

make_list(0) -> nil;
make_list(Length) -> {element, Length, make_list(Length - 1)}.

is_shorter_than(_, nil) -> false;
is_shorter_than(nil, _) -> true;
is_shorter_than({element, _, XTail}, {element, _, YTail}) ->
    is_shorter_than(XTail, YTail).

next(nil) -> nil;
next({element, _, Next}) -> Next.

tail(X, Y, Z) ->
    case is_shorter_than(Y, X) of
        true ->
            tail(tail(next(X), Y, Z),
                 tail(next(Y), Z, X),
                 tail(next(Z), X, Y));
        false ->
            Z
    end.

benchmark() ->
    len(tail(make_list(15), make_list(10), make_list(6))).

run(0, Total) -> Total;
run(Iterations, Total) -> run(Iterations - 1, Total + benchmark()).

main() -> io:format("~p~n", [run(2000, 0)]).
