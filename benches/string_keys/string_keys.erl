%% Counts how often each word turns up in a stream of short words, in a map
%% keyed by binary string, then looks a second stream up in it. It stresses
%% building short strings, hashing them and maps keyed by strings. Written for
%% this suite.
-module(string_keys).
-export([main/0]).

next(Seed) -> Seed * 16807 rem 2147483647.

spell(_R, 0, Acc) -> Acc;
spell(R, N, Acc) ->
    Letter = element(R rem 6 + 1, {$a, $e, $i, $n, $r, $t}),
    spell(R div 6, N - 1, <<Acc/binary, Letter>>).

%% A word of 2 to `Lengths + 1` letters, all of it decided by `R`.
word(R, Lengths) -> spell(R div Lengths, 2 + R rem Lengths, <<>>).

count(M, _Seed, 0) -> M;
count(M, Seed, I) ->
    S = next(Seed),
    W = word(S, 5),
    count(M#{W => maps:get(W, M, 0) + 1}, S, I - 1).

%% The second stream has longer words in it too, so some lookups miss.
probe(_M, _Seed, 0, Check) -> Check;
probe(M, Seed, I, Check) ->
    S = next(Seed),
    N = maps:get(word(S, 6), M, 0),
    probe(M, S, I - 1, (Check * 31 + N) rem 1000000007).

main() ->
    M = count(#{}, 42, 1000000),
    Squares = maps:fold(fun(_K, N, Acc) -> Acc + N * N end, 0, M),
    io:format("~p~n~p~n~p~n", [maps:size(M), Squares, probe(M, 2024, 1000000, 0)]).
