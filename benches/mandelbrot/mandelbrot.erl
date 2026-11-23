%% Mandelbrot: the bit-packed mandelbrot checksum. It stresses float arithmetic
%% and integer bit operations in nested loops.
%% Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet, MIT licence),
%% which took it from the Computer Language Benchmarks Game (revised BSD).
-module(mandelbrot).
-export([main/0]).

%% 1 when the point leaves the circle of radius 2 within 50 steps, 0 otherwise.
escape(Z, Zrzr, Zizi, Zi, Cr, Ci) when Z < 50 ->
    Zr = Zrzr - Zizi + Cr,
    Zi2 = 2.0 * Zr * Zi + Ci,
    Zrzr2 = Zr * Zr,
    Zizi2 = Zi2 * Zi2,
    if
        Zrzr2 + Zizi2 > 4.0 -> 1;
        true -> escape(Z + 1, Zrzr2, Zizi2, Zi2, Cr, Ci)
    end;
escape(_, _, _, _, _, _) ->
    0.

row(X, Size, Ci, Sum, ByteAcc, BitNum) when X < Size ->
    Cr = 2.0 * X / Size - 1.5,
    Acc = ByteAcc * 2 + escape(0, 0.0, 0.0, 0.0, Cr, Ci),
    Bits = BitNum + 1,
    if
        Bits =:= 8 ->
            row(X + 1, Size, Ci, Sum bxor Acc, 0, 0);
        X =:= Size - 1 ->
            row(X + 1, Size, Ci, Sum bxor (Acc bsl (8 - Bits)), 0, 0);
        true ->
            row(X + 1, Size, Ci, Sum, Acc, Bits)
    end;
row(_, _, _, Sum, _, _) ->
    Sum.

rows(Y, Size, Sum) when Y < Size ->
    Ci = 2.0 * Y / Size - 1.0,
    rows(Y + 1, Size, row(0, Size, Ci, Sum, 0, 0));
rows(_, _, Sum) ->
    Sum.

mandelbrot(Size) -> rows(0, Size, 0).

main() ->
    io:format("~p~n", [mandelbrot(1)]),
    io:format("~p~n", [mandelbrot(500)]),
    io:format("~p~n", [mandelbrot(750)]).
