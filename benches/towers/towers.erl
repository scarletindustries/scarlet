%% Towers of Hanoi with explicit disk stacks: 13 disks moved between three piles,
%% many times over. It stresses small record updates and linked-list pushes and pops.
%% Ported from the Are We Fast Yet suite (github.com/smarr/are-we-fast-yet, MIT licence).
-module(towers).
-export([main/0]).

%% piles is a tuple of three piles, and a pile is a list of disk sizes, top first.
-record(towers, {piles, moves = 0}).

push_disk(T = #towers{piles = Piles}, Size, Pile) ->
    case element(Pile, Piles) of
        [Top | _] when Size >= Top ->
            erlang:error("Cannot put a big disk on a smaller one");
        Rest ->
            T#towers{piles = setelement(Pile, Piles, [Size | Rest])}
    end.

move_top_disk(T = #towers{piles = Piles}, From, To) ->
    case element(From, Piles) of
        [] ->
            erlang:error("Attempting to remove a disk from an empty pile");
        [Size | Next] ->
            Popped = T#towers{piles = setelement(From, Piles, Next)},
            Pushed = push_disk(Popped, Size, To),
            Pushed#towers{moves = Pushed#towers.moves + 1}
    end.

build_tower_at(T, _Pile, I) when I < 0 -> T;
build_tower_at(T, Pile, I) -> build_tower_at(push_disk(T, I, Pile), Pile, I - 1).

move_disks(T, 1, From, To) ->
    move_top_disk(T, From, To);
move_disks(T, Disks, From, To) ->
    Other = 6 - From - To,
    T1 = move_disks(T, Disks - 1, From, Other),
    T2 = move_top_disk(T1, From, To),
    move_disks(T2, Disks - 1, Other, To).

weigh([], _Depth, Acc) -> Acc;
weigh([Size | Next], Depth, Acc) -> weigh(Next, Depth + 1, Acc + Size * Depth).

benchmark() ->
    Built = build_tower_at(#towers{piles = {[], [], []}}, 1, 13),
    move_disks(Built, 13, 1, 2).

run(0, Moves, Weight) ->
    io:format("~p~n~p~n", [Moves, Weight]);
run(N, Moves, Weight) ->
    #towers{piles = {A, B, _}, moves = M} = benchmark(),
    run(N - 1, Moves + M, Weight + weigh(B, 1, 0) + weigh(A, 1, 0)).

main() -> run(600, 0, 0).
