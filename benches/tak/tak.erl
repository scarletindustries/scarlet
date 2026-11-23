%% tak: the Takeuchi function. Deep non-tail recursion on small ints, so it
%% stresses function calls and nothing else. Classic benchmark (Takeuchi 1978,
%% in the Gabriel suite); written from the definition, not ported.
-module(tak).
-export([main/0]).

tak(X, Y, Z) when Y < X ->
    tak(tak(X - 1, Y, Z), tak(Y - 1, Z, X), tak(Z - 1, X, Y));
tak(_X, _Y, Z) ->
    Z.

main() ->
    io:format("~p~n", [tak(18, 12, 6)]),
    io:format("~p~n", [tak(28, 18, 8)]).
