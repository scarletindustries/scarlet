%% Richards: a simulation of an operating system's task scheduler. Stresses
%% dispatch on variants, records rebuilt with one field changed, and state passing.
%% Ported from Are We Fast Yet (https://github.com/smarr/are-we-fast-yet, MIT licence).
-module(richards).
-export([main/0]).

-define(NO_TASK, 0).
-define(IDLER, 1).
-define(WORKER, 2).
-define(HANDLER_A, 3).
-define(HANDLER_B, 4).
-define(DEVICE_A, 5).
-define(DEVICE_B, 6).

-define(DATA_SIZE, 4).

-record(packet, {identity, kind, datum = 1, data = [0, 0, 0, 0]}).

%% `data` is one of {idle, Control, Count}, {worker, Destination, Count},
%% {handler, WorkIn, DeviceIn} or {device, Pending}.
-record(task, {link, identity, priority, input, packet_pending, task_waiting,
               task_holding = false, data}).

%% `current` is the identity of the task to run next, or ?NO_TASK.
-record(scheduler, {tasks = #{}, current = ?NO_TASK, queue_count = 0, hold_count = 0}).

find_task(#scheduler{tasks = Tasks}, Identity) ->
    maps:get(Identity, Tasks).

put_task(S = #scheduler{tasks = Tasks}, T) ->
    S#scheduler{tasks = Tasks#{T#task.identity => T}}.

wait(S, T) ->
    put_task(S, T#task{task_waiting = true}).

hold_self(S = #scheduler{tasks = Tasks, hold_count = Held}, T) ->
    S#scheduler{tasks = Tasks#{T#task.identity => T#task{task_holding = true}},
                current = T#task.link,
                hold_count = Held + 1}.

release(S0, T, Identity) ->
    S = put_task(S0, T),
    Found = find_task(S, Identity),
    Freed = Found#task{task_holding = false},
    Next = if
        Freed#task.priority > T#task.priority -> Freed#task.identity;
        true -> T#task.identity
    end,
    S#scheduler{tasks = (S#scheduler.tasks)#{Identity => Freed}, current = Next}.

queue_packet(S0, T, Packet) ->
    S = put_task(S0, T),
    Target = find_task(S, Packet#packet.identity),
    Sent = Packet#packet{identity = T#task.identity},
    Tasks = S#scheduler.tasks,
    Queued = S#scheduler.queue_count + 1,
    case Target#task.input of
        [] ->
            Next = if
                Target#task.priority > T#task.priority -> Target#task.identity;
                true -> T#task.identity
            end,
            Woken = Target#task{input = [Sent], packet_pending = true},
            S#scheduler{tasks = Tasks#{Woken#task.identity => Woken},
                        current = Next,
                        queue_count = Queued};
        Queue ->
            Busy = Target#task{input = Queue ++ [Sent]},
            S#scheduler{tasks = Tasks#{Busy#task.identity => Busy}, queue_count = Queued}
    end.

idle(S, T, Control, Count0) ->
    Count = Count0 - 1,
    if
        Count =:= 0 ->
            hold_self(S, T#task{data = {idle, Control, Count}});
        Control band 1 =:= 0 ->
            release(S, T#task{data = {idle, Control div 2, Count}}, ?DEVICE_A);
        true ->
            release(S, T#task{data = {idle, (Control div 2) bxor 53256, Count}}, ?DEVICE_B)
    end.

%% Writes the next ?DATA_SIZE letters of the alphabet into a packet's data.
fill(Data, ?DATA_SIZE, Count) ->
    {lists:reverse(Data), Count};
fill(Data, I, Count0) ->
    Count = if
        Count0 >= 26 -> 1;
        true -> Count0 + 1
    end,
    fill([65 + Count - 1 | Data], I + 1, Count).

work(S, T, none, _Destination, _Count) ->
    wait(S, T);
work(S, T, Packet, Destination0, Count0) ->
    Destination = case Destination0 of
        ?HANDLER_A -> ?HANDLER_B;
        _ -> ?HANDLER_A
    end,
    {Data, Count} = fill([], 0, Count0),
    queue_packet(S, T#task{data = {worker, Destination, Count}},
                 Packet#packet{identity = Destination, datum = 1, data = Data}).

handle(S, T, none, WorkIn, DeviceIn) ->
    handle(S, T, WorkIn, DeviceIn);
handle(S, T, Packet = #packet{kind = work}, WorkIn, DeviceIn) ->
    handle(S, T, WorkIn ++ [Packet], DeviceIn);
handle(S, T, Packet = #packet{kind = device}, WorkIn, DeviceIn) ->
    handle(S, T, WorkIn, DeviceIn ++ [Packet]).

handle(S, T, [], DeviceIn) ->
    wait(S, T#task{data = {handler, [], DeviceIn}});
handle(S, T, [WorkPacket = #packet{datum = Count} | WorkRest], DeviceIn) when Count > ?DATA_SIZE ->
    queue_packet(S, T#task{data = {handler, WorkRest, DeviceIn}}, WorkPacket);
handle(S, T, WorkIn, []) ->
    wait(S, T#task{data = {handler, WorkIn, []}});
handle(S, T, [WorkPacket = #packet{datum = Count, data = Data} | WorkRest], [DevicePacket | DeviceRest]) ->
    Sent = DevicePacket#packet{datum = lists:nth(Count, Data)},
    Used = WorkPacket#packet{datum = Count + 1},
    queue_packet(S, T#task{data = {handler, [Used | WorkRest], DeviceRest}}, Sent).

drive(S, T, none, none) ->
    wait(S, T);
drive(S, T, none, Pending) ->
    queue_packet(S, T#task{data = {device, none}}, Pending);
drive(S, T, Packet, _Pending) ->
    hold_self(S, T#task{data = {device, Packet}}).

act(S, T = #task{data = {idle, Control, Count}}, _Message) ->
    idle(S, T, Control, Count);
act(S, T = #task{data = {worker, Destination, Count}}, Message) ->
    work(S, T, Message, Destination, Count);
act(S, T = #task{data = {handler, WorkIn, DeviceIn}}, Message) ->
    handle(S, T, Message, WorkIn, DeviceIn);
act(S, T = #task{data = {device, Pending}}, Message) ->
    drive(S, T, Message, Pending).

run_task(S, T = #task{packet_pending = true, task_waiting = true, task_holding = false,
                      input = [Message | Rest]}) ->
    act(S, T#task{input = Rest, packet_pending = Rest =/= [], task_waiting = false}, Message);
run_task(S, T) ->
    act(S, T, none).

schedule(S = #scheduler{current = ?NO_TASK}) ->
    S;
schedule(S = #scheduler{current = Current}) ->
    T = find_task(S, Current),
    case T#task.task_holding orelse ((not T#task.packet_pending) andalso T#task.task_waiting) of
        true -> schedule(S#scheduler{current = T#task.link});
        false -> schedule(run_task(S, T))
    end.

packet(Identity, Kind) ->
    #packet{identity = Identity, kind = Kind}.

create_task(S = #scheduler{tasks = Tasks, current = Link}, Identity, Priority, Input, Data) ->
    T = #task{link = Link,
              identity = Identity,
              priority = Priority,
              input = Input,
              packet_pending = Input =/= [],
              task_waiting = Identity =/= ?IDLER,
              data = Data},
    S#scheduler{tasks = Tasks#{Identity => T}, current = Identity}.

start() ->
    WorkQueue = [packet(?WORKER, work), packet(?WORKER, work)],
    AQueue = [packet(?DEVICE_A, device), packet(?DEVICE_A, device), packet(?DEVICE_A, device)],
    BQueue = [packet(?DEVICE_B, device), packet(?DEVICE_B, device), packet(?DEVICE_B, device)],
    S0 = #scheduler{},
    S1 = create_task(S0, ?IDLER, 0, [], {idle, 1, 10000}),
    S2 = create_task(S1, ?WORKER, 1000, WorkQueue, {worker, ?HANDLER_A, 0}),
    S3 = create_task(S2, ?HANDLER_A, 2000, AQueue, {handler, [], []}),
    S4 = create_task(S3, ?HANDLER_B, 3000, BQueue, {handler, [], []}),
    S5 = create_task(S4, ?DEVICE_A, 4000, [], {device, none}),
    S6 = create_task(S5, ?DEVICE_B, 5000, [], {device, none}),
    schedule(S6).

repeat(0, Queued, Held) ->
    {Queued, Held};
repeat(N, Queued, Held) ->
    S = start(),
    repeat(N - 1, Queued + S#scheduler.queue_count, Held + S#scheduler.hold_count).

main() ->
    {Queued, Held} = repeat(25, 0, 0),
    io:format("~p~n~p~n", [Queued, Held]).
