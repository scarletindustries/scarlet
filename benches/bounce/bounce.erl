%% Bounce: 100 balls bounce around a box for 50 steps. We count the bounces and
%% add up where the balls end.
%% It stresses record allocation, field reads and small-integer arithmetic.
%% Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet, MIT licence).
-module(bounce).
-export([main/0]).

-record(ball, {x, y, x_vel, y_vel}).

%% The suite's own generator. The seed is threaded through by hand.
next(Seed) -> (Seed * 1309 + 13849) band 65535.

make_balls(0, _Seed) ->
    [];
make_balls(Count, Seed) ->
    S1 = next(Seed),
    S2 = next(S1),
    S3 = next(S2),
    S4 = next(S3),
    Ball = #ball{x = S1 rem 500, y = S2 rem 500, x_vel = S3 rem 300 - 150, y_vel = S4 rem 300 - 150},
    [Ball | make_balls(Count - 1, S4)].

%% Moves a ball one step. The boolean says whether it hit a wall.
bounce(#ball{x = X0, y = Y0, x_vel = XVel0, y_vel = YVel0}) ->
    X1 = X0 + XVel0,
    Y1 = Y0 + YVel0,
    {X, XVel, HitX} =
        if
            X1 > 500 -> {500, 0 - abs(XVel0), true};
            X1 < 0 -> {0, abs(XVel0), true};
            true -> {X1, XVel0, false}
        end,
    {Y, YVel, HitY} =
        if
            Y1 > 500 -> {500, 0 - abs(YVel0), true};
            Y1 < 0 -> {0, abs(YVel0), true};
            true -> {Y1, YVel0, false}
        end,
    {#ball{x = X, y = Y, x_vel = XVel, y_vel = YVel}, HitX orelse HitY}.

step([], Moved, Bounces) ->
    {lists:reverse(Moved), Bounces};
step([Ball0 | Rest], Moved, Bounces) ->
    case bounce(Ball0) of
        {Ball, true} -> step(Rest, [Ball | Moved], Bounces + 1);
        {Ball, false} -> step(Rest, [Ball | Moved], Bounces)
    end.

run(0, Balls, Bounces) ->
    {Bounces, lists:foldl(fun(#ball{x = X, y = Y}, Sum) -> Sum + X + Y end, 0, Balls)};
run(Steps, Balls0, Bounces0) ->
    {Balls, Bounces} = step(Balls0, [], Bounces0),
    run(Steps - 1, Balls, Bounces).

%% One run gives the bounce count and the sum of every ball's final x and y.
benchmark() -> run(50, make_balls(100, 74755), 0).

repeat(0, Bounces, Positions) ->
    {Bounces, Positions};
repeat(Times, Bounces, Positions) ->
    {B, P} = benchmark(),
    repeat(Times - 1, Bounces + B, Positions + P).

main() ->
    {Bounces, Positions} = benchmark(),
    io:format("~b ~b~n", [Bounces, Positions]),
    {Bounces1000, Positions1000} = repeat(1000, 0, 0),
    io:format("~b ~b~n", [Bounces1000, Positions1000]).
