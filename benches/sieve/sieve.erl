%% sieve: the sieve of Eratosthenes over an array of 5000 flags, run many times.
%% It stresses array writes in a tight loop. Ported from the Are We Fast Yet
%% suite (https://github.com/smarr/are-we-fast-yet, MIT licence).
-module(sieve).
-export([main/0]).

make_flags(0, Flags) -> Flags;
make_flags(N, Flags) -> make_flags(N - 1, array:set(array:size(Flags), true, Flags)).

strike(Flags, K, Step, Size) when K =< Size ->
    strike(array:set(K - 1, false, Flags), K + Step, Step, Size);
strike(Flags, _K, _Step, _Size) -> Flags.

sieve(Flags, I, Size, Count) when I =< Size ->
    case array:get(I - 1, Flags) of
        true -> sieve(strike(Flags, I + I, I, Size), I + 1, Size, Count + 1);
        false -> sieve(Flags, I + 1, Size, Count)
    end;
sieve(_Flags, _I, _Size, Count) -> Count.

repeat(0, Total) -> Total;
repeat(N, Total) -> repeat(N - 1, Total + sieve(make_flags(5000, array:new()), 2, 5000, 0)).

main() -> io:format("~p~n", [repeat(45, 0)]).
