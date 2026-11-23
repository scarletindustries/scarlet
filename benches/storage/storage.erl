%% Storage builds a tree of nested arrays, four children to a node and seven
%% levels deep, over and over. It stresses allocation and freeing.
%% Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet), MIT licence.
-module(storage).
-export([main/0]).

next(Seed) -> (Seed * 1309 + 13849) rem 65536.

%% Returns {Tree, Seed, Count, Slots}. A node is a tuple of its children.
build(1, Seed, Count, Slots) ->
    S = next(Seed),
    N = S rem 10 + 1,
    {erlang:make_tuple(N, nil), S, Count + 1, Slots + N};
build(Depth, Seed, Count, Slots) ->
    {A, S1, C1, L1} = build(Depth - 1, Seed, Count + 1, Slots),
    {B, S2, C2, L2} = build(Depth - 1, S1, C1, L1),
    {C, S3, C3, L3} = build(Depth - 1, S2, C2, L2),
    {D, S4, C4, L4} = build(Depth - 1, S3, C3, L3),
    {{A, B, C, D}, S4, C4, L4}.

repeat(0, Count, Slots) ->
    {Count, Slots};
repeat(Rounds, Count, Slots) ->
    {_Tree, _Seed, C, L} = build(7, 74755, 0, 0),
    repeat(Rounds - 1, Count + C, Slots + L).

main() ->
    {Count, Slots} = repeat(500, 0, 0),
    io:format("~p~n~p~n", [Count, Slots]).
