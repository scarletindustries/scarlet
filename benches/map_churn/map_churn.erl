%% Stresses the built-in map: insert scattered int keys, look them all up
%% several times (with a miss probe beside each hit), then delete the oldest
%% key and insert a new one in a long churn loop.
%% Written for this suite, starting from examples/bench_map.sca.
-module(map_churn).
-export([main/0]).

%% A full-period generator, so the keys never repeat. Its products stay far
%% below 2^53, which keeps Lua and JavaScript numbers exact.
next(X) -> (X * 69069 + 1) rem 4294967296.

fill(M, X, N, N) -> {M, X};
fill(M, X, I, N) ->
    K = next(X),
    fill(maps:put(K, I, M), K, I + 1, N).

lookups(_M, _X, 0, Acc) -> Acc;
lookups(M, X, I, Acc) ->
    K = next(X),
    Hit = case maps:find(K, M) of
        {ok, V} -> (Acc * 31 + V) rem 1000000007;
        error -> Acc
    end,
    Probed = case maps:find(K + 1, M) of
        {ok, W} -> Hit + W;
        error -> Hit
    end,
    lookups(M, K, I - 1, Probed).

rounds(_M, _X, _N, 0, Acc) -> Acc;
rounds(M, X, N, R, Acc) -> rounds(M, X, N, R - 1, lookups(M, X, N, Acc)).

%% The map is a sliding window over the key sequence: Trail names the oldest
%% key, which goes, and Lead the next new one, which comes in.
churn(M, _Lead, Trail, Steps, Steps) -> {M, Trail};
churn(M, Lead, Trail, I, Steps) ->
    T = next(Trail),
    L = next(Lead),
    churn(maps:put(L, I, maps:remove(T, M)), L, T, I + 1, Steps).

main() ->
    N = 200000,
    Seed = 42,
    {M, Lead} = fill(#{}, Seed, 0, N),
    io:format("~p~n", [rounds(M, Seed, N, 10, 0)]),
    {Churned, Trail} = churn(M, Lead, Seed, 0, 3000000),
    %% The window's keys must all be there and the first N, long deleted, must
    %% all be gone: a hit in the second pass would change the sum.
    Window = lookups(Churned, Trail, N, 0),
    io:format("~p~n", [lookups(Churned, Seed, N, Window)]).
