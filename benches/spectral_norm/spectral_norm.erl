%% spectral-norm: the power method on an implicit matrix, 10 rounds. It stresses
%% float arithmetic over arrays. Ported from the Computer Language Benchmarks
%% Game (benchmarksgame-team.pages.debian.net/benchmarksgame, BSD-3 licence).
-module(spectral_norm).
-export([main/0]).

%% Entry (I, J) of the infinite matrix, counting from zero.
a(I, J) -> 1.0 / ((I + J) * (I + J + 1) div 2 + I + 1).

%% Vectors are tuples, read by index and built a whole vector at a time.
av_row(X, I, J, N, Sum) when J < N ->
    av_row(X, I, J + 1, N, Sum + element(J + 1, X) * a(I, J));
av_row(_, _, _, _, Sum) -> Sum.

av(X, I, N, Acc) when I < N -> av(X, I + 1, N, [av_row(X, I, 0, N, 0.0) | Acc]);
av(_, _, _, Acc) -> list_to_tuple(lists:reverse(Acc)).

atv_row(X, I, J, N, Sum) when J < N ->
    atv_row(X, I, J + 1, N, Sum + element(J + 1, X) * a(J, I));
atv_row(_, _, _, _, Sum) -> Sum.

atv(X, I, N, Acc) when I < N -> atv(X, I + 1, N, [atv_row(X, I, 0, N, 0.0) | Acc]);
atv(_, _, _, Acc) -> list_to_tuple(lists:reverse(Acc)).

atav(X, N) -> atv(av(X, 0, N, []), 0, N, []).

power(0, U, V, _) -> {U, V};
power(K, U, _, N) ->
    V = atav(U, N),
    power(K - 1, atav(V, N), V, N).

dot(X, Y, I, N, Sum) when I < N ->
    dot(X, Y, I + 1, N, Sum + element(I + 1, X) * element(I + 1, Y));
dot(_, _, _, _, Sum) -> Sum.

%% The same 30 Newton steps in every language, as Scarlet has no square root.
sqrt(_, X, 0) -> X;
sqrt(S, X, K) -> sqrt(S, 0.5 * (X + S / X), K - 1).

%% Prints the norm, scaled to an integer, and then floor(u.v). The second number
%% depends on every round, where the norm alone has converged after three.
spectral_norm(N) ->
    {U, V} = power(10, erlang:make_tuple(N, 1.0), {}, N),
    VBv = dot(U, V, 0, N, 0.0),
    Vv = dot(V, V, 0, N, 0.0),
    io:format("~p~n~p~n", [floor(sqrt(VBv / Vv, 1.0, 30) * 1000000000.0 + 0.5), floor(VBv)]).

main() ->
    spectral_norm(100),
    spectral_norm(700).
