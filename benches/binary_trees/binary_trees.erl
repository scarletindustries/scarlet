%% binary-trees: builds complete binary trees and walks them to count their
%% nodes. It stresses allocating and freeing many small objects.
%% Ported from the Computer Language Benchmarks Game (BSD-3-Clause licence).

-module(binary_trees).
-export([main/0]).

-define(MIN_DEPTH, 4).

bottom_up_tree(0) -> {nil, nil};
bottom_up_tree(Depth) ->
    {bottom_up_tree(Depth - 1), bottom_up_tree(Depth - 1)}.

item_check({nil, _}) -> 1;
item_check({Left, Right}) -> 1 + item_check(Left) + item_check(Right).

check_trees(0, _Depth, Check) -> Check;
check_trees(I, Depth, Check) ->
    check_trees(I - 1, Depth, Check + item_check(bottom_up_tree(Depth))).

run_depths(Depth, MaxDepth) when Depth =< MaxDepth ->
    Iterations = 1 bsl (MaxDepth - Depth + ?MIN_DEPTH),
    Check = check_trees(Iterations, Depth, 0),
    io:format("~w\t trees of depth ~w\t check: ~w~n", [Iterations, Depth, Check]),
    run_depths(Depth + 2, MaxDepth);
run_depths(_Depth, _MaxDepth) -> ok.

main() ->
    N = 16,
    MaxDepth = max(?MIN_DEPTH + 2, N),
    StretchDepth = MaxDepth + 1,
    io:format("stretch tree of depth ~w\t check: ~w~n",
              [StretchDepth, item_check(bottom_up_tree(StretchDepth))]),
    LongLivedTree = bottom_up_tree(MaxDepth),
    run_depths(?MIN_DEPTH, MaxDepth),
    io:format("long lived tree of depth ~w\t check: ~w~n",
              [MaxDepth, item_check(LongLivedTree)]).
