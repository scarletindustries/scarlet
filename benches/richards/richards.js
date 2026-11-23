// Richards: a simulation of an operating system's task scheduler. Stresses
// method dispatch, field reads and writes on small objects, and linked queues.
// Ported from Are We Fast Yet (https://github.com/smarr/are-we-fast-yet, MIT licence).
'use strict';

const NO_TASK = null;
const NO_WORK = null;

const IDLER = 0;
const WORKER = 1;
const HANDLER_A = 2;
const HANDLER_B = 3;
const DEVICE_A = 4;
const DEVICE_B = 5;
const NUM_TYPES = 6;

const DEVICE_PACKET_KIND = 0;
const WORK_PACKET_KIND = 1;

const DATA_SIZE = 4;

function append(packet, queueHead) {
  packet.link = NO_WORK;
  if (NO_WORK === queueHead) {
    return packet;
  }
  let mouse = queueHead;
  let link;
  while (NO_WORK !== (link = mouse.link)) {
    mouse = link;
  }
  mouse.link = packet;
  return queueHead;
}

class Packet {
  constructor(link, identity, kind) {
    this.link = link;
    this.identity = identity;
    this.kind = kind;
    this.datum = 0;
    this.data = new Array(DATA_SIZE).fill(0);
  }
}

class DeviceTaskDataRecord {
  constructor() {
    this.pending = NO_WORK;
  }
}

class HandlerTaskDataRecord {
  constructor() {
    this.workIn = NO_WORK;
    this.deviceIn = NO_WORK;
  }
}

class IdleTaskDataRecord {
  constructor() {
    this.control = 1;
    this.count = 10000;
  }
}

class WorkerTaskDataRecord {
  constructor() {
    this.destination = HANDLER_A;
    this.count = 0;
  }
}

class TaskControlBlock {
  constructor(link, identity, priority, initialWorkQueue, packetPending, taskWaiting, privateData, fn) {
    this.link = link;
    this.identity = identity;
    this.priority = priority;
    this.input = initialWorkQueue;
    this.packetPending = packetPending;
    this.taskWaiting = taskWaiting;
    this.taskHolding = false;
    this.handle = privateData;
    this.function = fn;
  }

  isTaskHoldingOrWaiting() {
    return this.taskHolding || (!this.packetPending && this.taskWaiting);
  }

  isWaitingWithPacket() {
    return this.packetPending && this.taskWaiting && !this.taskHolding;
  }

  addInputAndCheckPriority(packet, oldTask) {
    if (NO_WORK === this.input) {
      this.input = packet;
      this.packetPending = true;
      if (this.priority > oldTask.priority) { return this; }
    } else {
      this.input = append(packet, this.input);
    }
    return oldTask;
  }

  runTask() {
    let message;
    if (this.isWaitingWithPacket()) {
      message = this.input;
      this.input = message.link;
      this.packetPending = NO_WORK !== this.input;
      this.taskWaiting = false;
      this.taskHolding = false;
    } else {
      message = NO_WORK;
    }
    return this.function(message, this.handle);
  }
}

class Scheduler {
  constructor() {
    this.queueCount = 0;
    this.holdCount = 0;
    this.taskTable = new Array(NUM_TYPES).fill(NO_TASK);
    this.taskList = NO_TASK;
    this.currentTask = NO_TASK;
    this.currentTaskIdentity = 0;
  }

  createTask(identity, priority, work, packetPending, taskWaiting, data, fn) {
    const t = new TaskControlBlock(this.taskList, identity, priority, work, packetPending, taskWaiting, data, fn);
    this.taskList = t;
    this.taskTable[identity] = t;
  }

  createDevice(identity, priority, work) {
    this.createTask(identity, priority, work, false, true, new DeviceTaskDataRecord(), (packet, data) => {
      if (NO_WORK === packet) {
        packet = data.pending;
        if (NO_WORK === packet) {
          return this.wait();
        }
        data.pending = NO_WORK;
        return this.queuePacket(packet);
      }
      data.pending = packet;
      return this.holdSelf();
    });
  }

