%% Json from the Are We Fast Yet suite: a hand-written JSON parser run over the
%% suite's fixed 25,820-byte document. It stresses byte scanning, cutting strings
%% out of the input and building a tree. Ported from benchmarks/Lua/json.lua in
%% https://github.com/smarr/are-we-fast-yet (MIT licence).
%% The suite's parser is based on https://github.com/ralfstx/minimal-json (MIT).
-module(json).
-export([main/0]).

%% A value is null, true, false, {number, Text}, {string, Text}, {array, Items}
%% or {object, Names, Values, Table}, where Table maps a name to its place in
%% Names and Values.

parse(In) ->
    {Value, After} = read_value(In, skip_white_space(In, 0)),
    End = skip_white_space(In, After),
    case current(In, End) of
        -1 -> Value;
        _ -> error({json, End, end_of_text})
    end.

%% The byte at I, or -1 past the end.
current(In, I) ->
    case In of
        <<_:I/binary, C, _/binary>> -> C;
        _ -> -1
    end.

read_value(In, I) ->
    case current(In, I) of
        $n -> read_null(In, I);
        $t -> read_true(In, I);
        $f -> read_false(In, I);
        $" -> read_string(In, I);
        $[ -> read_array(In, I);
        ${ -> read_object(In, I);
        $- -> read_number(In, I);
        C when C >= $0, C =< $9 -> read_number(In, I);
        _ -> error({json, I, value})
    end.

read_array(In, I) ->
    First = skip_white_space(In, I + 1),
    case current(In, First) of
        $] -> {{array, []}, First + 1};
        _ -> read_elements(In, First, [])
    end.

read_elements(In, I, Items) ->
    {Value, After} = read_value(In, skip_white_space(In, I)),
    Next = skip_white_space(In, After),
    case current(In, Next) of
        $, -> read_elements(In, Next + 1, [Value | Items]);
        $] -> {{array, lists:reverse([Value | Items])}, Next + 1};
        _ -> error({json, Next, comma_or_bracket})
    end.

