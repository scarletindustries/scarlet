%% Strings: builds records by interpolation and concatenation, trims, splits,
%% joins and replaces them, searches for substrings and turns ints into strings
%% and back. It stresses the string built-ins. Written for this suite.
-module(strings).
-export([main/0]).

-define(ROWS, 120000).
-define(BATCH_SIZE, 50).

words() ->
    {<<"amber">>, <<"birch">>, <<"cedar">>, <<"delta">>, <<"ember">>, <<"flint">>,
     <<"grove">>, <<"heath">>, <<"ivory">>, <<"jade">>, <<"kelp">>, <<"lotus">>,
     <<"maple">>, <<"north">>, <<"olive">>, <<"pearl">>}.

next(Seed) -> (Seed * 1309 + 13849) rem 65536.

mix(H, V) -> (H * 31 + V) rem 1000000007.

count(true) -> 1;
count(false) -> 0.

contains(S, Needle) -> binary:match(S, Needle) =/= nomatch.

join(Parts, Sep) -> iolist_to_binary(lists:join(Sep, Parts)).

field_value(Field) ->
    case binary:split(Field, <<"=">>, [global]) of
        [Key, Value] when Key =:= <<"id">>; Key =:= <<"score">> -> binary_to_integer(Value);
        [_, Value] -> byte_size(Value);
        _ -> 0
    end.

field_text(Field) ->
    case binary:split(Field, <<"=">>, [global]) of
        [_, Value] -> Value;
        _ -> <<>>
    end.

row_checksum(H, I, A, B, C, Row) ->
    Padded = <<"  ", Row/binary, " ">>,
    Fields = binary:split(string:trim(Padded), <<",">>, [global]),
    H1 = lists:foldl(fun(Field, Acc) -> mix(Acc, field_value(Field)) end, H, Fields),
    Rebuilt = join([field_text(Field) || Field <- Fields], <<"|">>),
    H2 = mix(H1, byte_size(Rebuilt)),
    H3 = mix(H2, count(contains(Row, <<"7,tag=">>)) + 2 * count(contains(Row, <<"ar-ma">>))),
    H4 = mix(H3, byte_size(binary:replace(Row, <<"=">>, <<": ">>, [global]))),
    Digits = <<(integer_to_binary(A * 65536 + B))/binary, (integer_to_binary(C))/binary>>,
    Number = binary_to_integer(Digits),
    mix(mix(H4, Number rem 1000003), I).

batch_checksum(H, Batch) ->
    Text = join(Batch, <<"\n">>),
    Lines = binary:split(Text, <<"\n">>, [global]),
    Pieces = binary:split(Text, <<",">>, [global]),
    Hits = lists:foldl(fun(Line, Acc) -> Acc + count(contains(Line, <<"user=u1">>)) end, 0, Lines),
    H1 = mix(H, byte_size(Text)),
    H2 = mix(H1, length(Lines) * 1000 + Hits),
    H3 = mix(H2, length(Pieces)),
    mix(H3, byte_size(binary:replace(Text, <<"tag=">>, <<"#">>, [global]))).

run(?ROWS, _Seed, H, _Batch, _Size, _Words) ->
    H;
run(I, Seed, H, Batch, Size, Words) ->
    A = next(Seed),
    B = next(A),
    C = next(B),
    First = element(C rem 16 + 1, Words),
    Second = element(A rem 16 + 1, Words),
    Row = <<"id=", (integer_to_binary(I))/binary, ",user=u", (integer_to_binary(A rem 997))/binary,
            ",score=", (integer_to_binary(B))/binary, ",tag=", First/binary, "-", Second/binary>>,
    H1 = row_checksum(H, I, A, B, C, Row),
    case Size + 1 of
        ?BATCH_SIZE -> run(I + 1, C, batch_checksum(H1, lists:reverse([Row | Batch])), [], 0, Words);
        More -> run(I + 1, C, H1, [Row | Batch], More, Words)
    end.

main() -> io:format("~p~n", [run(0, 42, 7, [], 0, words())]).
