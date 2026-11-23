-- Storage builds a tree of nested arrays, four children to a node and seven
-- levels deep, over and over. It stresses allocation and freeing.
-- Ported from Are We Fast Yet (github.com/smarr/are-we-fast-yet), MIT licence.

local seed, count, slots

local function next_random()
    seed = (seed * 1309 + 13849) % 65536
    return seed
end

local function build(depth)
    count = count + 1
    if depth == 1 then
        local n = next_random() % 10 + 1
        slots = slots + n
        local arr = {}
        for i = 1, n do
            arr[i] = false
        end
        return arr
    else
        local arr = {}
        for i = 1, 4 do
            arr[i] = build(depth - 1)
        end
        return arr
    end
end

local total_count, total_slots = 0, 0
for _ = 1, 500 do
    seed, count, slots = 74755, 0, 0
    build(7)
    total_count = total_count + count
    total_slots = total_slots + slots
end
print(total_count)
print(total_slots)
