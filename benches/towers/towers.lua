-- Towers of Hanoi with explicit disk stacks: 13 disks moved between three piles,
-- many times over. It stresses small record updates and linked-list pushes and pops.
-- Ported from the Are We Fast Yet suite (github.com/smarr/are-we-fast-yet, MIT licence).

local piles
local moves_done

local function create_disk(size)
    return {size = size, next = nil}
end

local function push_disk(disk, pile)
    local top = piles[pile]
    if top and disk.size >= top.size then
        error 'Cannot put a big disk on a smaller one'
    end
    disk.next = top
    piles[pile] = disk
end

local function pop_disk_from(pile)
    local top = piles[pile]
    assert(top, 'Attempting to remove a disk from an empty pile')
    piles[pile] = top.next
    top.next = nil
    return top
end

local function move_top_disk(from_pile, to_pile)
    push_disk(pop_disk_from(from_pile), to_pile)
    moves_done = moves_done + 1
end

local function build_tower_at(pile, disks)
    for i = disks, 0, -1 do
        push_disk(create_disk(i), pile)
    end
end

local function move_disks(disks, from_pile, to_pile)
    if disks == 1 then
        move_top_disk(from_pile, to_pile)
    else
        local other_pile = 6 - from_pile - to_pile
        move_disks(disks - 1, from_pile, other_pile)
        move_top_disk(from_pile, to_pile)
        move_disks(disks - 1, other_pile, to_pile)
    end
end

local function weigh(disk)
    local depth, acc = 1, 0
    while disk do
        acc = acc + disk.size * depth
        depth = depth + 1
        disk = disk.next
    end
    return acc
end

local function benchmark()
    piles = {}
    build_tower_at(1, 13)
    moves_done = 0
    move_disks(13, 1, 2)
end

local moves, weight = 0, 0
for _ = 1, 600 do
    benchmark()
    moves = moves + moves_done
    weight = weight + weigh(piles[2]) + weigh(piles[1])
end
print(moves)
print(weight)
