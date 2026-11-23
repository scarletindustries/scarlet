%% cfold: build an expression tree, reassociate it and fold its constants, many times over.
%% It stresses allocating small nodes of a sum type and matching on them, with deep recursion.
%% Ported from test/bench/koka/cfold.kk in Koka's benchmarks (Apache-2.0), itself from Lean 4's const_fold.
-module(cfold).
-export([main/0]).

mk_expr(0, 0) -> {var, 1};
mk_expr(0, V) -> {val, V};
mk_expr(N, 0) -> {add, mk_expr(N - 1, 1), mk_expr(N - 1, 0)};
mk_expr(N, V) -> {add, mk_expr(N - 1, V + 1), mk_expr(N - 1, V - 1)}.

append_add({add, E1, E2}, E3) -> {add, E1, append_add(E2, E3)};
append_add(E0, E3) -> {add, E0, E3}.

append_mul({mul, E1, E2}, E3) -> {mul, E1, append_mul(E2, E3)};
append_mul(E0, E3) -> {mul, E0, E3}.

reassoc({add, E1, E2}) -> append_add(reassoc(E1), reassoc(E2));
reassoc({mul, E1, E2}) -> append_mul(reassoc(E1), reassoc(E2));
reassoc(E) -> E.

cfold({add, E1, E2}) ->
    F1 = cfold(E1),
    F2 = cfold(E2),
    case {F1, F2} of
        {{val, A}, {val, B}} -> {val, A + B};
        {{val, A}, {add, F, {val, B}}} -> {add, {val, A + B}, F};
        {{val, A}, {add, {val, B}, F}} -> {add, {val, A + B}, F};
        _ -> {add, F1, F2}
    end;
cfold({mul, E1, E2}) ->
    F1 = cfold(E1),
    F2 = cfold(E2),
    case {F1, F2} of
        {{val, A}, {val, B}} -> {val, A * B};
        {{val, A}, {mul, F, {val, B}}} -> {mul, {val, A * B}, F};
        {{val, A}, {mul, {val, B}, F}} -> {mul, {val, A * B}, F};
        _ -> {mul, F1, F2}
    end;
cfold(E) -> E.

eval({var, _}) -> 0;
eval({val, V}) -> V;
eval({add, L, R}) -> eval(L) + eval(R);
eval({mul, L, R}) -> eval(L) * eval(R).

%% An order-sensitive hash of the tree, so the output pins down the folded tree's
%% shape and not only its value, which folding leaves unchanged.
shape({var, N}) -> 3 * N + 1;
shape({val, V}) -> 3 * V + 2;
shape({add, L, R}) -> (31 * shape(L) + 17 * shape(R) + 3) rem 1000003;
shape({mul, L, R}) -> (37 * shape(L) + 19 * shape(R) + 5) rem 1000003.

%% The tree is rebuilt every round, from a start value that walks 0 to 7, so some
%% trees are mostly Var leaves and some mostly Val. Depth 12 is as deep as it goes
%% because the reassociated tree is one chain 4096 long, and LuaJIT's stack ends near 10000.
rounds(N, N, Before, After, Shapes) -> {Before, After, Shapes};
rounds(I, N, Before, After, Shapes) ->
    E = mk_expr(12, I rem 8),
    F = cfold(reassoc(E)),
    rounds(I + 1, N, Before + eval(E), After + eval(F), Shapes + shape(F)).

main() ->
    {Before, After, Shapes} = rounds(0, 700, 0, 0, 0),
    io:format("~p~n~p~n~p~n", [Before, After, Shapes]).