read_object(In, I) ->
    First = skip_white_space(In, I + 1),
    case current(In, First) of
        $} -> {{object, [], [], #{}}, First + 1};
        _ -> read_members(In, First, [], [], #{}, 1)
    end.

read_members(In, I, Names, Values, Table, Size) ->
    {Name, AfterName} = read_name(In, skip_white_space(In, I)),
    Colon = read_required_byte(In, skip_white_space(In, AfterName), $:),
    {Value, After} = read_value(In, skip_white_space(In, Colon)),
    Next = skip_white_space(In, After),
    MoreTable = Table#{Name => Size},
    case current(In, Next) of
        $, ->
            read_members(In, Next + 1, [Name | Names], [Value | Values], MoreTable, Size + 1);
        $} ->
            Object = {object, lists:reverse([Name | Names]), lists:reverse([Value | Values]), MoreTable},
            {Object, Next + 1};
        _ ->
            error({json, Next, comma_or_brace})
    end.

read_name(In, I) ->
    case current(In, I) of
        $" -> read_string_internal(In, I);
        _ -> error({json, I, name})
    end.

read_null(In, I) ->
    U = read_required_byte(In, I + 1, $u),
    L = read_required_byte(In, U, $l),
    {null, read_required_byte(In, L, $l)}.

read_true(In, I) ->
    R = read_required_byte(In, I + 1, $r),
    U = read_required_byte(In, R, $u),
    {true, read_required_byte(In, U, $e)}.

read_false(In, I) ->
    A = read_required_byte(In, I + 1, $a),
    L = read_required_byte(In, A, $l),
    S = read_required_byte(In, L, $s),
    {false, read_required_byte(In, S, $e)}.

%% Gives the place after Byte if it is the one at I.
read_required_byte(In, I, Byte) ->
    case current(In, I) of
        Byte -> I + 1;
        _ -> error({json, I, {byte, Byte}})
    end.

read_string(In, I) ->
    {Text, After} = read_string_internal(In, I),
    {{string, Text}, After}.

read_string_internal(In, I) ->
    scan_string(In, I + 1, I + 1, <<>>).

%% Start is where the run of plain bytes being captured began, and Buffer holds
%% what earlier runs and escapes came to.
scan_string(In, I, Start, Buffer) ->
    case current(In, I) of
        $" ->
            Run = capture(In, Start, I),
            case Buffer of
                <<>> -> {Run, I + 1};
                _ -> {<<Buffer/binary, Run/binary>>, I + 1}
            end;
        $\\ ->
            Run = capture(In, Start, I),
            Escaped = read_escape(In, I + 1),
            scan_string(In, I + 2, I + 2, <<Buffer/binary, Run/binary, Escaped>>);
        -1 ->
            error({json, I, {byte, $"}});
        _ ->
            scan_string(In, I + 1, Start, Buffer)
    end.

read_escape(In, I) ->
    case current(In, I) of
        $" -> $";
        $/ -> $/;
        $\\ -> $\\;
        $b -> $\b;
        $f -> $\f;
        $n -> $\n;
        $r -> $\r;
        $t -> $\t;
        _ -> error({json, I, escape})
    end.

capture(In, Start, End) ->
    binary:part(In, Start, End - Start).

read_number(In, I) ->
    Digits =
        case current(In, I) of
            $- -> I + 1;
            _ -> I
        end,
    First = current(In, Digits),
    case is_digit(First) of
        true ->
            %% A leading 0 stands alone.
            Whole =
                case First of
                    $0 -> Digits + 1;
                    _ -> skip_digits(In, Digits + 1)
                end,
            End = read_exponent(In, read_fraction(In, Whole)),
            {{number, capture(In, I, End)}, End};
        false ->
            error({json, Digits, digit})
    end.

read_fraction(In, I) ->
    case current(In, I) of
        $. -> read_digits(In, I + 1);
        _ -> I
    end.

read_exponent(In, I) ->
    case current(In, I) of
        E when E =:= $e; E =:= $E ->
            case current(In, I + 1) of
                S when S =:= $+; S =:= $- -> read_digits(In, I + 2);
                _ -> read_digits(In, I + 1)
            end;
        _ ->
            I
    end.

%% One digit or more.
read_digits(In, I) ->
    case is_digit(current(In, I)) of
        true -> skip_digits(In, I + 1);
        false -> error({json, I, digit})
    end.

skip_digits(In, I) ->
    case is_digit(current(In, I)) of
        true -> skip_digits(In, I + 1);
        false -> I
    end.

is_digit(Byte) -> Byte >= $0 andalso Byte =< $9.

skip_white_space(In, I) ->
    case current(In, I) of
        C when C =:= $\s; C =:= $\t; C =:= $\n; C =:= $\r -> skip_white_space(In, I + 1);
        _ -> I
    end.

get({object, _Names, Values, Table}, Name) ->
    case Table of
        #{Name := Index} -> lists:nth(Index, Values);
        _ -> null
    end;
get(_, _) ->
    null.

%% The suite's check: head is an object and operations is an array. It gives
%% the number of operations, or -1 for a document of another shape.
operations(Document) ->
    case {get(Document, <<"head">>), get(Document, <<"operations">>)} of
        {{object, _, _, _}, {array, Items}} -> length(Items);
        _ -> -1
    end.

-record(tally, {objects = 0, arrays = 0, strings = 0, numbers = 0, literals = 0, characters = 0}).

tally({number, Text}, T) ->
    T#tally{numbers = T#tally.numbers + 1, characters = T#tally.characters + byte_size(Text)};
tally({string, Text}, T) ->
    T#tally{strings = T#tally.strings + 1, characters = T#tally.characters + byte_size(Text)};
tally({array, Items}, T) ->
    lists:foldl(fun tally/2, T#tally{arrays = T#tally.arrays + 1}, Items);
tally({object, Names, Values, _Table}, T) ->
    Named = lists:foldl(
        fun(Name, Acc) -> Acc#tally{characters = Acc#tally.characters + byte_size(Name)} end,
        T#tally{objects = T#tally.objects + 1},
        Names
    ),
    lists:foldl(fun tally/2, Named, Values);
tally(_Literal, T) ->
    T#tally{literals = T#tally.literals + 1}.

%% Parses the document Rounds times, adding up the operations each parse found,
%% and gives the last tree back with the total.
run(_In, 0, Total, Last) ->
    {Total, Last};
run(In, Rounds, Total, _Last) ->
    Document = parse(In),
    run(In, Rounds - 1, Total + operations(Document), Document).

rap_benchmark_minified() ->
    <<"{\"head\":{\"requestCounter\":4},\"operations\":[[\"destroy\",\"w54\"],[\"set\",\"w2\",{\"activeControl\":\"w99\"}],[\"set\",\"w21\",{\"customVariant\":\"variant_navigation\"}],[\"set\",\"w28\",{\"customVariant\":\"variant_selected\"}],[\"set\",\"w53\",{\"children\":[\"w95\"]}],[\"create\",\"w95\",\"rwt.widgets.Composite\",{\"parent\":\"w53\",\"style\":[\"NONE\"],\"bounds\":[0,0,1008,586],\"children\":[\"w96\",\"w97\"],\"tabIndex\":-1,\"clientArea\":[0,0,1008,586]}],[\"create\",\"w96\",\"rwt.widgets.Label\",{\"parent\":\"w95\",\"style\":[\"NONE\"],\"bounds\":[10,30,112,26],\"tabIndex\":-1,\"customVariant\":\"variant_pageHeadline\",\"text\":\"TableViewer\"}],[\"create\",\"w97\",\"rwt.widgets.Composite\",{\"parent\":\"w95\",\"style\":[\"NONE\"],\"bounds\":[0,61,1008,525],\"children\":[\"w98\",\"w99\",\"w226\",\"w228\"],\"tabIndex\":-1,\"clientArea\":[0,0,1008,525]}],[\"create\",\"w98\",\"rwt.widgets.Text\",{\"parent\":\"w97\",\"style\":[\"LEFT\",\"SINGLE\",\"BORDER\"],\"bounds\":[10,10,988,32],\"tabIndex\":22,\"activeKeys\":[\"#13\",\"#27\",\"#40\"]}],[\"listen\",\"w98\",{\"KeyDown\":true,\"Modify\":true}],[\"create\",\"w99\",\"rwt.widgets.Grid\",{\"parent\":\"w97\",\"style\":[\"SINGLE\",\"BORDER\"],\"appearance\":\"table\",\"indentionWidth\":0,\"treeColumn\":-1,\"markupEnabled\":false}],[\"create\",\"w100\",\"rwt.widgets.ScrollBar\",{\"parent\":\"w99\",\"style\":[\"HORIZONTAL\"]}],[\"create\",\"w101\",\"rwt.widgets.ScrollBar\",{\"parent\":\"w99\",\"style\":[\"VERTICAL\"]}],[\"set\",\"w99\",{\"bounds\":[10,52,988,402],\"children\":[],\"tabIndex\":23,\"activeKeys\":[\"CTRL+#70\",\"CTRL+#78\",\"CTRL+#82\",\"CTRL+#89\",\"CTRL+#83\",\"CTRL+#71\",\"CTRL+#69\"],\"cancelKeys\":[\"CTRL+#70\",\"CTRL+#78\",\"CTRL+#82\",\"CTRL+#89\",\"CTRL+#83\",\"CTRL+#71\",\"CTRL+#69\"]}],[\"listen\",\"w99\",{\"MouseDown\":true,\"MouseUp\":true,\"MouseDoubleClick\":true,\"KeyDown\":true}],[\"set\",\"w99\",{\"itemCount\":118,\"itemHeight\":28,\"itemMetrics\":[[0,0,50,3,0,3,44],[1,50,50,53,0,53,44],[2,100,140,103,0,103,134],[3,240,180,243,0,243,174],[4,420,50,423,0,423,44],[5,470,50,473,0,473,44]],\"columnCount\":6,\"headerHeight\":35,\"headerVisible\":true,\"linesVisible\":true,\"focusItem\":\"w108\",\"selection\":[\"w108\"]}],[\"listen\",\"w99\",{\"Selection\":true,\"DefaultSelection\":true}],[\"set\",\"w99\",{\"enableCellToolTip\":true}],[\"listen\",\"w100\",{\"Selection\":true}],[\"set\",\"w101\",{\"visibility\":true}],[\"listen\",\"w101\",{\"Selection\":true}],[\"create\",\"w102\",\"rwt.widgets.GridColumn\",{\"parent\":\"w99\",\"text\":\"Nr.\",\"width\":50,\"moveable\":true}],[\"listen\",\"w102\",{\"Selection\":true}],[\"create\",\"w103\",\"rwt.widgets.GridColumn\",{\"parent\":\"w99\",\"text\":\"Sym.\",\"index\":1,\"left\":50,\"width\":50,\"moveable\":true}],[\"listen\",\"w103\",{\"Selection\":true}],[\"create\",\"w104\",\"rwt.widgets.GridColumn\",{\"parent\":\"w99\",\"text\":\"Name\",\"index\":2,\"left\":100,\"width\":140,\"moveable\":true}],[\"listen\",\"w104\",{\"Selection\":true}],[\"create\",\"w105\",\"rwt.widgets.GridColumn\",{\"parent\":\"w99\",\"text\":\"Series\",\"index\":3,\"left\":240,\"width\":180,\"moveable\":true}],[\"listen\",\"w105\",{\"Selection\":true}],[\"create\",\"w106\",\"rwt.widgets.GridColumn\",{\"parent\":\"w99\",\"text\":\"Group\",\"index\":4,\"left\":420,\"width\":50,\"moveable\":true}],[\"listen\",\"w106\",{\"Selection\":true}],[\"create\",\"w107\",\"rwt.widgets.GridColumn\",{\"parent\":\"w99\",\"text\":\"Period\",\"index\":5,\"left\":470,\"width\":50,\"moveable\":true}],[\"listen\",\"w107\",{\"Selection\":true}],[\"create\",\"w108\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":0,\"texts\":[\"1\",\"H\",\"Hydrogen\",\"Nonmetal\",\"1\",\"1\"],\"cellBackgrounds\":[null,null,null,[138,226,52,255],null,null]}],[\"create\",\"w109\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":1,\"texts\":[\"2\",\"He\",\"Helium\",\"Noble gas\",\"18\",\"1\"],\"cellBackgrounds\":[null,null,null,[114,159,207,255],null,null]}],[\"create\",\"w110\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":2,\"texts\":[\"3\",\"Li\",\"Lithium\",\"Alkali metal\",\"1\",\"2\"],\"cellBackgrounds\":[null,null,null,[239,41,41,255],null,null]}],[\"create\",\"w111\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":3,\"texts\":[\"4\",\"Be\",\"Beryllium\",\"Alkaline earth metal\",\"2\",\"2\"],\"cellBackgrounds\":[null,null,null,[233,185,110,255],null,null]}],[\"create\",\"w112\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":4,\"texts\":[\"5\",\"B\",\"Boron\",\"Metalloid\",\"13\",\"2\"],\"cellBackgrounds\":[null,null,null,[156,159,153,255],null,null]}],[\"create\",\"w113\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":5,\"texts\":[\"6\",\"C\",\"Carbon\",\"Nonmetal\",\"14\",\"2\"],\"cellBackgrounds\":[null,null,null,[138,226,52,255],null,null]}],[\"create\",\"w114\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":6,\"texts\":[\"7\",\"N\",\"Nitrogen\",\"Nonmetal\",\"15\",\"2\"],\"cellBackgrounds\":[null,null,null,[138,226,52,255],null,null]}],[\"create\",\"w115\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":7,\"texts\":[\"8\",\"O\",\"Oxygen\",\"Nonmetal\",\"16\",\"2\"],\"cellBackgrounds\":[null,null,null,[138,226,52,255],null,null]}],[\"create\",\"w116\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":8,\"texts\":[\"9\",\"F\",\"Fluorine\",\"Halogen\",\"17\",\"2\"],\"cellBackgrounds\":[null,null,null,[252,233,79,255],null,null]}],[\"create\",\"w117\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":9,\"texts\":[\"10\",\"Ne\",\"Neon\",\"Noble gas\",\"18\",\"2\"],\"cellBackgrounds\":[null,null,null,[114,159,207,255],null,null]}],[\"create\",\"w118\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":10,\"texts\":[\"11\",\"Na\",\"Sodium\",\"Alkali metal\",\"1\",\"3\"],\"cellBackgrounds\":[null,null,null,[239,41,41,255],null,null]}],[\"create\",\"w119\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":11,\"texts\":[\"12\",\"Mg\",\"Magnesium\",\"Alkaline earth metal\",\"2\",\"3\"],\"cellBackgrounds\":[null,null,null,[233,185,110,255],null,null]}],[\"create\",\"w120\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":12,\"texts\":[\"13\",\"Al\",\"Aluminium\",\"Poor metal\",\"13\",\"3\"],\"cellBackgrounds\":[null,null,null,[238,238,236,255],null,null]}],[\"create\",\"w121\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":13,\"texts\":[\"14\",\"Si\",\"Silicon\",\"Metalloid\",\"14\",\"3\"],\"cellBackgrounds\":[null,null,null,[156,159,153,255],null,null]}],[\"create\",\"w122\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":14,\"texts\":[\"15\",\"P\",\"Phosphorus\",\"Nonmetal\",\"15\",\"3\"],\"cellBackgrounds\":[null,null,null,[138,226,52,255],null,null]}],[\"create\",\"w123\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":15,\"texts\":[\"16\",\"S\",\"Sulfur\",\"Nonmetal\",\"16\",\"3\"],\"cellBackgrounds\":[null,null,null,[138,226,52,255],null,null]}],[\"create\",\"w124\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":16,\"texts\":[\"17\",\"Cl\",\"Chlorine\",\"Halogen\",\"17\",\"3\"],\"cellBackgrounds\":[null,null,null,[252,233,79,255],null,null]}],[\"create\",\"w125\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":17,\"texts\":[\"18\",\"Ar\",\"Argon\",\"Noble gas\",\"18\",\"3\"],\"cellBackgrounds\":[null,null,null,[114,159,207,255],null,null]}],[\"create\",\"w126\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":18,\"texts\":[\"19\",\"K\",\"Potassium\",\"Alkali metal\",\"1\",\"4\"],\"cellBackgrounds\":[null,null,null,[239,41,41,255],null,null]}],[\"create\",\"w127\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":19,\"texts\":[\"20\",\"Ca\",\"Calcium\",\"Alkaline earth metal\",\"2\",\"4\"],\"cellBackgrounds\":[null,null,null,[233,185,110,255],null,null]}],[\"create\",\"w128\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":20,\"texts\":[\"21\",\"Sc\",\"Scandium\",\"Transition metal\",\"3\",\"4\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w129\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":21,\"texts\":[\"22\",\"Ti\",\"Titanium\",\"Transition metal\",\"4\",\"4\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w130\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":22,\"texts\":[\"23\",\"V\",\"Vanadium\",\"Transition metal\",\"5\",\"4\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w131\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":23,\"texts\":[\"24\",\"Cr\",\"Chromium\",\"Transition metal\",\"6\",\"4\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w132\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":24,\"texts\":[\"25\",\"Mn\",\"Manganese\",\"Transition metal\",\"7\",\"4\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w133\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":25,\"texts\":[\"26\",\"Fe\",\"Iron\",\"Transition metal\",\"8\",\"4\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w134\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":26,\"texts\":[\"27\",\"Co\",\"Cobalt\",\"Transition metal\",\"9\",\"4\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w135\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":27,\"texts\":[\"28\",\"Ni\",\"Nickel\",\"Transition metal\",\"10\",\"4\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w136\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":28,\"texts\":[\"29\",\"Cu\",\"Copper\",\"Transition metal\",\"11\",\"4\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w137\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":29,\"texts\":[\"30\",\"Zn\",\"Zinc\",\"Transition metal\",\"12\",\"4\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w138\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":30,\"texts\":[\"31\",\"Ga\",\"Gallium\",\"Poor metal\",\"13\",\"4\"],\"cellBackgrounds\":[null,null,null,[238,238,236,255],null,null]}],[\"create\",\"w139\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":31,\"texts\":[\"32\",\"Ge\",\"Germanium\",\"Metalloid\",\"14\",\"4\"],\"cellBackgrounds\":[null,null,null,[156,159,153,255],null,null]}],[\"create\",\"w140\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":32,\"texts\":[\"33\",\"As\",\"Arsenic\",\"Metalloid\",\"15\",\"4\"],\"cellBackgrounds\":[null,null,null,[156,159,153,255],null,null]}],[\"create\",\"w141\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":33,\"texts\":[\"34\",\"Se\",\"Selenium\",\"Nonmetal\",\"16\",\"4\"],\"cellBackgrounds\":[null,null,null,[138,226,52,255],null,null]}],[\"create\",\"w142\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":34,\"texts\":[\"35\",\"Br\",\"Bromine\",\"Halogen\",\"17\",\"4\"],\"cellBackgrounds\":[null,null,null,[252,233,79,255],null,null]}],[\"create\",\"w143\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":35,\"texts\":[\"36\",\"Kr\",\"Krypton\",\"Noble gas\",\"18\",\"4\"],\"cellBackgrounds\":[null,null,null,[114,159,207,255],null,null]}],[\"create\",\"w144\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":36,\"texts\":[\"37\",\"Rb\",\"Rubidium\",\"Alkali metal\",\"1\",\"5\"],\"cellBackgrounds\":[null,null,null,[239,41,41,255],null,null]}],[\"create\",\"w145\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":37,\"texts\":[\"38\",\"Sr\",\"Strontium\",\"Alkaline earth metal\",\"2\",\"5\"],\"cellBackgrounds\":[null,null,null,[233,185,110,255],null,null]}],[\"create\",\"w146\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":38,\"texts\":[\"39\",\"Y\",\"Yttrium\",\"Transition metal\",\"3\",\"5\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w147\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":39,\"texts\":[\"40\",\"Zr\",\"Zirconium\",\"Transition metal\",\"4\",\"5\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w148\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":40,\"texts\":[\"41\",\"Nb\",\"Niobium\",\"Transition metal\",\"5\",\"5\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w149\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":41,\"texts\":[\"42\",\"Mo\",\"Molybdenum\",\"Transition metal\",\"6\",\"5\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w150\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":42,\"texts\":[\"43\",\"Tc\",\"Technetium\",\"Transition metal\",\"7\",\"5\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w151\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":43,\"texts\":[\"44\",\"Ru\",\"Ruthenium\",\"Transition metal\",\"8\",\"5\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w152\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":44,\"texts\":[\"45\",\"Rh\",\"Rhodium\",\"Transition metal\",\"9\",\"5\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w153\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":45,\"texts\":[\"46\",\"Pd\",\"Palladium\",\"Transition metal\",\"10\",\"5\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w154\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":46,\"texts\":[\"47\",\"Ag\",\"Silver\",\"Transition metal\",\"11\",\"5\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w155\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":47,\"texts\":[\"48\",\"Cd\",\"Cadmium\",\"Transition metal\",\"12\",\"5\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w156\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":48,\"texts\":[\"49\",\"In\",\"Indium\",\"Poor metal\",\"13\",\"5\"],\"cellBackgrounds\":[null,null,null,[238,238,236,255],null,null]}],[\"create\",\"w157\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":49,\"texts\":[\"50\",\"Sn\",\"Tin\",\"Poor metal\",\"14\",\"5\"],\"cellBackgrounds\":[null,null,null,[238,238,236,255],null,null]}],[\"create\",\"w158\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":50,\"texts\":[\"51\",\"Sb\",\"Antimony\",\"Metalloid\",\"15\",\"5\"],\"cellBackgrounds\":[null,null,null,[156,159,153,255],null,null]}],[\"create\",\"w159\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":51,\"texts\":[\"52\",\"Te\",\"Tellurium\",\"Metalloid\",\"16\",\"5\"],\"cellBackgrounds\":[null,null,null,[156,159,153,255],null,null]}],[\"create\",\"w160\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":52,\"texts\":[\"53\",\"I\",\"Iodine\",\"Halogen\",\"17\",\"5\"],\"cellBackgrounds\":[null,null,null,[252,233,79,255],null,null]}],[\"create\",\"w161\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":53,\"texts\":[\"54\",\"Xe\",\"Xenon\",\"Noble gas\",\"18\",\"5\"],\"cellBackgrounds\":[null,null,null,[114,159,207,255],null,null]}],[\"create\",\"w162\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":54,\"texts\":[\"55\",\"Cs\",\"Caesium\",\"Alkali metal\",\"1\",\"6\"],\"cellBackgrounds\":[null,null,null,[239,41,41,255],null,null]}],[\"create\",\"w163\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":55,\"texts\":[\"56\",\"Ba\",\"Barium\",\"Alkaline earth metal\",\"2\",\"6\"],\"cellBackgrounds\":[null,null,null,[233,185,110,255],null,null]}],[\"create\",\"w164\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":56,\"texts\":[\"57\",\"La\",\"Lanthanum\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w165\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":57,\"texts\":[\"58\",\"Ce\",\"Cerium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w166\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":58,\"texts\":[\"59\",\"Pr\",\"Praseodymium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w167\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":59,\"texts\":[\"60\",\"Nd\",\"Neodymium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w168\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":60,\"texts\":[\"61\",\"Pm\",\"Promethium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w169\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":61,\"texts\":[\"62\",\"Sm\",\"Samarium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w170\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":62,\"texts\":[\"63\",\"Eu\",\"Europium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w171\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":63,\"texts\":[\"64\",\"Gd\",\"Gadolinium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w172\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":64,\"texts\":[\"65\",\"Tb\",\"Terbium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w173\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":65,\"texts\":[\"66\",\"Dy\",\"Dysprosium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w174\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":66,\"texts\":[\"67\",\"Ho\",\"Holmium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w175\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":67,\"texts\":[\"68\",\"Er\",\"Erbium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w176\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":68,\"texts\":[\"69\",\"Tm\",\"Thulium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w177\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":69,\"texts\":[\"70\",\"Yb\",\"Ytterbium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w178\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":70,\"texts\":[\"71\",\"Lu\",\"Lutetium\",\"Lanthanide\",\"3\",\"6\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w179\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":71,\"texts\":[\"72\",\"Hf\",\"Hafnium\",\"Transition metal\",\"4\",\"6\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w180\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":72,\"texts\":[\"73\",\"Ta\",\"Tantalum\",\"Transition metal\",\"5\",\"6\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w181\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":73,\"texts\":[\"74\",\"W\",\"Tungsten\",\"Transition metal\",\"6\",\"6\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w182\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":74,\"texts\":[\"75\",\"Re\",\"Rhenium\",\"Transition metal\",\"7\",\"6\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w183\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":75,\"texts\":[\"76\",\"Os\",\"Osmium\",\"Transition metal\",\"8\",\"6\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w184\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":76,\"texts\":[\"77\",\"Ir\",\"Iridium\",\"Transition metal\",\"9\",\"6\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w185\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":77,\"texts\":[\"78\",\"Pt\",\"Platinum\",\"Transition metal\",\"10\",\"6\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w186\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":78,\"texts\":[\"79\",\"Au\",\"Gold\",\"Transition metal\",\"11\",\"6\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w187\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":79,\"texts\":[\"80\",\"Hg\",\"Mercury\",\"Transition metal\",\"12\",\"6\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w188\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":80,\"texts\":[\"81\",\"Tl\",\"Thallium\",\"Poor metal\",\"13\",\"6\"],\"cellBackgrounds\":[null,null,null,[238,238,236,255],null,null]}],[\"create\",\"w189\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":81,\"texts\":[\"82\",\"Pb\",\"Lead\",\"Poor metal\",\"14\",\"6\"],\"cellBackgrounds\":[null,null,null,[238,238,236,255],null,null]}],[\"create\",\"w190\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":82,\"texts\":[\"83\",\"Bi\",\"Bismuth\",\"Poor metal\",\"15\",\"6\"],\"cellBackgrounds\":[null,null,null,[238,238,236,255],null,null]}],[\"create\",\"w191\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":83,\"texts\":[\"84\",\"Po\",\"Polonium\",\"Metalloid\",\"16\",\"6\"],\"cellBackgrounds\":[null,null,null,[156,159,153,255],null,null]}],[\"create\",\"w192\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":84,\"texts\":[\"85\",\"At\",\"Astatine\",\"Halogen\",\"17\",\"6\"],\"cellBackgrounds\":[null,null,null,[252,233,79,255],null,null]}],[\"create\",\"w193\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":85,\"texts\":[\"86\",\"Rn\",\"Radon\",\"Noble gas\",\"18\",\"6\"],\"cellBackgrounds\":[null,null,null,[114,159,207,255],null,null]}],[\"create\",\"w194\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":86,\"texts\":[\"87\",\"Fr\",\"Francium\",\"Alkali metal\",\"1\",\"7\"],\"cellBackgrounds\":[null,null,null,[239,41,41,255],null,null]}],[\"create\",\"w195\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":87,\"texts\":[\"88\",\"Ra\",\"Radium\",\"Alkaline earth metal\",\"2\",\"7\"],\"cellBackgrounds\":[null,null,null,[233,185,110,255],null,null]}],[\"create\",\"w196\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":88,\"texts\":[\"89\",\"Ac\",\"Actinium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w197\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":89,\"texts\":[\"90\",\"Th\",\"Thorium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w198\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":90,\"texts\":[\"91\",\"Pa\",\"Protactinium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w199\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":91,\"texts\":[\"92\",\"U\",\"Uranium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w200\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":92,\"texts\":[\"93\",\"Np\",\"Neptunium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w201\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":93,\"texts\":[\"94\",\"Pu\",\"Plutonium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w202\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":94,\"texts\":[\"95\",\"Am\",\"Americium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w203\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":95,\"texts\":[\"96\",\"Cm\",\"Curium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w204\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":96,\"texts\":[\"97\",\"Bk\",\"Berkelium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w205\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":97,\"texts\":[\"98\",\"Cf\",\"Californium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w206\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":98,\"texts\":[\"99\",\"Es\",\"Einsteinium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w207\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":99,\"texts\":[\"100\",\"Fm\",\"Fermium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w208\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":100,\"texts\":[\"101\",\"Md\",\"Mendelevium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w209\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":101,\"texts\":[\"102\",\"No\",\"Nobelium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w210\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":102,\"texts\":[\"103\",\"Lr\",\"Lawrencium\",\"Actinide\",\"3\",\"7\"],\"cellBackgrounds\":[null,null,null,[173,127,168,255],null,null]}],[\"create\",\"w211\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":103,\"texts\":[\"104\",\"Rf\",\"Rutherfordium\",\"Transition metal\",\"4\",\"7\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w212\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":104,\"texts\":[\"105\",\"Db\",\"Dubnium\",\"Transition metal\",\"5\",\"7\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w213\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":105,\"texts\":[\"106\",\"Sg\",\"Seaborgium\",\"Transition metal\",\"6\",\"7\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w214\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":106,\"texts\":[\"107\",\"Bh\",\"Bohrium\",\"Transition metal\",\"7\",\"7\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w215\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":107,\"texts\":[\"108\",\"Hs\",\"Hassium\",\"Transition metal\",\"8\",\"7\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w216\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":108,\"texts\":[\"109\",\"Mt\",\"Meitnerium\",\"Transition metal\",\"9\",\"7\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w217\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":109,\"texts\":[\"110\",\"Ds\",\"Darmstadtium\",\"Transition metal\",\"10\",\"7\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w218\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":110,\"texts\":[\"111\",\"Rg\",\"Roentgenium\",\"Transition metal\",\"11\",\"7\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w219\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":111,\"texts\":[\"112\",\"Uub\",\"Ununbium\",\"Transition metal\",\"12\",\"7\"],\"cellBackgrounds\":[null,null,null,[252,175,62,255],null,null]}],[\"create\",\"w220\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":112,\"texts\":[\"113\",\"Uut\",\"Ununtrium\",\"Poor metal\",\"13\",\"7\"],\"cellBackgrounds\":[null,null,null,[238,238,236,255],null,null]}],[\"create\",\"w221\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":113,\"texts\":[\"114\",\"Uuq\",\"Ununquadium\",\"Poor metal\",\"14\",\"7\"],\"cellBackgrounds\":[null,null,null,[238,238,236,255],null,null]}],[\"create\",\"w222\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":114,\"texts\":[\"115\",\"Uup\",\"Ununpentium\",\"Poor metal\",\"15\",\"7\"],\"cellBackgrounds\":[null,null,null,[238,238,236,255],null,null]}],[\"create\",\"w223\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":115,\"texts\":[\"116\",\"Uuh\",\"Ununhexium\",\"Poor metal\",\"16\",\"7\"],\"cellBackgrounds\":[null,null,null,[238,238,236,255],null,null]}],[\"create\",\"w224\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":116,\"texts\":[\"117\",\"Uus\",\"Ununseptium\",\"Halogen\",\"17\",\"7\"],\"cellBackgrounds\":[null,null,null,[252,233,79,255],null,null]}],[\"create\",\"w225\",\"rwt.widgets.GridItem\",{\"parent\":\"w99\",\"index\":117,\"texts\":[\"118\",\"Uuo\",\"Ununoctium\",\"Noble gas\",\"18\",\"7\"],\"cellBackgrounds\":[null,null,null,[114,159,207,255],null,null]}],[\"create\",\"w226\",\"rwt.widgets.Composite\",{\"parent\":\"w97\",\"style\":[\"BORDER\"],\"bounds\":[10,464,988,25],\"children\":[\"w227\"],\"tabIndex\":-1,\"clientArea\":[0,0,986,23]}],[\"create\",\"w227\",\"rwt.widgets.Label\",{\"parent\":\"w226\",\"style\":[\"NONE\"],\"bounds\":[10,10,966,3],\"tabIndex\":-1,\"text\":\"Hydrogen (H)\"}],[\"create\",\"w228\",\"rwt.widgets.Label\",{\"parent\":\"w97\",\"style\":[\"WRAP\"],\"bounds\":[10,499,988,16],\"tabIndex\":-1,\"foreground\":[150,150,150,255],\"font\":[[\"Verdana\",\"Lucida Sans\",\"Arial\",\"Helvetica\",\"sans-serif\"],10,false,false],\"text\":\"Shortcuts: [CTRL+F] - Filter | Sort by: [CTRL+R] - Number, [CTRL+Y] - Symbol, [CTRL+N] - Name, [CTRL+S] - Series, [CTRL+G] - Group, [CTRL+E] - Period\"}],[\"set\",\"w1\",{\"focusControl\":\"w99\"}],[\"call\",\"rwt.client.BrowserNavigation\",\"addToHistory\",{\"entries\":[[\"tableviewer\",\"TableViewer\"]]}]]}">>.

main() ->
    {Total, Document} = run(rap_benchmark_minified(), 300, 0, null),
    T = tally(Document, #tally{}),
    io:format("~b~n~b ~b ~b ~b ~b ~b~n", [
        Total,
        T#tally.objects,
        T#tally.arrays,
        T#tally.strings,
        T#tally.numbers,
        T#tally.literals,
        T#tally.characters
    ]).
