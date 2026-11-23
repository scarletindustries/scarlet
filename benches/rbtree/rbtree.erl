%% Stresses pattern matching on nested constructors, allocation, and reuse of
%% nodes in place: insert into a functional red-black tree (Okasaki balance), then fold.
%% Ported from Koka's test/bench/koka/rbtree.kk (https://github.com/koka-lang/koka, Apache-2.0).
-module(rbtree).
-export([main/0]).

%% A node is {Color, Left, Key, Value, Right}; a leaf is the atom leaf.

is_red({red, _, _, _, _}) -> true;
is_red(_) -> false.

balance_left({_, {red, Lx, Kx, Vx, Rx}, Ky, Vy, Ry}, K, V, R) ->
    {red, {black, Lx, Kx, Vx, Rx}, Ky, Vy, {black, Ry, K, V, R}};
balance_left({_, Ly, Ky, Vy, {red, Lx, Kx, Vx, Rx}}, K, V, R) ->
    {red, {black, Ly, Ky, Vy, Lx}, Kx, Vx, {black, Rx, K, V, R}};
balance_left({_, Lx, Kx, Vx, Rx}, K, V, R) ->
    {black, {red, Lx, Kx, Vx, Rx}, K, V, R};
balance_left(leaf, _, _, _) ->
    leaf.

balance_right(L, K, V, {_, {red, Lx, Kx, Vx, Rx}, Ky, Vy, Ry}) ->
    {red, {black, L, K, V, Lx}, Kx, Vx, {black, Rx, Ky, Vy, Ry}};
balance_right(L, K, V, {_, Lx, Kx, Vx, {red, Ly, Ky, Vy, Ry}}) ->
    {red, {black, L, K, V, Lx}, Kx, Vx, {black, Ly, Ky, Vy, Ry}};
balance_right(L, K, V, {_, Lx, Kx, Vx, Rx}) ->
    {black, L, K, V, {red, Lx, Kx, Vx, Rx}};
balance_right(_, _, _, leaf) ->
    leaf.

ins({red, L, Kx, Vx, R}, K, V) ->
    if
        K < Kx -> {red, ins(L, K, V), Kx, Vx, R};
        K > Kx -> {red, L, Kx, Vx, ins(R, K, V)};
        true -> {red, L, K, V, R}
    end;
ins({black, L, Kx, Vx, R}, K, V) ->
    if
        K < Kx ->
            case is_red(L) of
                true -> balance_left(ins(L, K, V), Kx, Vx, R);
                false -> {black, ins(L, K, V), Kx, Vx, R}
            end;
        K > Kx ->
            case is_red(R) of
                true -> balance_right(L, Kx, Vx, ins(R, K, V));
                false -> {black, L, Kx, Vx, ins(R, K, V)}
            end;
        true ->
            {black, L, K, V, R}
    end;
ins(leaf, K, V) ->
    {red, leaf, K, V, leaf}.

set_black({_, L, K, V, R}) -> {black, L, K, V, R};
set_black(T) -> T.

insert(T, K, V) -> set_black(ins(T, K, V)).

fold({_, L, K, V, R}, B, F) -> fold(R, F(K, V, fold(L, B, F)), F);
fold(leaf, B, _) -> B.

height({_, L, _, _, R}) ->
    Hl = height(L),
    Hr = height(R),
    if
        Hl > Hr -> Hl + 1;
        true -> Hr + 1
    end;
height(leaf) ->
    0.

make_tree_aux(N, T) when N =< 0 -> T;
make_tree_aux(N, T) ->
    N1 = N - 1,
    make_tree_aux(N1, insert(T, N1, N1 rem 10 =:= 0)).

make_tree(N) -> make_tree_aux(N, leaf).

main() ->
    T = make_tree(300000),
    Count = fold(T, 0, fun(_K, V, R) -> if V -> R + 1; true -> R end end),
    io:format("~p~n", [Count]),
    io:format("~p~n", [fold(T, 0, fun(K, _V, R) -> (R * 31 + K) rem 1000000007 end)]),
    io:format("~p~n", [height(T)]).
