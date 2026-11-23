-- Richards: a simulation of an operating system's task scheduler. Stresses
-- method dispatch, field reads and writes on small objects, and linked queues.
-- Ported from Are We Fast Yet (https://github.com/smarr/are-we-fast-yet, MIT licence).

local bit = require 'bit'
local band, bxor = bit.band, bit.bxor

local NO_TASK = nil
local NO_WORK = nil

local IDLER     = 1
local WORKER    = 2
local HANDLER_A = 3
local HANDLER_B = 4
local DEVICE_A  = 5
local DEVICE_B  = 6

local DEVICE_PACKET_KIND = 0
local WORK_PACKET_KIND   = 1

local DATA_SIZE = 4

local function append (packet, queue_head)
    packet.link = NO_WORK
    if NO_WORK == queue_head then
        return packet
    end
    local mouse = queue_head
    local link = mouse.link
    while NO_WORK ~= link do
        mouse = link
        link = mouse.link
    end
    mouse.link = packet
    return queue_head
end

local function new_packet (link, identity, kind)
    return {
        link     = link,
        kind     = kind,
        identity = identity,
        datum    = 1,
        data     = {0, 0, 0, 0},
    }
end

local TaskControlBlock = {}
TaskControlBlock.__index = TaskControlBlock

function TaskControlBlock.new (link, identity, priority, initial_work_queue,
                               packet_pending, task_waiting, private_data, fn)
    local obj = {
        link = link,
        identity = identity,
        priority = priority,
        input = initial_work_queue,
        handle = private_data,
        packet_pending = packet_pending,
        task_waiting   = task_waiting,
        task_holding   = false,
        fn = fn,
    }
    return setmetatable(obj, TaskControlBlock)
end

function TaskControlBlock:is_task_holding_or_waiting ()
    return self.task_holding or (not self.packet_pending and self.task_waiting)
end

function TaskControlBlock:is_waiting_with_packet ()
    return self.packet_pending and self.task_waiting and not self.task_holding
end

function TaskControlBlock:add_input_and_check_priority (packet, old_task)
    if NO_WORK == self.input then
        self.input = packet
        self.packet_pending = true
        if self.priority > old_task.priority then
            return self
        end
    else
        self.input = append(packet, self.input)
    end
    return old_task
end

function TaskControlBlock:run_task ()
    local message
    if self:is_waiting_with_packet() then
        message = self.input
        self.input = message.link
        self.packet_pending = NO_WORK ~= self.input
        self.task_waiting = false
        self.task_holding = false
    else
        message = NO_WORK
    end
    return self.fn(message, self.handle)
end

local Scheduler = {}
Scheduler.__index = Scheduler

function Scheduler.new ()
    local obj = {
        task_list    = NO_TASK,
        current_task = NO_TASK,
        current_task_identity = 0,
        task_table = {},
        queue_count = 0,
        hold_count  = 0,
    }
    return setmetatable(obj, Scheduler)
end

function Scheduler:create_task (identity, priority, work, packet_pending, task_waiting, data, fn)
    local tcb = TaskControlBlock.new(self.task_list, identity, priority, work,
                                     packet_pending, task_waiting, data, fn)
    self.task_list = tcb
    self.task_table[identity] = tcb
end

function Scheduler:create_device (identity, priority, work)
    self:create_task(identity, priority, work, false, true, {pending = NO_WORK},
                     function (packet, data)
        if NO_WORK == packet then
            packet = data.pending
            if NO_WORK == packet then
                return self:wait()
            else
                data.pending = NO_WORK
                return self:queue_packet(packet)
            end
        else
            data.pending = packet
            return self:hold_self()
        end
    end)
end

function Scheduler:create_handler (identity, priority, work)
    self:create_task(identity, priority, work, true, true,
                     {work_in = NO_WORK, device_in = NO_WORK},
                     function (packet, data)
        if NO_WORK ~= packet then
            if WORK_PACKET_KIND == packet.kind then
                data.work_in = append(packet, data.work_in)
            else
                data.device_in = append(packet, data.device_in)
            end
        end

        local work_packet = data.work_in
        if NO_WORK == work_packet then
            return self:wait()
        else
            local count = work_packet.datum
            if count > DATA_SIZE then
                data.work_in = work_packet.link
                return self:queue_packet(work_packet)
            else
                local device_packet = data.device_in
                if NO_WORK == device_packet then
                    return self:wait()
                else
                    data.device_in = device_packet.link
                    device_packet.datum = work_packet.data[count]
                    work_packet.datum = count + 1
                    return self:queue_packet(device_packet)
                end
            end
        end
    end)
end

function Scheduler:create_idler (identity, priority, work)
    self:create_task(identity, priority, work, false, false,
                     {control = 1, count = 10000},
                     function (_, data)
        data.count = data.count - 1
        if 0 == data.count then
            return self:hold_self()
        else
            if 0 == band(data.control, 1) then
                data.control = data.control / 2
                return self:release(DEVICE_A)
            else
                data.control = bxor((data.control - 1) / 2, 53256)
                return self:release(DEVICE_B)
            end
        end
    end)
end

function Scheduler:create_worker (identity, priority, work)
    self:create_task(identity, priority, work, true, true,
                     {destination = HANDLER_A, count = 0},
                     function (packet, data)
        if NO_WORK == packet then
            return self:wait()
        else
            data.destination = (HANDLER_A == data.destination) and HANDLER_B or HANDLER_A
            packet.identity = data.destination
            packet.datum = 1
            for i = 1, DATA_SIZE do
                data.count = data.count + 1
                if data.count > 26 then
                    data.count = 1
                end
                packet.data[i] = 65 + data.count - 1
            end
            return self:queue_packet(packet)
        end
    end)
end

function Scheduler:start ()
    local queue
    self:create_idler(IDLER, 0, NO_WORK)
    queue = new_packet(NO_WORK, WORKER, WORK_PACKET_KIND)
    queue = new_packet(queue,   WORKER, WORK_PACKET_KIND)

    self:create_worker(WORKER, 1000, queue)
    queue = new_packet(NO_WORK, DEVICE_A, DEVICE_PACKET_KIND)
    queue = new_packet(queue,   DEVICE_A, DEVICE_PACKET_KIND)
    queue = new_packet(queue,   DEVICE_A, DEVICE_PACKET_KIND)

    self:create_handler(HANDLER_A, 2000, queue)
    queue = new_packet(NO_WORK, DEVICE_B, DEVICE_PACKET_KIND)
    queue = new_packet(queue,   DEVICE_B, DEVICE_PACKET_KIND)
    queue = new_packet(queue,   DEVICE_B, DEVICE_PACKET_KIND)

    self:create_handler(HANDLER_B, 3000, queue)
    self:create_device(DEVICE_A, 4000, NO_WORK)
    self:create_device(DEVICE_B, 5000, NO_WORK)

    self:schedule()
end

function Scheduler:hold_self ()
    self.hold_count = self.hold_count + 1
    local current_task = self.current_task
    current_task.task_holding = true
    return current_task.link
end

function Scheduler:queue_packet (packet)
    local task = self.task_table[packet.identity]
    self.queue_count = self.queue_count + 1
    packet.link     = NO_WORK
    packet.identity = self.current_task_identity
    return task:add_input_and_check_priority(packet, self.current_task)
end

function Scheduler:release (identity)
    local task = self.task_table[identity]
    task.task_holding = false
    if task.priority > self.current_task.priority then
        return task
    else
        return self.current_task
    end
end

function Scheduler:wait ()
    local current_task = self.current_task
    current_task.task_waiting = true
    return current_task
end

function Scheduler:schedule ()
    self.current_task = self.task_list
    while self.current_task ~= NO_TASK do
        if self.current_task:is_task_holding_or_waiting() then
            self.current_task = self.current_task.link
        else
            self.current_task_identity = self.current_task.identity
            self.current_task = self.current_task:run_task()
        end
    end
end

local queued, held = 0, 0
for _ = 1, 25 do
    local scheduler = Scheduler.new()
    scheduler:start()
    queued = queued + scheduler.queue_count
    held = held + scheduler.hold_count
end
print(queued)
print(held)
