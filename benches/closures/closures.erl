%% closures: map, filter and fold over a 1000-element list with funs that
%% capture variables, a composed pipeline of small functions, and a counter
%% whose step function returns the next counter. It stresses making funs
%% and calling them. Written for this suite, not a port.
-module(closures).
-export([main/0]).

adder(K) -> fun(X) -> X + K end.

scaler(K, M) -> fun(X) -> X * K rem M end.

compose(F, G) -> fun(X) -> G(F(X)) end.

counter(Value, Step) ->
    {Value, fun(D) -> counter((Value + Step * D) rem 1000003, Step + 1) end}.

count({Value, _}, 0) -> Value;
count({_, Next}, N) -> count(Next(N rem 5 + 1), N - 1).

rounds(_, 0, Acc) -> Acc;
rounds(Xs, R, Acc) ->
    K = R rem 7,
    Scaled = lists:map(fun(X) -> (X * R + Acc) rem 1009 end, Xs),
    Kept = lists:filter(fun(X) -> X rem 7 =/= K end, Scaled),
    Direct = lists:foldl(fun(X, S) -> (S + X * R) rem 1000003 end, 0, Kept),
    Steps = [adder(R), scaler(3, 1013), adder(Acc rem 13), scaler(K + 2, 1019)],
    Pipeline = lists:foldl(fun(G, F) -> compose(F, G) end, fun(X) -> X end, Steps),
    Piped = lists:foldl(fun(X, S) -> (S + X) rem 1000003 end, 0, lists:map(Pipeline, Kept)),
    Counted = count(counter(Acc rem 97, R), 200),
    rounds(Xs, R - 1, (Acc * 31 + Direct + Piped + Counted) rem 1000003).

main() -> io:format("~p~n", [rounds(lists:seq(0, 999), 1500, 1)]).
