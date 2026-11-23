%% NBody: five bodies (the sun and four planets) pull on each other for many
%% small time steps. It stresses float arithmetic and record fields. Ported from
%% the Are We Fast Yet suite (MIT licence), which took it from the Computer
%% Language Benchmarks Game. The square root is Newton's method written out, the
%% same in every language, because Scarlet has none.
-module(nbody).
-export([main/0]).

-define(PI, 3.141592653589793).
-define(SOLAR_MASS, (4.0 * ?PI * ?PI)).
-define(DAYS_PER_YEAR, 365.24).
-define(STEPS, 100000).

-record(body, {x, y, z, vx, vy, vz, mass}).

body(X, Y, Z, Vx, Vy, Vz, Mass) ->
    #body{x = X, y = Y, z = Z,
          vx = Vx * ?DAYS_PER_YEAR,
          vy = Vy * ?DAYS_PER_YEAR,
          vz = Vz * ?DAYS_PER_YEAR,
          mass = Mass * ?SOLAR_MASS}.

sqrt(D) when D > 1.0 -> sqrt(D, D);
sqrt(D) -> sqrt(D, 1.0).

sqrt(D, Guess) ->
    Next = 0.5 * (Guess + D / Guess),
    if
        Next >= Guess -> Guess;
        true -> sqrt(D, Next)
    end.

momentum([], Px, Py, Pz) ->
    {Px, Py, Pz};
momentum([#body{vx = Vx, vy = Vy, vz = Vz, mass = Mass} | Rest], Px, Py, Pz) ->
    momentum(Rest, Px + Vx * Mass, Py + Vy * Mass, Pz + Vz * Mass).

create_bodies() ->
    Sun = body(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0),
    Planets = [
        body(4.84143144246472090,
             -1.16032004402742839,
             -0.103622044471123109,
             0.00166007664274403694,
             0.00769901118419740425,
             -0.0000690460016972063023,
             0.000954791938424326609),
        body(8.34336671824457987,
             4.12479856412430479,
             -0.403523417114321381,
             -0.00276742510726862411,
             0.00499852801234917238,
             0.0000230417297573763929,
             0.000285885980666130812),
        body(12.8943695621391310,
             -15.1111514016986312,
             -0.223307578892655734,
             0.00296460137564761618,
             0.00237847173959480950,
             -0.0000296589568540237556,
             0.0000436624404335156298),
        body(15.3796971148509165,
             -25.9193146099879641,
             0.179258772950371181,
             0.00268067772490389322,
             0.00162824170038242295,
             -0.0000951592254519715870,
             0.0000515138902046611451)
    ],
    {Px, Py, Pz} = momentum([Sun | Planets], 0.0, 0.0, 0.0),
    [Sun#body{vx = 0.0 - Px / ?SOLAR_MASS,
              vy = 0.0 - Py / ?SOLAR_MASS,
              vz = 0.0 - Pz / ?SOLAR_MASS} | Planets].

%% Lets B and every body after it change each other's velocity.
pull(B, [], _Dt) ->
    {B, []};
pull(B, [O | Rest], Dt) ->
    Dx = B#body.x - O#body.x,
    Dy = B#body.y - O#body.y,
    Dz = B#body.z - O#body.z,
    DSquared = Dx * Dx + Dy * Dy + Dz * Dz,
    Distance = sqrt(DSquared),
    Mag = Dt / (DSquared * Distance),
    Pulled = B#body{vx = B#body.vx - Dx * O#body.mass * Mag,
                    vy = B#body.vy - Dy * O#body.mass * Mag,
                    vz = B#body.vz - Dz * O#body.mass * Mag},
    Other = O#body{vx = O#body.vx + Dx * B#body.mass * Mag,
                   vy = O#body.vy + Dy * B#body.mass * Mag,
                   vz = O#body.vz + Dz * B#body.mass * Mag},
    {Done, OthersDone} = pull(Pulled, Rest, Dt),
    {Done, [Other | OthersDone]}.

pull_all([], _Dt) ->
    [];
pull_all([B | Rest], Dt) ->
    {Done, Others} = pull(B, Rest, Dt),
    [Done | pull_all(Others, Dt)].

advance(Bodies, Dt) ->
    [B#body{x = B#body.x + Dt * B#body.vx,
            y = B#body.y + Dt * B#body.vy,
            z = B#body.z + Dt * B#body.vz} || B <- pull_all(Bodies, Dt)].

potential(_B, [], E) ->
    E;
potential(B, [O | Rest], E) ->
    Dx = B#body.x - O#body.x,
    Dy = B#body.y - O#body.y,
    Dz = B#body.z - O#body.z,
    Distance = sqrt(Dx * Dx + Dy * Dy + Dz * Dz),
    potential(B, Rest, E - B#body.mass * O#body.mass / Distance).

energy([], E) ->
    E;
energy([B | Rest], E) ->
    Kinetic = E + 0.5 * B#body.mass * (B#body.vx * B#body.vx +
                                       B#body.vy * B#body.vy +
                                       B#body.vz * B#body.vz),
    energy(Rest, potential(B, Rest, Kinetic)).

run(Bodies, 0) -> Bodies;
run(Bodies, Steps) -> run(advance(Bodies, 0.01), Steps - 1).

%% The energy as a whole number, so every language prints the same text.
scaled(E) -> trunc(E * 1000000000000000.0).

main() ->
    Bodies = create_bodies(),
    io:format("~p~n", [scaled(energy(Bodies, 0.0))]),
    io:format("~p~n", [scaled(energy(run(Bodies, ?STEPS), 0.0))]).
