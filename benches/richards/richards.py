# Richards: a simulation of an operating system's task scheduler. Stresses
# method dispatch, attribute reads and writes on small objects, and linked queues.
# Ported from Are We Fast Yet (https://github.com/smarr/are-we-fast-yet, MIT licence).

NO_TASK = None
NO_WORK = None

IDLER = 0
WORKER = 1
HANDLER_A = 2
HANDLER_B = 3
DEVICE_A = 4
DEVICE_B = 5
NUM_TYPES = 6

DEVICE_PACKET_KIND = 0
WORK_PACKET_KIND = 1

DATA_SIZE = 4


def append(packet, queue_head):
    packet.link = NO_WORK
    if queue_head is NO_WORK:
        return packet
    mouse = queue_head
    link = mouse.link
    while link is not NO_WORK:
        mouse = link
        link = mouse.link
    mouse.link = packet
    return queue_head


class Packet:
    def __init__(self, link, identity, kind):
        self.link = link
        self.identity = identity
        self.kind = kind
        self.datum = 0
        self.data = [0] * DATA_SIZE


class DeviceTaskDataRecord:
    def __init__(self):
        self.pending = NO_WORK


class HandlerTaskDataRecord:
    def __init__(self):
        self.work_in = NO_WORK
        self.device_in = NO_WORK


class IdleTaskDataRecord:
    def __init__(self):
        self.control = 1
        self.count = 10000


class WorkerTaskDataRecord:
    def __init__(self):
        self.destination = HANDLER_A
        self.count = 0


class TaskControlBlock:
    def __init__(self, link, identity, priority, initial_work_queue, packet_pending, task_waiting, private_data, fn):
        self.link = link
        self.identity = identity
        self.priority = priority
        self.input = initial_work_queue
        self.packet_pending = packet_pending
        self.task_waiting = task_waiting
        self.task_holding = False
        self.handle = private_data
        self.function = fn

    def is_task_holding_or_waiting(self):
        return self.task_holding or (not self.packet_pending and self.task_waiting)

    def is_waiting_with_packet(self):
        return self.packet_pending and self.task_waiting and not self.task_holding

    def add_input_and_check_priority(self, packet, old_task):
        if self.input is NO_WORK:
            self.input = packet
            self.packet_pending = True
            if self.priority > old_task.priority:
                return self
        else:
            self.input = append(packet, self.input)
        return old_task

    def run_task(self):
        if self.is_waiting_with_packet():
            message = self.input
            self.input = message.link
            self.packet_pending = self.input is not NO_WORK
            self.task_waiting = False
            self.task_holding = False
        else:
            message = NO_WORK
        return self.function(message, self.handle)


class Scheduler:
    def __init__(self):
        self.queue_count = 0
        self.hold_count = 0
        self.task_table = [NO_TASK] * NUM_TYPES
        self.task_list = NO_TASK
        self.current_task = NO_TASK
        self.current_task_identity = 0

    def create_task(self, identity, priority, work, packet_pending, task_waiting, data, fn):
        tcb = TaskControlBlock(self.task_list, identity, priority, work, packet_pending, task_waiting, data, fn)
        self.task_list = tcb
        self.task_table[identity] = tcb

    def create_device(self, identity, priority, work):
        def fn(packet, data):
            if packet is NO_WORK:
                packet = data.pending
                if packet is NO_WORK:
                    return self.wait()
                data.pending = NO_WORK
                return self.queue_packet(packet)
            data.pending = packet
            return self.hold_self()

        self.create_task(identity, priority, work, False, True, DeviceTaskDataRecord(), fn)

    def create_handler(self, identity, priority, work):
        def fn(packet, data):
            if packet is not NO_WORK:
                if packet.kind == WORK_PACKET_KIND:
                    data.work_in = append(packet, data.work_in)
                else:
                    data.device_in = append(packet, data.device_in)

            work_packet = data.work_in
            if work_packet is NO_WORK:
                return self.wait()
            count = work_packet.datum
            if count >= DATA_SIZE:
                data.work_in = work_packet.link
                return self.queue_packet(work_packet)
            device_packet = data.device_in
            if device_packet is NO_WORK:
                return self.wait()
            data.device_in = device_packet.link
            device_packet.datum = work_packet.data[count]
            work_packet.datum = count + 1
            return self.queue_packet(device_packet)

        self.create_task(identity, priority, work, True, True, HandlerTaskDataRecord(), fn)

    def create_idler(self, identity, priority, work):
        def fn(_packet, data):
            data.count -= 1
            if data.count == 0:
                return self.hold_self()
            if data.control & 1 == 0:
                data.control //= 2
                return self.release(DEVICE_A)
            data.control = (data.control // 2) ^ 53256
            return self.release(DEVICE_B)

        self.create_task(identity, priority, work, False, False, IdleTaskDataRecord(), fn)

    def create_worker(self, identity, priority, work):
        def fn(packet, data):
            if packet is NO_WORK:
                return self.wait()
            data.destination = HANDLER_B if data.destination == HANDLER_A else HANDLER_A
            packet.identity = data.destination
            packet.datum = 0
            for i in range(DATA_SIZE):
                data.count += 1
                if data.count > 26:
                    data.count = 1
                packet.data[i] = 65 + data.count - 1
            return self.queue_packet(packet)

        self.create_task(identity, priority, work, True, True, WorkerTaskDataRecord(), fn)

    def start(self):
        self.create_idler(IDLER, 0, NO_WORK)
        queue = Packet(NO_WORK, WORKER, WORK_PACKET_KIND)
        queue = Packet(queue, WORKER, WORK_PACKET_KIND)

        self.create_worker(WORKER, 1000, queue)
        queue = Packet(NO_WORK, DEVICE_A, DEVICE_PACKET_KIND)
        queue = Packet(queue, DEVICE_A, DEVICE_PACKET_KIND)
        queue = Packet(queue, DEVICE_A, DEVICE_PACKET_KIND)

        self.create_handler(HANDLER_A, 2000, queue)
        queue = Packet(NO_WORK, DEVICE_B, DEVICE_PACKET_KIND)
        queue = Packet(queue, DEVICE_B, DEVICE_PACKET_KIND)
        queue = Packet(queue, DEVICE_B, DEVICE_PACKET_KIND)

        self.create_handler(HANDLER_B, 3000, queue)
        self.create_device(DEVICE_A, 4000, NO_WORK)
        self.create_device(DEVICE_B, 5000, NO_WORK)

        self.schedule()

    def hold_self(self):
        self.hold_count += 1
        current_task = self.current_task
        current_task.task_holding = True
        return current_task.link

    def queue_packet(self, packet):
        task = self.task_table[packet.identity]
        self.queue_count += 1
        packet.link = NO_WORK
        packet.identity = self.current_task_identity
        return task.add_input_and_check_priority(packet, self.current_task)

    def release(self, identity):
        task = self.task_table[identity]
        task.task_holding = False
        if task.priority > self.current_task.priority:
            return task
        return self.current_task

    def wait(self):
        current_task = self.current_task
        current_task.task_waiting = True
        return current_task

    def schedule(self):
        self.current_task = self.task_list
        while self.current_task is not NO_TASK:
            if self.current_task.is_task_holding_or_waiting():
                self.current_task = self.current_task.link
            else:
                self.current_task_identity = self.current_task.identity
                self.current_task = self.current_task.run_task()


def main():
    queued = 0
    held = 0
    for _ in range(25):
        scheduler = Scheduler()
        scheduler.start()
        queued += scheduler.queue_count
        held += scheduler.hold_count
    print(queued)
    print(held)


main()
