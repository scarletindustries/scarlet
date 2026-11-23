-- Counts how often each word turns up in a stream of short words, in a table
-- keyed by string, then looks a second stream up in it. It stresses building
-- short strings, hashing them and maps keyed by strings. Written for this suite.

local floor = math.floor
local letters = { "a", "e", "i", "n", "r", "t" }

local function next_seed(seed)
  return seed * 16807 % 2147483647
end

-- A word of 2 to `lengths + 1` letters, all of it decided by `r`.
local function word(r, lengths)
  local n = 2 + r % lengths
  r = floor(r / lengths)
  local w = ""
  for _ = 1, n do
    w = w .. letters[r % 6 + 1]
    r = floor(r / 6)
  end
  return w
end

local counts = {}
local seed = 42
for _ = 1, 1000000 do
  seed = next_seed(seed)
  local w = word(seed, 5)
  counts[w] = (counts[w] or 0) + 1
end

local distinct, squares = 0, 0
for _, n in pairs(counts) do
  distinct = distinct + 1
  squares = squares + n * n
end

-- The second stream has longer words in it too, so some lookups miss.
local check = 0
seed = 2024
for _ = 1, 1000000 do
  seed = next_seed(seed)
  local n = counts[word(seed, 6)] or 0
  check = (check * 31 + n) % 1000000007
end

print(string.format("%d", distinct))
print(string.format("%d", squares))
print(string.format("%d", check))
