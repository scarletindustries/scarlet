%% deriv: differentiate x^x symbolically nine times over, simplifying as it goes, and do that twice.
%% It stresses allocating small nodes of a sum type and matching on pairs of them.
%% Ported from test/bench/koka/deriv.kk in Koka's benchmarks (Apache-2.0), itself from Lean 4's deriv.ml.
-module(deriv).
-export([main/0]).

pown(_, B, Acc) when B =< 0 -> Acc;
pown(A, B, Acc) -> pown(A, B - 1, Acc * A).

add({val, N}, {val, M}) -> {val, N + M};
add({val, 0}, F) -> F;
add(F, {val, 0}) -> F;
add(F, {val, N}) -> add({val, N}, F);
add({val, N}, {add, {val, M}, F}) -> add({val, N + M}, F);
add(F, {add, {val, N}, G}) -> add({val, N}, add(F, G));
add({add, F, G}, H) -> add(F, add(G, H));
add(F, G) -> {add, F, G}.

mul({val, N}, {val, M}) -> {val, N * M};
mul({val, 0}, _) -> {val, 0};
mul(_, {val, 0}) -> {val, 0};
mul({val, 1}, F) -> F;
mul(F, {val, 1}) -> F;
mul(F, {val, N}) -> mul({val, N}, F);
mul({val, N}, {mul, {val, M}, F}) -> mul({val, N * M}, F);
mul(F, {mul, {val, N}, G}) -> mul({val, N}, mul(F, G));
mul({mul, F, G}, H) -> mul(F, mul(G, H));
mul(F, G) -> {mul, F, G}.

powr({val, M}, {val, N}) -> {val, pown(M, N, 1)};
powr(_, {val, 0}) -> {val, 1};
powr(F, {val, 1}) -> F;
powr({val, 0}, _) -> {val, 0};
powr(F, G) -> {pow, F, G}.

ln({val, 1}) -> {val, 0};
ln(F) -> {ln, F}.

d(_, {val, _}) -> {val, 0};
d(X, {var, Y}) when X =:= Y -> {val, 1};
d(_, {var, _}) -> {val, 0};
d(X, {add, F, G}) -> add(d(X, F), d(X, G));
d(X, {mul, F, G}) -> add(mul(F, d(X, G)), mul(G, d(X, F)));
d(X, {pow, F, G}) ->
    mul(
        powr(F, G),
        add(mul(mul(G, d(X, F)), powr(F, {val, -1})), mul(ln(F), d(X, G)))
    );
d(X, {ln, F}) -> mul(d(X, F), powr(F, {val, -1})).

count({val, _}) -> 1;
count({var, _}) -> 1;
count({add, F, G}) -> count(F) + count(G);
count({mul, F, G}) -> count(F) + count(G);
count({pow, F, G}) -> count(F) + count(G);
count({ln, F}) -> count(F).

nest(N, N, F) ->
    F;
nest(I, N, F) ->
    D = d(<<"x">>, F),
    io:format("~p count: ~p~n", [I + 1, count(D)]),
    nest(I + 1, N, D).

main() ->
    X = {var, <<"x">>},
    nest(0, 9, powr(X, X)),
    nest(0, 9, powr(X, X)),
    io:format("done~n").
