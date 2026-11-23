%% Permute: walks the permutations of a six-element array by swapping.
%% It stresses array reads and writes and deep recursion.
%% Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet), MIT licence.
-module(permute).
-export([main/0]).

%% The array is a tuple, and a write makes a new tuple. Indexes are 1-based.
swap(V, I, J) ->
    A = element(I, V),
    B = element(J, V),
    setelement(J, setelement(I, V, B), A).

permute(0, V, Count, Check) ->
    {V, Count + 1, (Check * 31 + element(1, V)) rem 1000003};
permute(N, V, Count, Check) ->
    {V1, Count1, Check1} = permute(N - 1, V, Count + 1, Check),
    swaps(N, N, V1, Count1, Check1).

swaps(_N, 0, V, Count, Check) ->
    {V, Count, Check};
swaps(N, I, V, Count, Check) ->
    {V1, Count1, Check1} = permute(N - 1, swap(V, N, I), Count, Check),
    swaps(N, I - 1, swap(V1, N, I), Count1, Check1).

run(0, Total, Check) ->
    {Total, Check};
run(Iterations, Total, Check) ->
    {_V, Count, Check1} = permute(6, {0, 1, 2, 3, 4, 5}, 0, Check),
    run(Iterations - 1, Total + Count, Check1).

main() ->
    {Total, Check} = run(200, 0, 0),
    io:format("~p~n~p~n", [Total, Check]).
