%% Queens from the Are We Fast Yet suite: place eight queens by backtracking over
%% boolean arrays of free rows and diagonals. Stresses array reads and writes.
%% Ported from https://github.com/smarr/are-we-fast-yet (MIT licence).
-module(queens).
-export([main/0]).

%% The board is {FreeRows, FreeMaxs, FreeMins, QueenRows}, each a tuple used as
%% an array. Rows and columns count from 1, as tuple indexes do.
queens() ->
    Board = {erlang:make_tuple(8, true), erlang:make_tuple(16, true),
             erlang:make_tuple(16, true), erlang:make_tuple(8, -1)},
    place_queen(Board, 1, 1).

%% Tries rows R to 8 for the queen in column C. Backtracking is going back to
%% the board from before the queen was placed.
place_queen(_Board, _C, 9) ->
    false;
place_queen(Board, C, R) ->
    case is_free(Board, R, C) of
        true ->
            Next = take(Board, R, C),
            if
                C =:= 8 ->
                    {true, Next};
                true ->
                    case place_queen(Next, C + 1, 1) of
                        {true, Done} -> {true, Done};
                        false -> place_queen(Board, C, R + 1)
                    end
            end;
        false ->
            place_queen(Board, C, R + 1)
    end.

is_free({Rows, Maxs, Mins, _Queens}, R, C) ->
    element(R, Rows) andalso element(C + R - 1, Maxs) andalso element(C - R + 8, Mins).

take({Rows, Maxs, Mins, Queens}, R, C) ->
    {setelement(R, Rows, false),
     setelement(C + R - 1, Maxs, false),
     setelement(C - R + 8, Mins, false),
     setelement(R, Queens, C)}.

%% The column of each row's queen, counted from zero, read as one decimal number.
digits(_Queens, 9, Acc) -> Acc;
digits(Queens, R, Acc) -> digits(Queens, R + 1, Acc * 10 + element(R, Queens) - 1).

run(0, Solved, Sum) ->
    io:format("~p~n~p~n", [Solved, Sum]);
run(N, Solved, Sum) ->
    case queens() of
        {true, {_, _, _, Queens}} -> run(N - 1, Solved + 1, Sum + digits(Queens, 1, 0));
        false -> run(N - 1, Solved, Sum)
    end.

main() -> run(3500, 0, 0).