  createHandler(identity, priority, work) {
    this.createTask(identity, priority, work, true, true, new HandlerTaskDataRecord(), (packet, data) => {
      if (NO_WORK !== packet) {
        if (WORK_PACKET_KIND === packet.kind) {
          data.workIn = append(packet, data.workIn);
        } else {
          data.deviceIn = append(packet, data.deviceIn);
        }
      }

      const workPacket = data.workIn;
      if (NO_WORK === workPacket) {
        return this.wait();
      }
      const count = workPacket.datum;
      if (count >= DATA_SIZE) {
        data.workIn = workPacket.link;
        return this.queuePacket(workPacket);
      }
      const devicePacket = data.deviceIn;
      if (NO_WORK === devicePacket) {
        return this.wait();
      }
      data.deviceIn = devicePacket.link;
      devicePacket.datum = workPacket.data[count];
      workPacket.datum = count + 1;
      return this.queuePacket(devicePacket);
    });
  }

  createIdler(identity, priority, work) {
    this.createTask(identity, priority, work, false, false, new IdleTaskDataRecord(), (_packet, data) => {
      data.count -= 1;
      if (0 === data.count) {
        return this.holdSelf();
      }
      if (0 === (data.control & 1)) {
        data.control /= 2;
        return this.release(DEVICE_A);
      }
      data.control = (data.control / 2) ^ 53256;
      return this.release(DEVICE_B);
    });
  }

  createWorker(identity, priority, work) {
    this.createTask(identity, priority, work, true, true, new WorkerTaskDataRecord(), (packet, data) => {
      if (NO_WORK === packet) {
        return this.wait();
      }
      data.destination = (HANDLER_A === data.destination) ? HANDLER_B : HANDLER_A;
      packet.identity = data.destination;
      packet.datum = 0;
      for (let i = 0; i < DATA_SIZE; i += 1) {
        data.count += 1;
        if (data.count > 26) { data.count = 1; }
        packet.data[i] = 65 + data.count - 1;
      }
      return this.queuePacket(packet);
    });
  }

  start() {
    let queue;
    this.createIdler(IDLER, 0, NO_WORK);
    queue = new Packet(NO_WORK, WORKER, WORK_PACKET_KIND);
    queue = new Packet(queue, WORKER, WORK_PACKET_KIND);

    this.createWorker(WORKER, 1000, queue);
    queue = new Packet(NO_WORK, DEVICE_A, DEVICE_PACKET_KIND);
    queue = new Packet(queue, DEVICE_A, DEVICE_PACKET_KIND);
    queue = new Packet(queue, DEVICE_A, DEVICE_PACKET_KIND);

    this.createHandler(HANDLER_A, 2000, queue);
    queue = new Packet(NO_WORK, DEVICE_B, DEVICE_PACKET_KIND);
    queue = new Packet(queue, DEVICE_B, DEVICE_PACKET_KIND);
    queue = new Packet(queue, DEVICE_B, DEVICE_PACKET_KIND);

    this.createHandler(HANDLER_B, 3000, queue);
    this.createDevice(DEVICE_A, 4000, NO_WORK);
    this.createDevice(DEVICE_B, 5000, NO_WORK);

    this.schedule();
  }

  holdSelf() {
    this.holdCount += 1;
    const currentTask = this.currentTask;
    currentTask.taskHolding = true;
    return currentTask.link;
  }

  queuePacket(packet) {
    const t = this.taskTable[packet.identity];
    this.queueCount += 1;
    packet.link = NO_WORK;
    packet.identity = this.currentTaskIdentity;
    return t.addInputAndCheckPriority(packet, this.currentTask);
  }

  release(identity) {
    const t = this.taskTable[identity];
    t.taskHolding = false;
    return (t.priority > this.currentTask.priority) ? t : this.currentTask;
  }

  wait() {
    const currentTask = this.currentTask;
    currentTask.taskWaiting = true;
    return currentTask;
  }

  schedule() {
    this.currentTask = this.taskList;
    while (NO_TASK !== this.currentTask) {
      if (this.currentTask.isTaskHoldingOrWaiting()) {
        this.currentTask = this.currentTask.link;
      } else {
        this.currentTaskIdentity = this.currentTask.identity;
        this.currentTask = this.currentTask.runTask();
      }
    }
  }
}

let queued = 0;
let held = 0;
for (let run = 0; run < 25; run += 1) {
  const scheduler = new Scheduler();
  scheduler.start();
  queued += scheduler.queueCount;
  held += scheduler.holdCount;
}
console.log(queued);
console.log(held);
