%% fannkuch-redux: flips the front of every permutation of 1..N until 1 leads,
%% and keeps a checksum and the longest run of flips. It stresses small-array
%% reads, writes and reversal. Ported from Mike Pall's Lua program in the Computer
%% Language Benchmarks Game (benchmarksgame-team.pages.debian.net, BSD-3 licence).
-module(fannkuch).
-export([main/0]).

-define(N, 9).

%% Swaps Q[I] and Q[J] and walks inwards until the two meet.
reverse(Q, I, J) when I < J ->
    A = element(I, Q),
    B = element(J, Q),
    reverse(setelement(J, setelement(I, Q, B), A), I + 1, J - 1);
reverse(Q, _I, _J) -> Q.

%% The first element lives in Q1, not in Q, so slot 1 of Q is never read.
flip(Q, Q1, Flips) ->
    case element(Q1, Q) of
        1 -> Flips;
        QQ -> flip(reverse(setelement(Q1, Q, Q1), 2, Q1 - 1), QQ, Flips + 1)
    end.

%% Moves P[J + 1] down to P[J] for J = 1..I, then puts T in slot I + 1.
rotate(P, J, I, T) when J =< I -> rotate(setelement(J, P, element(J + 1, P)), J + 1, I, T);
rotate(P, _J, I, T) -> setelement(I + 1, P, T).

swap(P, I, J) ->
    A = element(I, P),
    setelement(J, setelement(I, P, element(J, P)), A).

advance(P, S, I, N) ->
    case element(I, S) of
        1 when I =:= N -> done;
        1 -> advance(rotate(P, 1, I, element(1, P)), setelement(I, S, I), I + 1, N);
        Sx -> {P, setelement(I, S, Sx - 1)}
    end.

run(P, S, Sign, Sum, MaxFlips, N) ->
    Flips = case element(1, P) of
        1 -> 0;
        Q1 -> flip(P, Q1, 1)
    end,
    Sum2 = Sum + Sign * Flips,
    Max2 = if Flips > MaxFlips -> Flips; true -> MaxFlips end,
    case Sign of
        1 -> run(swap(P, 1, 2), S, -1, Sum2, Max2, N);
        _ ->
            case advance(swap(P, 2, 3), S, 3, N) of
                done -> {Sum2, Max2};
                {P2, S2} -> run(P2, S2, 1, Sum2, Max2, N)
            end
    end.

main() ->
    Start = list_to_tuple(lists:seq(1, ?N)),
    {Sum, Flips} = run(Start, Start, 1, 0, 0, ?N),
    io:format("~p~nPfannkuchen(~p) = ~p~n", [Sum, ?N, Flips]).
