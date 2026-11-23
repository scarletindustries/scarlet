-- Stresses the built-in map (a Lua table): insert scattered int keys, look
-- them all up several times (with a miss probe beside each hit), then delete
-- the oldest key and insert a new one in a long churn loop.
-- Written for this suite, starting from examples/bench_map.sca.

-- A full-period generator, so the keys never repeat. Its products stay far
-- below 2^53, which keeps the doubles exact.
local function next_key(x)
  return (x * 69069 + 1) % 4294967296
end

local function lookups(m, x, n, acc)
  for _ = 1, n do
    x = next_key(x)
    local v = m[x]
    if v ~= nil then acc = (acc * 31 + v) % 1000000007 end
    v = m[x + 1]
    if v ~= nil then acc = acc + v end
  end
  return acc
end

local n = 200000
local seed = 42

local m = {}
local lead = seed
for i = 0, n - 1 do
  lead = next_key(lead)
  m[lead] = i
end

local acc = 0
for _ = 1, 10 do
  acc = lookups(m, seed, n, acc)
end
print(acc)

-- The map is a sliding window over the key sequence: `trail` names the oldest
-- key, which goes, and `lead` the next new one, which comes in.
local trail = seed
for i = 0, 3000000 - 1 do
  trail = next_key(trail)
  lead = next_key(lead)
  m[trail] = nil
  m[lead] = i
end
-- The window's keys must all be there and the first `n`, long deleted, must
-- all be gone: a hit in the second pass would change the sum.
local window = lookups(m, trail, n, 0)
print(lookups(m, seed, n, window))
